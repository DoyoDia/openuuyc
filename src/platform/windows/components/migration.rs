//! Temporary convergence of historical installations. Remove this module and its
//! explicit installation/GUI/management hooks at the next major-version boundary.
//! No credential conversion, arbitrary-path cleanup, or version-number guessing.
use super::{application, files};
use crate::platform::windows::host_service::{self, process, vault};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
const STATE: &str = "migration-v1.json";
const LIMIT: usize = 256;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Report {
    pub complete: bool,
    pub pending: Vec<String>,
    pub preserved: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Record {
    schema: u32,
    target: String,
    owner: String,
    user_ready: bool,
    report: Report,
}
fn path() -> Result<PathBuf> {
    Ok(application::directory()?.join(STATE))
}
fn load() -> Result<Option<Record>> {
    let path = path()?;
    files::reject_reparse(&path)?;
    let file = match std::fs::File::open(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(file.metadata()?.len() < 65536, "迁移记录过大");
    let record: Record = serde_json::from_reader(file)?;
    ensure!(
        record.schema == 1
            && record.target.len() == 64
            && record.target.bytes().all(|c| c.is_ascii_hexdigit()),
        "迁移记录格式无效"
    );
    Ok(Some(record))
}
fn save(record: &Record) -> Result<()> {
    let path = path()?;
    let temp = path.with_extension("partial");
    files::reject_reparse(&path)?;
    files::reject_reparse(&temp)?;
    if temp.exists() {
        std::fs::remove_file(&temp)?;
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(record)?)?;
    file.sync_all()?;
    drop(file);
    use windows::{Win32::Storage::FileSystem::*, core::PCWSTR};
    let wide = |p: &Path| {
        p.to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    unsafe {
        MoveFileExW(
            PCWSTR(wide(&temp).as_ptr()),
            PCWSTR(wide(&path).as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )?;
    }
    Ok(())
}
/// Called with the component mutation lock, after the new image and SCM
/// registration have committed. Backups survive until the new executor answers.
pub(crate) fn begin() -> Result<()> {
    application::verify_directory(&application::directory()?)?;
    save(&Record {
        schema: 1,
        target: process::image_hash(&application::directory()?.join("OpenUUYC.exe"))?,
        owner: vault::owner()?.context("安装所有者缺失")?,
        user_ready: false,
        report: Report::default(),
    })
}
pub(crate) fn legacy_directory() -> Result<PathBuf> {
    Ok(application::directory()?
        .parent()
        .context("安装目录无效")?
        .join("OpenUUYCInputService"))
}
pub(crate) fn active_directory(current: &Path) -> Result<PathBuf> {
    let old = legacy_directory()?;
    Ok(
        if current.join("owner.txt").exists() || !old.join("owner.txt").exists() {
            current.into()
        } else {
            old
        },
    )
}
pub(crate) fn image_in(dir: &Path) -> Result<PathBuf> {
    Ok(dir.join(if dir == legacy_directory()? {
        "OpenUUYCHost.exe"
    } else {
        "OpenUUYC.exe"
    }))
}
pub(crate) fn known_directory(dir: &Path) -> Result<bool> {
    Ok(dir == application::directory()? || dir == legacy_directory()?)
}
pub(crate) fn installed_target(target: &Path) -> Result<bool> {
    Ok(target == application::directory()?.join("OpenUUYC.exe")
        || target == legacy_directory()?.join("OpenUUYCHost.exe"))
}
pub(crate) fn legacy_service_command(command: &str) -> Result<bool> {
    let old = legacy_directory()?;
    if !command.eq_ignore_ascii_case(&format!(
        "\"{}\" service",
        old.join("OpenUUYCHost.exe").display()
    )) {
        return Ok(false);
    }
    ensure!(
        matches_record(&old.join("owner.txt"))?,
        "既有组件归属不匹配"
    );
    Ok(true)
}
/// Old installations could register a portable GUI separately from the host
/// executable. Its recorded content identity, not its filename, authorizes exit.
pub(crate) fn registered_client(candidate: &Path) -> Result<bool> {
    let directory = host_service::install::directory()?;
    application::verify_directory(&directory)?;
    let path = directory.join("images.json");
    files::reject_reparse(&path)?;
    ensure!(std::fs::metadata(&path)?.len() < 4096, "旧程序身份记录过大");
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let expected = value
        .get("client")
        .and_then(|v| v.as_str())
        .context("旧主程序身份缺失")?;
    ensure!(
        expected.len() == 64 && expected.bytes().all(|v| v.is_ascii_hexdigit()),
        "旧主程序身份无效"
    );
    Ok(process::image_hash(candidate)? == expected)
}
pub(crate) fn legacy_center(hwnd: windows::Win32::Foundation::HWND) -> bool {
    let mut title = [0u16; 128];
    let length =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(hwnd, &mut title) };
    matches!(
        String::from_utf16_lossy(&title[..length.max(0) as usize]).as_str(),
        "OpenUUYC" | "OpenUUYC · 控制中心"
    )
}
fn matches_record(file: &Path) -> Result<bool> {
    files::reject_reparse(file)?;
    if !file.is_file() {
        return Ok(false);
    }
    ensure!(std::fs::metadata(file)?.len() < 1024, "安装归属记录无效");
    Ok(std::fs::read_to_string(file)? == application::RECEIPT)
}
fn pin_directory(dir: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .custom_flags(0x02000000 | 0x00200000)
        .open(dir)?;
    ensure!(
        file.metadata()?.is_dir() && file.metadata()?.file_attributes() & 0x400 == 0,
        "旧目录不可为重解析点"
    );
    Ok(file)
}
fn backup(name: &str) -> bool {
    ["retired-", "staged-"].into_iter().any(|p| {
        name.strip_prefix(p)
            .and_then(|n| n.strip_suffix(".exe"))
            .is_some_and(|n| n.len() == 32 && n.bytes().all(|b| b.is_ascii_hexdigit()))
    })
}
fn remove_file(path: &Path, report: &mut Report) -> bool {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(e) => {
            report.pending.push(format!("{}：{e}", path.display()));
            return false;
        }
        Ok(m) if !m.is_file() || m.file_attributes() & 0x400 != 0 => {
            report.preserved.push(path.display().to_string());
            return false;
        }
        Ok(_) => (),
    }
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) => {
            report.pending.push(format!("{}：{e}", path.display()));
            false
        }
    }
}
fn inventory(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let items = std::fs::read_dir(dir)?
        .take(LIMIT + 1)
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(items.len() <= LIMIT, "旧安装目录条目过多，已保留");
    Ok(items)
}
fn clean(current: &Path, legacy: &Path, owner: &str) -> Result<Report> {
    let mut report = Report::default();
    let _current = pin_directory(current)?;
    match live_display(&current.join("display-agent.json")) {
        Ok(Some(_)) => report.pending.push("旧显示维护进程仍在运行".into()),
        Ok(None) => {
            remove_file(&current.join("display-agent.json"), &mut report);
        }
        Err(error) => report.preserved.push(format!("旧显示维护记录：{error:#}")),
    }
    remove_file(&current.join("OpenUUYCHost.exe"), &mut report);
    for item in inventory(current)? {
        if backup(&item.file_name().to_string_lossy()) {
            remove_file(&item.path(), &mut report);
        }
    }
    let legacy_result = (|| -> Result<()> {
        if std::fs::symlink_metadata(legacy).is_ok() {
            let guard = pin_directory(legacy)?;
            if !matches_record(&legacy.join("owner.txt"))? {
                report
                    .preserved
                    .push(format!("{}：无法确认归属", legacy.display()));
            } else {
                let old_suite = legacy.join("suite.json");
                let same_owner = if old_suite.exists() {
                    files::reject_reparse(&old_suite)?;
                    ensure!(
                        std::fs::metadata(&old_suite)?.len() < 4096,
                        "旧安装记录过大"
                    );
                    serde_json::from_slice::<serde_json::Value>(&std::fs::read(&old_suite)?)?
                        .get("owner")
                        .and_then(|v| v.as_str())
                        == Some(owner)
                } else {
                    true
                };
                if !same_owner {
                    report
                        .preserved
                        .push(format!("{}：属于另一Windows用户", legacy.display()));
                } else if live_display(&legacy.join("display-agent.json"))?.is_some() {
                    report.pending.push("旧目录的显示维护进程仍在运行".into());
                } else {
                    let executable_removed =
                        remove_file(&legacy.join("OpenUUYCHost.exe"), &mut report)
                            & remove_file(&legacy.join("OpenUUYC.exe"), &mut report);
                    if executable_removed {
                        for name in [
                            "images.json",
                            "resident.pid",
                            "display-agent.json",
                            "suite.json",
                        ] {
                            remove_file(&legacy.join(name), &mut report);
                        }
                        let policy = legacy.join("sas-policy.json");
                        if policy.exists() {
                            files::reject_reparse(&policy)?;
                            ensure!(
                                std::fs::metadata(&policy)?.len() < 65536,
                                "旧策略恢复记录过大"
                            );
                            if std::fs::read(current.join("sas-policy.json"))
                                .ok()
                                .as_deref()
                                == Some(std::fs::read(&policy)?.as_slice())
                            {
                                remove_file(&policy, &mut report);
                            } else {
                                report.preserved.push(format!(
                                    "{}：保留未合并的策略恢复记录",
                                    policy.display()
                                ));
                            }
                        }
                        for item in inventory(legacy)? {
                            let name = item.file_name();
                            let name = name.to_string_lossy();
                            if backup(&name) {
                                remove_file(&item.path(), &mut report);
                            } else if name != "owner.txt"
                                && !report
                                    .pending
                                    .iter()
                                    .any(|v| v.starts_with(&item.path().display().to_string()))
                                && !report
                                    .preserved
                                    .iter()
                                    .any(|v| v.starts_with(&item.path().display().to_string()))
                            {
                                report.preserved.push(item.path().display().to_string());
                            }
                        }
                        if inventory(legacy)?
                            .iter()
                            .all(|e| e.file_name() == "owner.txt")
                        {
                            remove_file(&legacy.join("owner.txt"), &mut report);
                            drop(guard);
                            if let Err(e) = std::fs::remove_dir(legacy) {
                                report.pending.push(format!("{}：{e}", legacy.display()));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = legacy_result {
        report
            .preserved
            .push(format!("{}：{error:#}", legacy.display()));
    }
    report.complete = report.pending.is_empty();
    Ok(report)
}
/// Invoked only by the new service after its verified executor answered
/// DeploymentReady and the installing user finished updating all user entries.
pub(crate) fn finish_machine(target: &str) -> Result<Report> {
    let _serial = super::Serial::acquire()?;
    let current = application::directory()?;
    application::verify_directory(&current)?;
    ensure!(
        dunce::canonicalize(std::env::current_exe()?)?
            == dunce::canonicalize(current.join("OpenUUYC.exe"))?,
        "仅当前安装服务可以清理旧安装"
    );
    let mut record = load()?.context("迁移记录缺失")?;
    ensure!(
        record.target == target
            && process::image_hash(&current.join("OpenUUYC.exe"))? == target
            && vault::owner()?.as_deref() == Some(record.owner.as_str()),
        "迁移目标已改变"
    );
    if record.report.complete {
        return Ok(record.report);
    }
    record.user_ready = true;
    save(&record)?;
    record.report = clean(&current, &legacy_directory()?, &record.owner)?;
    save(&record)?;
    tracing::info!(complete=record.report.complete,pending=?record.report.pending,preserved=?record.report.preserved,"installation layout migration");
    Ok(record.report)
}
pub(crate) fn finish_user() -> Result<bool> {
    struct UserSerial(host_service::pipe::Handle);
    impl Drop for UserSerial {
        fn drop(&mut self) {
            unsafe {
                let _ = windows::Win32::System::Threading::ReleaseMutex(self.0.0);
            }
        }
    }
    use windows::{
        Win32::{Foundation::*, System::Threading::*},
        core::w,
    };
    let mutex = host_service::pipe::Handle(unsafe {
        CreateMutexW(
            None,
            false,
            w!("Local\\OpenUUYC.InstallationMigration.User.v1"),
        )?
    });
    let result = unsafe { WaitForSingleObject(mutex.0, 0) };
    ensure!(
        result == WAIT_OBJECT_0 || result == WAIT_ABANDONED,
        "安装入口迁移正在进行"
    );
    let _serial = UserSerial(mutex);
    let Some(record) = load()? else {
        return Ok(true);
    };
    if record.report.complete {
        return Ok(true);
    }
    ensure!(
        vault::sid(std::process::id())? == record.owner,
        "迁移用户不匹配"
    );
    ensure!(
        process::image_hash(&std::env::current_exe()?)? == record.target,
        "请由新版本完成安装迁移"
    );
    if !record.user_ready {
        host_service::startup::set(true)?;
        application::integrate_user()?;
        crate::platform::windows::notifications::retarget_installed()?;
    }
    match host_service::resident::call(host_service::resident::Request::FinishMigration {
        target: record.target,
    })?
    .checked()?
    {
        host_service::resident::Reply::Migration(report) => Ok(report.complete),
        _ => anyhow::bail!("迁移服务未返回清理结果"),
    }
}
pub(crate) struct UserRecovery {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl UserRecovery {
    pub(crate) fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        let thread = std::thread::Builder::new()
            .name("installation-migration".into())
            .spawn(move || {
                if !load().ok().flatten().is_some_and(|r| {
                    !r.report.complete
                        && vault::sid(std::process::id()).ok().as_deref() == Some(&r.owner)
                }) {
                    return;
                }
                while !cancelled.load(Ordering::Acquire) {
                    match finish_user() {
                        Ok(true) => break,
                        Ok(false) => (),
                        Err(error) => tracing::warn!(%error,"installation migration deferred"),
                    }
                    for _ in 0..300 {
                        if cancelled.load(Ordering::Acquire) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .ok();
        Self { stop, thread }
    }
}
impl Drop for UserRecovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Deserialize)]
struct DisplayIdentity {
    pid: u32,
    parent: u32,
    birth: u64,
}
fn live_display(path: &Path) -> Result<Option<DisplayIdentity>> {
    files::reject_reparse(path)?;
    let bytes = match std::fs::read(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(bytes.len() < 1024, "显示维护记录过大");
    let value: DisplayIdentity = serde_json::from_slice(&bytes)?;
    Ok(crate::platform::display::recovery::owner_alive(value.pid, value.birth)?.then_some(value))
}
pub(crate) fn legacy_display_pid(parent: u32) -> Result<Option<u32>> {
    Ok(
        live_display(&host_service::install::directory()?.join("display-agent.json"))?
            .filter(|v| v.parent == parent)
            .map(|v| v.pid),
    )
}
