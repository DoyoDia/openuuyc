//! User-selected UU invite activation. Preserve a prior HKCU registration and
//! only retarget/remove a registration whose command still belongs to us.
use super::registry::{Key, key, read, set, wide};
use anyhow::{Context, Result, ensure};
use std::path::Path;
use windows::{
    Win32::{Foundation::*, System::Registry::*, UI::Shell::*},
    core::PCWSTR,
};

const SCHEME: &str = r"Software\Classes\uuremote";
const BACKUP: &str = r"Software\OpenUUYC\AssistLinkBackup.v1";
const OWNER: &str = "OpenUUYC.AssistLink.v1";

fn command(image: &Path) -> String {
    format!("\"{}\" \"%1\"", image.display())
}
fn stored_command() -> Option<String> {
    read(&format!(r"{SCHEME}\shell\open\command"), "")
        .ok()
        .flatten()
}
fn open_read(path: &str) -> Result<Option<Key>> {
    let mut handle = HKEY::default();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(path).as_ptr()),
            None,
            KEY_READ,
            &mut handle,
        )
    };
    if result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    result.ok()?;
    Ok(Some(Key(handle)))
}
fn delete_tree(path: &str) -> Result<()> {
    let result = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide(path).as_ptr())) };
    if result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
        return Ok(());
    }
    result
        .ok()
        .with_context(|| format!("删除协议登记失败：{path}"))?;
    Ok(())
}
fn copy_tree(from: &str, to: &str) -> Result<bool> {
    let Some(source) = open_read(from)? else {
        return Ok(false);
    };
    // RegCopyTree also copies security descriptors; ordinary KEY_WRITE is
    // insufficient for this destination handle. Do not broaden shared helpers.
    let mut handle = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(to).as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_ALL_ACCESS,
            None,
            &mut handle,
            None,
        )
        .ok()
        .context("创建协议备份/恢复目标失败")?;
    }
    let target = Key(handle);
    unsafe {
        RegCopyTreeW(source.0, None, target.0)
            .ok()
            .context("复制协助链接登记失败")?;
    }
    Ok(true)
}
fn changed() {
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST | SHCNF_FLUSH, None, None);
    }
}

pub(crate) fn owned() -> bool {
    read(SCHEME, "OpenUUYCOwner").ok().flatten().as_deref() == Some(OWNER)
        && read(&format!(r"{SCHEME}\shell\open\command"), "DelegateExecute")
            .is_ok_and(|v| v.is_none())
        && stored_command()
            .zip(read(SCHEME, "OpenUUYCCommand").ok().flatten())
            .is_some_and(|(actual, ours)| actual.eq_ignore_ascii_case(&ours))
}
fn backup_previous() -> Result<()> {
    if open_read(BACKUP)?.is_some() {
        ensure!(
            read(BACKUP, "Owner")?.as_deref() == Some(OWNER),
            "协助链接备份不属于本程序"
        );
        delete_tree(BACKUP)?;
    }
    let backup = key(BACKUP)?;
    set(backup.0, "Owner", OWNER)?;
    let previous = copy_tree(SCHEME, &format!(r"{BACKUP}\Previous"))?;
    set(backup.0, "HadPrevious", if previous { "1" } else { "0" })
}
fn restore_previous() -> Result<()> {
    ensure!(
        read(BACKUP, "Owner")?.as_deref() == Some(OWNER),
        "协助链接原登记备份不可用，未删除当前登记"
    );
    let previous = read(BACKUP, "HadPrevious")?.context("协助链接备份不完整")?;
    ensure!(
        matches!(previous.as_str(), "0" | "1"),
        "协助链接备份状态无效"
    );
    if previous == "1" {
        ensure!(
            open_read(&format!(r"{BACKUP}\Previous"))?.is_some(),
            "原协议登记备份缺失"
        );
    }
    delete_tree(SCHEME)?;
    if previous == "1" {
        ensure!(
            copy_tree(&format!(r"{BACKUP}\Previous"), SCHEME)?,
            "恢复原协议登记失败"
        );
    }
    delete_tree(BACKUP)?;
    changed();
    Ok(())
}
fn write_registration(image: &Path) -> Result<()> {
    let wanted = command(image);
    let root = key(SCHEME)?;
    set(root.0, "", "URL:OpenUUYC remote assistance")?;
    set(root.0, "URL Protocol", "")?;
    let open = key(&format!(r"{SCHEME}\shell\open\command"))?;
    set(open.0, "", &wanted)?;
    set(root.0, "OpenUUYCCommand", &wanted)?;
    set(root.0, "OpenUUYCOwner", OWNER)?;
    changed();
    Ok(())
}
fn enable_image(image: &Path) -> Result<()> {
    let was_owned = owned();
    let previous_command = stored_command();
    if !was_owned {
        backup_previous()?;
        // A previous handler may use DelegateExecute or other verbs. The
        // whole original tree is preserved above before replacing it.
        delete_tree(SCHEME)?;
    }
    if let Err(error) = write_registration(image) {
        if !was_owned {
            let current = stored_command();
            if current.is_none()
                || current == previous_command
                || current.as_deref() == Some(command(image).as_str())
            {
                restore_previous().context("协议登记失败且恢复原登记失败")?;
            }
        }
        return Err(error);
    }
    Ok(())
}
pub(crate) fn enable() -> Result<()> {
    // This option is applied after successful installation, to the stable image.
    let image = dunce::canonicalize(super::components::application::image()?)?;
    enable_image(&image)
}
pub(crate) fn disable() -> Result<()> {
    if !owned() {
        return Ok(());
    }
    restore_previous()
}
fn retarget_image(image: &Path) -> Result<()> {
    if !owned() || stored_command().as_deref() == Some(command(image).as_str()) {
        return Ok(());
    }
    write_registration(image)
}
pub(crate) fn retarget() -> Result<()> {
    if !owned() {
        return Ok(());
    }
    let image = dunce::canonicalize(super::components::application::image()?)?;
    retarget_image(&image)
}
pub(crate) fn retarget_on_start() {
    if let Err(error) = retarget() {
        tracing::warn!(error = %format!("{error:#}"), "校正协助链接的注册失败");
    }
}
