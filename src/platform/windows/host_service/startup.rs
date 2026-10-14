//! Installing user's ordinary-permission tray startup; no Windows password.
use anyhow::{Result, ensure};
use windows::{
    Win32::{Foundation::ERROR_FILE_NOT_FOUND, System::Registry::*},
    core::{PCWSTR, w},
};
/// Whether this user's login starts OpenUUYC.
pub(crate) fn enabled() -> Result<bool> {
    let mut size = 0u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            w!("OpenUUYC"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    result.ok()?;
    Ok(true)
}
/// The settings switch. Points at the installed program while the components
/// are installed, otherwise at this copy, as enabling remote wake does.
pub(crate) fn configure(enabled: bool) -> Result<()> {
    if !enabled {
        return set_image(false, std::path::Path::new(""));
    }
    let image = if super::resident::managed() {
        crate::platform::windows::components::application::image()?
    } else {
        std::env::current_exe()?
    };
    set_image(true, &image)
}
pub(crate) fn set(enabled: bool) -> Result<()> {
    set_image(
        enabled,
        &crate::platform::windows::components::application::image()?,
    )
}
pub(crate) fn set_image(enabled: bool, path: &std::path::Path) -> Result<()> {
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let result = (|| -> Result<()> {
        if enabled {
            ensure!(path.is_file(), "尚未部署自启动程序");
            let command: Vec<u16> = format!("\"{}\" gui --background", path.display())
                .encode_utf16()
                .chain(Some(0))
                .collect();
            unsafe {
                RegSetValueExW(
                    key,
                    w!("OpenUUYC"),
                    None,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        command.as_ptr().cast(),
                        command.len() * 2,
                    )),
                )
                .ok()?;
            }
        } else {
            let result = unsafe { RegDeleteValueW(key, PCWSTR(w!("OpenUUYC").as_ptr())) };
            ensure!(
                result.is_ok() || result == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND,
                "无法删除本程序自启动设置"
            );
        }
        Ok(())
    })();
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}
