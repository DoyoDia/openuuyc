//! Optional audio component, using the common PnP/installation transaction.
use crate::platform::windows::components::{Kind, Operation, Status, driver::Package};
use anyhow::{Context, Result, ensure};
use windows::core::GUID;
include!(concat!(env!("OUT_DIR"), "/audio_driver.rs"));
const PACKAGE: Package = Package {
    kind: Kind::AudioDriver,
    class: GUID::from_u128(0x4d36e96c_e325_11ce_bfc1_08002be10318),
    hardware: r"ROOT\OPENUUYC_AUDIO",
    node: "OpenUUYCAudio",
    description: "OpenUUYC Virtual Audio",
    inf: "OpenUUYCAudio.inf",
    catalog: "OpenUUYCAudio.cat",
    files: FILES,
    repair: true,
};
pub(crate) fn status() -> Result<Status> {
    PACKAGE.status()
}
fn idle() -> Result<()> {
    if status()?.installed {
        // If another process owns the bridge, open fails and the mutation is
        // refused. Active WASAPI users also prevent removal, even without us.
        match super::Bridge::open() {
            Ok(mut bridge) => {
                let state = bridge.state()?;
                ensure!(
                    state.microphone_running == 0 && state.speaker_running == 0,
                    "虚拟声卡正在使用，请先关闭使用它的应用和连接"
                );
            }
            Err(error) => {
                use windows::Win32::Foundation::{
                    ERROR_DEV_NOT_EXIST, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND,
                    ERROR_PATH_NOT_FOUND,
                };
                let missing = error
                    .downcast_ref::<windows::core::Error>()
                    .is_some_and(|error| {
                        [
                            ERROR_FILE_NOT_FOUND,
                            ERROR_PATH_NOT_FOUND,
                            ERROR_DEV_NOT_EXIST,
                            ERROR_DEVICE_NOT_CONNECTED,
                        ]
                        .iter()
                        .any(|code| error.code() == code.to_hresult())
                    });
                ensure!(missing, "虚拟声卡接口不可用或正在使用中：{error:#}");
            }
        }
    }
    Ok(())
}
pub(crate) fn preflight() -> Result<()> {
    idle()?;
    // Resolve ownership before other components are stopped or removed.
    drop(PACKAGE.removal()?);
    Ok(())
}
pub(crate) fn execute(operation: Operation) -> Result<bool> {
    idle()?;
    match operation {
        Operation::Install => {
            let mut defaults =
                super::defaults::Defaults::snapshot().context("无法保存安装前的默认音频设备")?;
            // Windows decides whether this boot permits the test-signed image.
            // TESTSIGNING is not the only supported development route (the
            // Startup Settings one-boot option leaves that BCD setting off).
            // Catalog verification, package ownership and PnP checks still run.
            let installed = PACKAGE.install().context("虚拟声卡安装或加载失败；此测试签名包需要本次启动允许测试驱动，具体原因见系统错误和安装日志")
                .and_then(|reboot| { if !reboot { defaults.wait_for_install()?; } Ok(reboot) });
            let restored = defaults.restore().context("安装后恢复默认音频设备失败");
            match (installed, restored) {
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error),
                (Ok(reboot), Ok(())) => Ok(reboot),
            }
        }
        Operation::Uninstall => {
            super::Defaults::recover_defaults().context("卸载前恢复默认音频设备失败")?;
            PACKAGE.removal()?.execute()
        }
    }
}
