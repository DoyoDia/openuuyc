//! Single management boundary for three independently optional components.
pub(crate) mod application;
pub(crate) mod driver;
pub(crate) mod files;
pub(crate) mod suite;
use anyhow::{Context, Result, ensure};
use std::sync::atomic::{AtomicBool, Ordering};
use windows::{
    Win32::{
        Foundation::*,
        System::Threading::*,
        UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
    },
    core::{PCWSTR, w},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Kind {
    Application,
    Suite,
    #[value(skip)]
    HostService,
    #[value(skip)]
    InputDriver,
    #[value(skip)]
    DisplayDriver,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Operation {
    Install,
    Uninstall,
}
static MAINTENANCE: AtomicBool = AtomicBool::new(false);
pub(crate) fn maintaining() -> bool {
    MAINTENANCE.load(Ordering::Acquire)
}
struct Maintenance;
impl Drop for Maintenance {
    fn drop(&mut self) {
        MAINTENANCE.store(false, Ordering::Release);
    }
}
pub(crate) struct Status {
    pub label: String,
    pub installed: bool,
    pub removable: bool,
    pub ready: bool,
}
impl Kind {
    pub(crate) const ALL: [Self; 3] = [Self::HostService, Self::InputDriver, Self::DisplayDriver];
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Application => "程序",
            Self::Suite => "服务",
            Self::HostService => "被控服务",
            Self::InputDriver => "输入驱动",
            Self::DisplayDriver => "虚拟显示驱动",
        }
    }
    pub(crate) fn certificate(self) -> Option<(&'static str, &'static str, &'static [u8])> {
        match self {
            Self::HostService | Self::Suite | Self::Application => None,
            Self::InputDriver | Self::DisplayDriver => Some((
                "CN=OpenUUYC Drivers",
                "CCDA4E99C26E394C8A0AFD3B85AB03A608C66611",
                include_bytes!("../../../../assets/drivers/OpenUUYCDrivers.cer"),
            )),
        }
    }
}
/// Called only by the elevated, explicitly confirmed installer. Trust the pinned
/// embedded public certificate directly; do not reopen a user-writable temp file.
pub(crate) fn trust_certificate(kind: Kind) -> Result<()> {
    use windows::Win32::Security::Cryptography::*;
    let (_, expected, bytes) = kind.certificate().context("被控服务不需要驱动证书")?;
    unsafe {
        let context = CertCreateCertificateContext(X509_ASN_ENCODING, bytes);
        ensure!(!context.is_null(), "组件签名证书无效");
        let mut hash = [0u8; 20];
        let mut length = hash.len() as u32;
        let result = CertGetCertificateContextProperty(
            context,
            CERT_SHA1_HASH_PROP_ID,
            Some(hash.as_mut_ptr().cast()),
            &mut length,
        );
        let _ = CertFreeCertificateContext(Some(context));
        result?;
        let actual = hash.iter().map(|b| format!("{b:02X}")).collect::<String>();
        ensure!(
            length == 20 && actual == expected,
            "组件签名证书与内置指纹不匹配"
        );
        for name in [w!("ROOT"), w!("TrustedPublisher")] {
            let store = CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                CERT_QUERY_ENCODING_TYPE(0),
                None,
                CERT_OPEN_STORE_FLAGS(CERT_SYSTEM_STORE_LOCAL_MACHINE)
                    | CERT_STORE_OPEN_EXISTING_FLAG,
                Some(name.as_ptr().cast()),
            )?;
            let result = CertAddEncodedCertificateToStore(
                Some(store),
                X509_ASN_ENCODING,
                bytes,
                CERT_STORE_ADD_USE_EXISTING,
                None,
            );
            let close = CertCloseStore(Some(store), 0);
            result?;
            close?;
        }
    }
    Ok(())
}

pub(crate) fn status(kind: Kind) -> Result<Status> {
    match kind {
        Kind::Application => suite::status(),
        Kind::Suite => suite::status(),
        Kind::HostService => super::host_service::install::status(),
        Kind::InputDriver => super::input::install::status(),
        Kind::DisplayDriver => super::display::install::status(),
    }
}
pub(crate) fn execute(
    kind: Kind,
    operation: Operation,
    allow_sas: bool,
    owner: Option<&str>,
    remove_display: bool,
    remove_data: bool,
) -> Result<bool> {
    let result = (|| {
        ensure!(
            matches!(kind, Kind::Application | Kind::Suite),
            "请使用统一安装或卸载入口"
        );
        ensure!(
            owner.is_none() || (kind == Kind::Suite && operation == Operation::Install),
            "安装用户只适用于安装服务"
        );
        ensure!(
            !remove_data || (kind == Kind::Application && operation == Operation::Uninstall),
            "清除数据仅适用于卸载程序"
        );
        ensure!(
            (kind == Kind::Suite && operation == Operation::Install) || !allow_sas,
            "SAS策略只属于被控服务"
        );
        ensure!(
            !remove_display
                || (matches!(kind, Kind::Suite | Kind::Application)
                    && operation == Operation::Uninstall),
            "显示驱动卸载选项只属于整套卸载"
        );
        let _serial = Serial::acquire()?;
        super::host_service::install::preflight()?;
        match (kind, operation) {
            (Kind::Application, Operation::Uninstall) => application::uninstall(remove_display),
            (Kind::Application, Operation::Install) => anyhow::bail!("请使用程序安装入口"),
            (Kind::Suite, operation) => suite::execute(operation, owner, allow_sas, remove_display),
            _ => unreachable!("component target checked above"),
        }
    })();
    match &result {
        Ok(reboot) => tracing::info!(?kind, ?operation, reboot, "component operation completed"),
        Err(error) => tracing::error!(?kind,?operation,%error,"component operation failed"),
    }
    result
}
struct Serial(super::host_service::pipe::Handle);
impl Serial {
    fn acquire() -> Result<Self> {
        let handle = super::host_service::pipe::Handle(unsafe {
            CreateMutexW(None, false, w!("Global\\OpenUUYC.ComponentMutation"))
        }?);
        let result = unsafe { WaitForSingleObject(handle.0, 0) };
        ensure!(
            result == WAIT_OBJECT_0 || result == WAIT_ABANDONED,
            "已有组件操作正在进行，请稍后重试"
        );
        Ok(Self(handle))
    }
}
impl Drop for Serial {
    fn drop(&mut self) {
        let _ = unsafe { ReleaseMutex(self.0.0) };
    }
}
pub(crate) fn request(
    kind: Kind,
    operation: Operation,
    allow_sas: bool,
    remove_display: bool,
    remove_data: bool,
) -> Result<bool> {
    ensure!(
        MAINTENANCE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "已有组件操作正在进行"
    );
    let _maintenance = Maintenance;
    if kind == Kind::Application {
        ensure!(operation == Operation::Uninstall, "无效程序操作");
        return application::request_uninstall(remove_display, remove_data);
    }
    ensure!(!remove_data, "清除数据仅适用于卸载程序");
    if kind == Kind::Suite {
        return suite::request(operation, allow_sas, remove_display);
    }
    anyhow::bail!("请使用统一安装或卸载入口")
}
pub(super) fn elevate(args: &str, kind: Kind) -> Result<bool> {
    let exe = std::env::current_exe()?;
    let exe: Vec<u16> = exe
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let args: Vec<u16> = args.encode_utf16().chain(Some(0)).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.context("组件操作未启动（可能已取消管理员授权）")?;
    let process = super::host_service::pipe::Handle(info.hProcess);
    ensure!(!process.0.is_invalid(), "组件操作进程不可用");
    ensure!(
        unsafe { WaitForSingleObject(process.0, INFINITE) } == WAIT_OBJECT_0,
        "无法等待组件操作完成"
    );
    let mut code = 0;
    unsafe {
        GetExitCodeProcess(process.0, &mut code)?;
    }
    ensure!(
        code == 0 || code == 3010,
        "{}操作失败，请查看日志（退出码 {code}）",
        kind.label()
    );
    Ok(code == 3010)
}
