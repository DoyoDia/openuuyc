//! Independent optional HID driver; does not deploy or modify the host service.
use crate::platform::windows::components::{Kind, Status, driver::Package};
use anyhow::Result;
use windows::core::GUID;
const FILES: &[(&str, &[u8])] = &[
    (
        "OpenUUYCInput.inf",
        include_bytes!("../../../../assets/drivers/input/OpenUUYCInput.inf"),
    ),
    (
        "OpenUUYCInput.dll",
        include_bytes!("../../../../assets/drivers/input/OpenUUYCInput.dll"),
    ),
    (
        "OpenUUYCInput.cat",
        include_bytes!("../../../../assets/drivers/input/OpenUUYCInput.cat"),
    ),
];

const PACKAGE: Package = Package {
    kind: Kind::InputDriver,
    class: GUID::from_u128(0x4d36e97d_e325_11ce_bfc1_08002be10318),
    hardware: r"ROOT\OPENUUYC_INPUT",
    node: "OpenUUYCInput",
    description: "OpenUUYC Input",
    inf: "OpenUUYCInput.inf",
    catalog: "OpenUUYCInput.cat",
    files: FILES,
    repair: true,
};
pub(crate) fn status() -> Result<Status> {
    PACKAGE.status()
}
pub(crate) fn install() -> Result<bool> {
    PACKAGE.install()
}
pub(crate) fn uninstall() -> Result<bool> {
    let plan = PACKAGE.removal()?;
    plan.execute()
}
