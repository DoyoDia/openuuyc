//! Installed application ownership. Services consume this image; they do not own it.
use super::files::{reject_reparse, secure_directory};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use windows::{
    Win32::{System::Com::CoTaskMemFree, UI::Shell::*},
    core::GUID,
};

mod data;
pub(super) mod integration;
pub(crate) fn purge_machine_data() -> Result<()> {
    data::machine()
}
pub(crate) const RECEIPT: &str = "OpenUUYC Input Service deployment v1\n";

pub(super) fn folder(id: &GUID) -> Result<PathBuf> {
    unsafe {
        let raw = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)?;
        let text = raw.to_string();
        CoTaskMemFree(Some(raw.0.cast()));
        Ok(PathBuf::from(text?))
    }
}
pub(crate) fn directory() -> Result<PathBuf> {
    Ok(folder(&FOLDERID_ProgramFiles)?.join("OpenUUYC"))
}
pub(crate) fn legacy_directory() -> Result<PathBuf> {
    super::migration::legacy_directory()
}
pub(crate) fn active_directory() -> Result<PathBuf> {
    super::migration::active_directory(&directory()?)
}
pub(crate) fn image() -> Result<PathBuf> {
    image_in(&active_directory()?)
}
pub(crate) fn image_in(directory: &Path) -> Result<PathBuf> {
    super::migration::image_in(directory)
}
pub(crate) fn verify_directory(directory: &Path) -> Result<()> {
    ensure!(
        super::migration::known_directory(directory)?,
        "安装目录无效"
    );
    reject_reparse(directory.parent().context("安装目录无效")?)?;
    reject_reparse(directory)?;
    ensure!(
        std::fs::read_to_string(directory.join("owner.txt"))? == RECEIPT,
        "安装目录归属不匹配"
    );
    Ok(())
}

/// Keep the prior image and metadata until SCM configuration has succeeded.
pub(crate) struct Deployment {
    directory: PathBuf,
    backup: Option<PathBuf>,
    previous_images: Option<Vec<u8>>,
    created: bool,
    committed: bool,
    image_written: bool,
}
struct StagedImage(PathBuf);
impl Drop for StagedImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
impl Deployment {
    pub(crate) fn prepare() -> Result<Self> {
        let directory = directory()?;
        let old = active_directory()?;
        let created = !directory.join("owner.txt").exists();
        reject_reparse(directory.parent().context("安装目录无效")?)?;
        reject_reparse(&directory)?;
        if created {
            ensure!(
                !directory.exists() || std::fs::read_dir(&directory)?.next().is_none(),
                "安装目录已有未登记文件"
            );
        } else {
            verify_directory(&directory)?;
        }
        if old != directory {
            verify_directory(&old)?;
        }
        std::fs::create_dir_all(&directory)?;
        secure_directory(&directory)?;
        let mut transaction = Self {
            previous_images: std::fs::read(directory.join("images.json")).ok(),
            directory,
            backup: None,
            created,
            committed: false,
            image_written: false,
        };
        if created {
            for name in ["suite.json", "sas-policy.json"] {
                let source = old.join(name);
                if old != transaction.directory && source.exists() {
                    reject_reparse(&source)?;
                    ensure!(std::fs::metadata(&source)?.len() < 4096, "安装记录无效");
                    std::fs::copy(source, transaction.directory.join(name))?;
                }
            }
            std::fs::write(transaction.directory.join("owner.txt"), RECEIPT)?;
        }
        let image = transaction.directory.join("OpenUUYC.exe");
        reject_reparse(&image)?;
        let staged = transaction
            .directory
            .join(format!("staged-{}.exe", uuid::Uuid::new_v4().simple()));
        let _staged_cleanup = StagedImage(staged.clone());
        std::fs::copy(std::env::current_exe()?, &staged)?;
        if crate::platform::windows::host_service::process::image_hash(&staged)?
            != crate::platform::windows::host_service::process::image_hash(
                &std::env::current_exe()?
            )?
        {
            let _ = std::fs::remove_file(&staged);
            anyhow::bail!("复制后的程序校验失败");
        }
        if image.exists() {
            let backup = transaction
                .directory
                .join(format!("retired-{}.exe", uuid::Uuid::new_v4().simple()));
            if let Err(error) = std::fs::rename(&image, &backup) {
                let _ = std::fs::remove_file(staged);
                return Err(error.into());
            }
            transaction.backup = Some(backup);
        }
        if let Err(error) = std::fs::rename(&staged, &image) {
            let _ = std::fs::remove_file(staged);
            return Err(error.into());
        }
        transaction.image_written = true;
        Ok(transaction)
    }
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for Deployment {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(backup) = &self.backup {
            let _ = std::fs::remove_file(self.directory.join("OpenUUYC.exe"));
            let _ = std::fs::rename(backup, self.directory.join("OpenUUYC.exe"));
        } else if self.image_written {
            let _ = std::fs::remove_file(self.directory.join("OpenUUYC.exe"));
        }
        if let Some(bytes) = &self.previous_images {
            let _ = std::fs::write(self.directory.join("images.json"), bytes);
        } else {
            let _ = std::fs::remove_file(self.directory.join("images.json"));
        }
        if self.created {
            for name in ["owner.txt", "suite.json", "sas-policy.json", "images.json"] {
                let _ = std::fs::remove_file(self.directory.join(name));
            }
            let _ = std::fs::remove_dir(&self.directory);
        }
    }
}
pub(crate) fn cleanup_retired(directory: &Path) -> Result<()> {
    verify_directory(directory)?;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix("retired-")
            .and_then(|s| s.strip_suffix(".exe"))
            .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            reject_reparse(&entry.path())?;
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

pub(crate) fn needs_handoff() -> Result<bool> {
    let image = image()?;
    if !image.is_file() {
        return Ok(false);
    }
    verify_directory(image.parent().context("安装路径无效")?)?;
    Ok(std::fs::canonicalize(std::env::current_exe()?)? != std::fs::canonicalize(image)?)
}
pub(crate) fn installed_version() -> Result<String> {
    use std::{
        io::Read,
        os::windows::process::CommandExt,
        process::Stdio,
        time::{Duration, Instant},
    };
    let image = image()?;
    verify_directory(image.parent().context("安装路径无效")?)?;
    // --version exits in clap, before logging, account initialization or GUI.
    let mut child = std::process::Command::new(image)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("读取已安装版本超时");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    ensure!(status.success(), "读取已安装版本失败");
    let mut output = String::new();
    child
        .stdout
        .take()
        .context("版本输出缺失")?
        .take(256)
        .read_to_string(&mut output)?;
    let version = output
        .trim()
        .strip_prefix("OpenUUYC ")
        .context("已安装版本信息无效")?;
    ensure!(
        !version.is_empty() && version.len() <= 80 && !version.chars().any(char::is_control),
        "已安装版本信息无效"
    );
    Ok(version.to_owned())
}
pub(crate) fn start_installed(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let image = image()?;
    verify_directory(image.parent().context("安装路径无效")?)?;
    ensure!(image.is_file(), "安装程序缺失，请修复安装");
    let child = std::process::Command::new(&image)
        .args(arguments)
        .current_dir(image.parent().unwrap())
        .creation_flags(0x08000000)
        .spawn()
        .context("启动已安装程序失败")?;
    // Hand off foreground permission while the launching UI is still alive.
    // Only this child receives it; services and unrelated processes do not.
    if let Err(error) =
        unsafe { windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(child.id()) }
    {
        tracing::debug!(%error, "foreground launch permission unavailable");
    }
    Ok(())
}
pub(crate) fn integrate_user() -> Result<()> {
    integration::shortcuts(true)
}
pub(crate) fn register() -> Result<()> {
    integration::register(true)
}

pub(crate) fn request_uninstall(removal: super::RemovalOptions) -> Result<bool> {
    use crate::platform::windows::host_service::{startup, vault};
    if let Some(owner) = vault::owner()? {
        ensure!(
            owner == vault::sid(std::process::id())?,
            "请由安装此程序的Windows用户卸载"
        );
        if !removal.remove_data {
            crate::account::auth::restore_portable()?;
        }
    }
    let notification_image = image()?;
    let args = format!("component application uninstall{}", removal.arguments());
    let reboot = super::elevate(&args, super::Kind::Application)?;
    startup::set(false)?;
    integration::shortcuts(false)?;
    crate::platform::windows::notifications::unregister(&notification_image)?;
    if removal.remove_data && !reboot {
        data::user()?;
    }
    Ok(reboot)
}
pub(crate) fn uninstall(removal: super::RemovalOptions) -> Result<bool> {
    use crate::platform::windows::{host_service, input};
    let directory = active_directory()?;
    removal.preflight()?;
    crate::platform::windows::virtual_audio::Defaults::recover_defaults()
        .context("卸载前恢复默认音频设备失败")?;
    if !directory.exists() {
        // Allow a failed data-cleanup step to be retried after the application
        // image has already been removed. All component removals remain scoped.
        ensure!(
            !host_service::install::status()?.installed,
            "服务安装目录缺失，请先修复安装"
        );
        let mut reboot = removal.remove_optional_drivers()?;
        reboot |= input::install::uninstall()?;
        integration::register(false)?;
        return Ok(reboot);
    }
    verify_directory(&directory)?;
    let reboot = if directory.join("suite.json").is_file() {
        super::suite::execute(super::Operation::Uninstall, None, false, removal)?
    } else {
        host_service::install::preflight()?;
        host_service::install::stop_for_maintenance()?;
        let mut reboot = removal.remove_optional_drivers()?;
        reboot |= input::install::uninstall()?;
        reboot | host_service::install::uninstall()?
    };
    ensure!(!reboot, "组件移除需要重启Windows；重启后可继续卸载程序");
    remove_directory(&directory)?;
    let legacy = legacy_directory()?;
    if legacy != directory && legacy.join("owner.txt").is_file() {
        remove_directory(&legacy)?;
    }
    integration::register(false)?;
    Ok(false)
}
fn remove_directory(directory: &Path) -> Result<()> {
    verify_directory(directory)?;
    cleanup_retired(directory)?;
    for name in [
        "OpenUUYC.exe",
        "OpenUUYCHost.exe",
        "images.json",
        "resident.pid",
        "suite.json",
        "sas-policy.json",
        "migration-v1.json",
        "migration-v1.partial",
    ] {
        let file = directory.join(name);
        reject_reparse(&file)?;
        if file.exists() {
            std::fs::remove_file(&file)
                .with_context(|| format!("无法移除{}，请先退出运行中的程序", file.display()))?;
        }
    }
    std::fs::remove_file(directory.join("owner.txt"))?;
    if std::fs::read_dir(directory)?.next().is_none() {
        std::fs::remove_dir(directory)?;
    }
    Ok(())
}
