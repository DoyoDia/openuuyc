//! Display-specific ownership and occupancy; package mechanics are shared.
use super::virtual_driver::Driver;
use crate::platform::windows::components::{
    Kind, Operation, Status,
    driver::{Package, Removal},
};
use anyhow::{Context, Result, ensure};
use windows::core::GUID;
const FILES: &[(&str, &[u8])] = &[
    (
        "OpenUUYCDisplay.inf",
        include_bytes!("../../../../assets/drivers/display/OpenUUYCDisplay.inf"),
    ),
    (
        "OpenUUYCDisplay.dll",
        include_bytes!("../../../../assets/drivers/display/OpenUUYCDisplay.dll"),
    ),
    (
        "OpenUUYCDisplay.cat",
        include_bytes!("../../../../assets/drivers/display/OpenUUYCDisplay.cat"),
    ),
];
const PACKAGE: Package = Package {
    kind: Kind::DisplayDriver,
    class: GUID::from_u128(0x4D36E968_E325_11CE_BFC1_08002BE10318),
    hardware: r"ROOT\OPENUUYC_DISPLAY",
    node: "OpenUUYCDisplay",
    description: "OpenUUYC Virtual Display Adapter",
    inf: "OpenUUYCDisplay.inf",
    catalog: "OpenUUYCDisplay.cat",
    files: FILES,
    repair: true,
};
pub(crate) fn status() -> Result<Status> {
    let mut status = PACKAGE.status()?;
    if status.ready && Driver::device_instance().is_err() {
        status.label = "已安装，显示接口未就绪".into();
        status.ready = false;
    }
    Ok(status)
}
pub(crate) fn preflight(operation: Operation) -> Result<()> {
    if operation == Operation::Uninstall || PACKAGE.status()?.installed {
        drop(removal_plan()?);
    }
    Ok(())
}
pub(crate) fn install() -> Result<bool> {
    let _serial = super::recovery::Serial::acquire()?;
    preflight(Operation::Install)?;
    PACKAGE.install()
}
fn removal_plan() -> Result<Removal> {
    let plan = PACKAGE.removal()?;
    if let Some(instance) = plan.instance()? {
        for target in super::topology::Topology::query(false)?
            .targets()?
            .iter()
            .filter(|t| t.available)
        {
            let path = target
                .adapter_path
                .trim_start_matches(r"\\?\")
                .trim_start_matches(r"\??\");
            let owner = path.split("#{").next().unwrap_or(path).replace('#', r"\");
            ensure!(
                !owner.eq_ignore_ascii_case(&instance),
                "虚拟显示驱动仍有显示器，请先结束使用它的连接或程序"
            );
        }
        if Driver::device_instance().is_ok() {
            drop(Driver::open().context("虚拟显示驱动正在使用中，无法卸载")?);
        }
    }
    Ok(plan)
}
pub(crate) fn uninstall() -> Result<bool> {
    let _serial = super::recovery::Serial::acquire()?;
    let reboot = removal_plan()?.execute()?;
    if !reboot {
        ensure!(!status()?.removable, "卸载尚未完成，请重新检查驱动状态");
    }
    Ok(reboot)
}
