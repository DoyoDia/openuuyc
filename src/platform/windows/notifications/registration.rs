//! User-scoped identity and protocol activation for unpackaged desktop toasts.
use super::super::registry::{Key, key, read, set, wide};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::StructuredStorage::*, Com::*, Registry::*},
        UI::Shell::{PropertiesSystem::*, *},
    },
    core::{GUID, Interface, PCWSTR, w},
};
const SCHEME: &str = r"Software\Classes\openuuyc-notification";
const OWNER: &str = "OpenUUYC.Notifications.v1";
const APP_KEY: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};
const TOAST_KEY: PROPERTYKEY = PROPERTYKEY {
    fmtid: APP_KEY.fmtid,
    pid: 26,
};
const ACTIVATOR: GUID = GUID::from_u128(0x8eafbec2_7395_44cd_96cb_78dfc53072db);
fn command(image: &Path) -> String {
    format!("\"{}\" notification \"%1\"", image.display())
}
fn stored_command() -> Option<String> {
    read(&format!(r"{SCHEME}\shell\open\command"), "")
        .ok()
        .flatten()
}
fn owned_registration() -> bool {
    read(SCHEME, "OpenUUYCOwner").ok().flatten().as_deref() == Some(OWNER)
}
pub(crate) fn owns_shortcut(link: &IShellLinkW, target: &Path) -> bool {
    let checks = (|| -> Result<bool> {
        let properties: IPropertyStore = link.cast()?;
        let value = unsafe { properties.GetValue(&APP_KEY)? };
        let name = windows::core::BSTR::try_from(&value)?.to_string();
        Ok(name == super::AUMID
            && owned_registration()
            && stored_command().is_some_and(|v| v.eq_ignore_ascii_case(&command(target))))
    })();
    checks.unwrap_or(false)
}
fn shortcut() -> Result<PathBuf> {
    let p = unsafe { SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None)? };
    let root = unsafe { p.to_string() };
    unsafe {
        CoTaskMemFree(Some(p.0.cast()));
    }
    Ok(PathBuf::from(root?).join("OpenUUYC.lnk"))
}
pub(super) fn register(aumid: &str) -> Result<()> {
    let image = dunce::canonicalize(std::env::current_exe()?)?;
    let old = read(&format!(r"{SCHEME}\shell\open\command"), "")?;
    let owner = read(SCHEME, "OpenUUYCOwner")?;
    let mut existing = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(SCHEME).as_ptr()),
            None,
            KEY_READ,
            &mut existing,
        )
    };
    if opened.is_ok() {
        drop(Key(existing));
        ensure!(
            old.as_ref()
                .is_some_and(|v| v.eq_ignore_ascii_case(&command(&image)))
                || owner.as_deref() == Some(OWNER),
            "通知协议已登记到其他程序"
        );
    } else if opened != ERROR_FILE_NOT_FOUND && opened != ERROR_PATH_NOT_FOUND {
        opened.ok()?;
    }
    let path = shortcut()?;
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
    let file: IPersistFile = link.cast()?;
    if path.exists() {
        unsafe {
            // Updating an existing shortcut also writes its property store.
            // STGM_READ makes SetValue/Commit fail with STG_E_ACCESSDENIED,
            // even when the user has full filesystem access to the shortcut.
            file.Load(
                PCWSTR(wide(&path.to_string_lossy()).as_ptr()),
                STGM_READWRITE,
            )
            .context("以读写方式打开通知快捷方式")?;
        }
        let mut target = [0u16; 32768];
        unsafe {
            link.GetPath(&mut target, std::ptr::null_mut(), 0)?;
        }
        let end = target
            .iter()
            .position(|&v| v == 0)
            .context("快捷方式目标无效")?;
        let target = PathBuf::from(String::from_utf16_lossy(&target[..end]));
        ensure!(
            target == image || owns_shortcut(&link, &target),
            "已有OpenUUYC快捷方式指向其他程序，已保留"
        );
    }
    unsafe {
        link.SetPath(PCWSTR(wide(&image.to_string_lossy()).as_ptr()))?;
        link.SetArguments(w!("gui"))?;
        link.SetDescription(w!("OpenUUYC"))?;
        link.SetIconLocation(PCWSTR(wide(&image.to_string_lossy()).as_ptr()), 0)?;
    }
    let properties: IPropertyStore = link.cast()?;
    let text = PROPVARIANT::from(aumid);
    let mut app = PROPVARIANT::default();
    unsafe {
        PropVariantChangeType(
            &mut app,
            &text,
            PROPVAR_CHANGE_FLAGS(0),
            windows::Win32::System::Variant::VT_LPWSTR,
        )?;
    }
    let activator = unsafe { InitPropVariantFromCLSID(&ACTIVATOR)? };
    std::fs::create_dir_all(path.parent().context("快捷方式目录无效")?)?;
    unsafe {
        properties
            .SetValue(&APP_KEY, &app)
            .context("写入通知应用标识")?;
        properties
            .SetValue(&TOAST_KEY, &activator)
            .context("写入通知激活标识")?;
        properties.Commit().context("提交通知快捷方式属性")?;
        file.Save(PCWSTR(wide(&path.to_string_lossy()).as_ptr()), true)
            .context("保存通知快捷方式")?;
    }
    let root = key(SCHEME)?;
    set(root.0, "", "URL:OpenUUYC notification")?;
    set(root.0, "URL Protocol", "")?;
    set(root.0, "OpenUUYCOwner", OWNER)?;
    let open = key(&format!(r"{SCHEME}\shell\open\command"))?;
    set(open.0, "", &command(&image))?;
    unsafe {
        SHChangeNotify(
            SHCNE_CREATE,
            SHCNF_PATHW | SHCNF_FLUSH,
            Some(wide(&path.to_string_lossy()).as_ptr().cast()),
            None,
        );
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST | SHCNF_FLUSH, None, None);
    }
    Ok(())
}
pub(crate) fn unregister(image: &Path) -> Result<()> {
    if owned_registration()
        && stored_command().is_some_and(|v| v.eq_ignore_ascii_case(&command(image)))
    {
        unsafe {
            RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide(SCHEME).as_ptr())).ok()?;
        }
    }
    Ok(())
}
/// Installation already updated owned shortcuts; retarget an existing toast
/// protocol even when Windows notifications are currently disabled in the UI.
pub(crate) fn retarget_installed() -> Result<()> {
    if !owned_registration() {
        return Ok(());
    }
    let image = crate::platform::windows::components::application::image()?;
    let open = key(&format!(r"{SCHEME}\shell\open\command"))?;
    set(open.0, "", &command(&image))
}
