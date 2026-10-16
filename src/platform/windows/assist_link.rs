//! `uuremote://` 协议注册：让系统把协助邀请链接交给本程序。
//!
//! 官方客户端把这条协议登记在 HKLM（安装时要管理员），而 `HKCU\Software\Classes`
//! 对当前用户优先。我们只动 HKCU：不用提权，也不碰官方客户端写下的那一份，
//! 卸载时只删带自己归属标记的键。
//!
//! 注册与否由用户在安装对话框里决定（见组件管理），这里只负责把决定落到注册表，
//! 并在程序搬家或更新后把命令改回当前路径。
use super::registry::{key, read, set, wide};
use anyhow::Result;
use std::path::Path;
use windows::{
    Win32::{System::Registry::*, UI::Shell::*},
    core::PCWSTR,
};

const SCHEME: &str = r"Software\Classes\uuremote";
const OWNER: &str = "OpenUUYC.AssistLink.v1";

fn command(image: &Path) -> String {
    format!("\"{}\" \"%1\"", image.display())
}

fn stored_command() -> Option<String> {
    read(&format!(r"{SCHEME}\shell\open\command"), "")
        .ok()
        .flatten()
}

/// 这条协议是不是我们登记的。官方客户端写在 HKLM，看不到这个标记。
pub(crate) fn owned() -> bool {
    read(SCHEME, "OpenUUYCOwner").ok().flatten().as_deref() == Some(OWNER)
}

/// 登记本程序，或者把已有的登记改指过来。用户已经在界面上确认过要接管，
/// 所以这里直接覆盖 HKCU 里同名的那一项。
pub(crate) fn enable() -> Result<()> {
    let image = dunce::canonicalize(std::env::current_exe()?)?;
    let root = key(SCHEME)?;
    set(root.0, "", "URL:OpenUUYC remote assistance")?;
    set(root.0, "URL Protocol", "")?;
    set(root.0, "OpenUUYCOwner", OWNER)?;
    let open = key(&format!(r"{SCHEME}\shell\open\command"))?;
    set(open.0, "", &command(&image))?;
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST | SHCNF_FLUSH, None, None);
    }
    tracing::info!(image = %image.display(), "已接管 uuremote:// 协助链接");
    Ok(())
}

/// 取消接管。只在登记确实是我们写的（归属标记还在）时才删，免得删掉别人
/// 后来写进去的同一项。
pub(crate) fn disable() -> Result<()> {
    if !owned() {
        return Ok(());
    }
    unsafe {
        RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide(SCHEME).as_ptr())).ok()?;
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST | SHCNF_FLUSH, None, None);
    }
    tracing::info!("已交回 uuremote:// 协助链接");
    Ok(())
}

/// 程序搬家或更新之后，把命令改回当前这份程序。没登记过就什么都不做——
/// 不能因为本程序启动一次就把用户留给别的程序的协议抢过来。
pub(crate) fn retarget() -> Result<()> {
    if !owned() {
        return Ok(());
    }
    let image = dunce::canonicalize(std::env::current_exe()?)?;
    let wanted = command(&image);
    if stored_command().is_some_and(|value| value == wanted) {
        return Ok(());
    }
    let open = key(&format!(r"{SCHEME}\shell\open\command"))?;
    set(open.0, "", &wanted)?;
    Ok(())
}

/// 控制中心每次起来都校正一次路径；失败不打断启动。
pub(crate) fn retarget_on_start() {
    if let Err(error) = retarget() {
        tracing::warn!(error = %format!("{error:#}"), "校正协助链接的注册失败");
    }
}
