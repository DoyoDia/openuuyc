//! Explicit full-uninstall data removal, scoped to this product and current user.
use super::*;

fn remove_tree(path: &Path) -> Result<()> {
    use windows::{
        Win32::{Foundation::*, Storage::FileSystem::*},
        core::PCWSTR,
    };
    let text = std::path::absolute(path)?.to_string_lossy().into_owned();
    let text = if text.starts_with(r"\\?\") {
        text
    } else if let Some(unc) = text.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{unc}")
    } else {
        format!(r"\\?\{text}")
    };
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    // Deny DELETE sharing while enumerating. A user-writable directory cannot
    // be swapped for a junction under an elevated cleanup worker.
    let handle = match unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    } {
        Ok(handle) => crate::platform::windows::host_service::pipe::Handle(handle),
        Err(error)
            if error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                || error.code() == ERROR_PATH_NOT_FOUND.to_hresult() =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(handle.0, &mut info)?;
    }
    let directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    let link = info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
    if directory && !link {
        for entry in std::fs::read_dir(path)? {
            remove_tree(&entry?.path())?;
        }
    }
    drop(handle);
    // Removing a link unlinks only that name, never its target.
    if directory {
        std::fs::remove_dir(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}
fn ensure_recovered(root: &Path) -> Result<()> {
    let directory = root.join("displays/recovery");
    if directory.exists() {
        reject_reparse(&directory)?;
        ensure!(
            std::fs::read_dir(directory)?.next().is_none(),
            "屏幕布局仍有待恢复记录，请先完成恢复再清除数据"
        );
    }
    Ok(())
}
pub(super) fn machine() -> Result<()> {
    let root = crate::platform::windows::host_service::vault::root()?;
    let directory = root.parent().context("本机数据目录无效")?;
    reject_reparse(directory)?;
    reject_reparse(&root)?;
    ensure_recovered(&root)?;
    crate::platform::windows::virtual_audio::Defaults::ensure_recovered()?;
    remove_tree(directory)
}
pub(super) fn user() -> Result<()> {
    let directory = folder(&FOLDERID_LocalAppData)?.join("OpenUUYC");
    reject_reparse(&directory)?;
    ensure_recovered(&directory)?;
    // Use the native keyring's metadata search; never fetch credential secrets.
    // Match only namespaces actually owned by this product, not arbitrary names
    // containing its branding or other applications' credential entries.
    keyring::Entry::store_status()
        .as_ref()
        .map_err(|_| anyhow::anyhow!("系统凭据库不可用"))?;
    let entries = keyring_core::Entry::search(&std::collections::HashMap::from([(
        "pattern",
        r"\.com\.openuuyc\.(session|wallpaper|(assist|viewing|audio|microphone|host|device-preferences)\.[0-9a-f]{64})$",
    )]))?;
    for entry in entries {
        entry.delete_credential()?;
    }
    remove_tree(&directory)
}
