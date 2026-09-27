//! Product installation composes independent backends and records its ownership.
use super::{Kind, Operation, Status};
use crate::platform::windows::host_service::{self, vault};
use anyhow::{Context, Result, ensure};
#[derive(serde::Serialize, serde::Deserialize)]
struct Receipt {
    owner: String,
}
fn path() -> Result<std::path::PathBuf> {
    Ok(host_service::install::directory()?.join("suite.json"))
}
fn receipt() -> Result<Option<Receipt>> {
    match std::fs::read(path()?) {
        Ok(bytes) => {
            ensure!(bytes.len() < 4096, "服务安装记录无效");
            Ok(Some(serde_json::from_slice(&bytes)?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn save(value: &Receipt) -> Result<()> {
    std::fs::write(path()?, serde_json::to_vec(value)?)?;
    Ok(())
}
pub(crate) fn status() -> Result<Status> {
    let service = host_service::install::status()?;
    let installed = receipt()?.is_some();
    let complete = installed
        && vault::applies()?
        && service.ready
        && super::super::input::install::status()?.ready
        && super::super::display::install::status()?.ready;
    Ok(Status {
        label: if complete {
            "服务已安装".into()
        } else if installed {
            "服务需要完成安装或更新".into()
        } else {
            "未安装常驻服务".into()
        },
        installed,
        removable: installed,
        ready: complete,
    })
}
pub(crate) fn execute(
    operation: Operation,
    owner: Option<&str>,
    allow_sas: bool,
    remove_display: bool,
) -> Result<bool> {
    match operation {
        Operation::Install => {
            let owner = owner.context("缺少安装用户")?;
            let record = receipt()?.unwrap_or_else(|| Receipt {
                owner: owner.into(),
            });
            ensure!(record.owner == owner, "此服务由另一 Windows 用户管理");
            host_service::install::preflight()?;
            super::super::display::install::preflight(Operation::Install)?;
            vault::create(owner)?;
            let mut reboot = host_service::install::install(allow_sas)?;
            save(&record)?;
            let input = super::super::input::install::status()?;
            if !input.ready {
                reboot |= super::super::input::install::install()?;
            }
            let display = super::super::display::install::status()?;
            if !display.ready {
                reboot |= super::super::display::install::install()?;
            }
            host_service::install::start_installed()?;
            if let Err(error) = super::application::cleanup_legacy() {
                tracing::warn!(%error, "previous installation remains in use; retained for later cleanup");
            }
            Ok(reboot)
        }
        Operation::Uninstall => {
            let record = receipt()?.context("没有本程序的服务安装记录")?;
            host_service::install::preflight()?;
            if remove_display {
                super::super::display::install::preflight(Operation::Uninstall)?;
            }
            // Close admission and agents before removing their driver resources.
            host_service::install::stop_for_maintenance()?;
            let mut reboot = false;
            if remove_display {
                reboot |= super::super::display::install::uninstall()?;
            }
            reboot |= super::super::input::install::uninstall()?;
            reboot |= host_service::install::uninstall()?;
            // Only this fixed, explicitly owned vault is removed. Keep unrelated
            // user settings, other drivers and shared signing certificates.
            let root = vault::root()?;
            ensure!(vault::owner()? == Some(record.owner), "服务数据归属已改变");
            // Switch readers back to the already-restored portable store before
            // deleting any encrypted entry; never expose a half-empty account.
            let enabled = root.join("enabled");
            if enabled.exists() {
                std::fs::remove_file(enabled)?;
            }
            for entry in std::fs::read_dir(&root)? {
                let entry = entry?;
                let file = entry.path();
                super::files::reject_reparse(&file)?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if entry.file_type()?.is_file()
                    && (matches!(
                        name.as_ref(),
                        "enabled" | "owner.sid" | "identity.lock" | "session.lock"
                    ) || name.ends_with(".secret") && name.len() == 71)
                {
                    std::fs::remove_file(file)?;
                }
            }
            std::fs::remove_file(path()?)?;
            if std::fs::read_dir(&root)?.next().is_none() {
                std::fs::remove_dir(root)?;
            }
            Ok(reboot)
        }
    }
}
pub(crate) fn request(operation: Operation, allow_sas: bool, remove_display: bool) -> Result<bool> {
    if operation == Operation::Uninstall {
        crate::account::auth::restore_portable()?;
    }
    let owner = vault::sid(std::process::id())?;
    let args = format!(
        "component suite {}{}{}",
        if operation == Operation::Install {
            "install"
        } else {
            "uninstall"
        },
        if operation == Operation::Install {
            format!(" --owner {owner}")
        } else {
            String::new()
        },
        if operation == Operation::Install && allow_sas {
            " --allow-sas"
        } else {
            ""
        }
    );
    let args = if remove_display && operation == Operation::Uninstall {
        format!("{args} --remove-display-driver")
    } else {
        args
    };
    let reboot = super::elevate(&args, Kind::Suite)?;
    if operation == Operation::Install {
        crate::account::auth::enroll_resident()?;
        host_service::startup::set(true)?;
        super::application::integrate_user()?;
        let _ = host_service::resident::call(host_service::resident::Request::Resume)?;
    } else {
        host_service::startup::set(false)?;
    }
    Ok(reboot)
}
