//! Account-independent lifetime of the installed headless display maintainer.
use super::{pipe::Handle, process};
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};
use windows::Win32::{Foundation::WAIT_TIMEOUT, System::Threading::*};

#[derive(serde::Serialize, serde::Deserialize)]
struct Identity {
    pid: u32,
    parent: u32,
    birth: u64,
}
fn identity_path() -> Result<std::path::PathBuf> {
    Ok(super::install::directory()?.join("display-agent.json"))
}
pub(super) fn maintenance_pid(parent: u32) -> Result<Option<u32>> {
    let bytes = match std::fs::read(identity_path()?) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(bytes.len() < 1024, "显示守护身份记录过大");
    let identity: Identity = serde_json::from_slice(&bytes)?;
    if identity.parent != parent
        || !crate::platform::display::recovery::owner_alive(identity.pid, identity.birth)?
    {
        return Ok(None);
    }
    Ok(Some(identity.pid))
}

pub(crate) fn supervise(running: impl Fn() -> bool) -> Result<()> {
    let identity_path = identity_path()?;
    let _ = std::fs::remove_file(&identity_path);
    let mut child: Option<(u32, process::Agent)> = None;
    let mut retry = Instant::now();
    while running() {
        let session = process::active_session();
        let enabled = session != u32::MAX
            && crate::platform::display::virtual_driver::Driver::device_instance().is_ok();
        if child
            .as_ref()
            .is_some_and(|(id, p)| !enabled || *id != session || !p.alive())
        {
            child.take();
            let _ = std::fs::remove_file(&identity_path);
            retry = Instant::now() + Duration::from_secs(2);
        }
        if enabled && child.is_none() && Instant::now() >= retry {
            match process::Agent::start_display(session) {
                Ok(agent) => {
                    let identity = Identity {
                        pid: agent.pid,
                        parent: std::process::id(),
                        birth: crate::platform::display::recovery::birth(agent.process.0)?,
                    };
                    std::fs::write(&identity_path, serde_json::to_vec(&identity)?)?;
                    child = Some((session, agent));
                }
                Err(error) => tracing::warn!(%error,"display maintainer launch deferred"),
            }
            retry = Instant::now() + Duration::from_secs(10);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // The job ends only our maintainer. The driver's fallback survives; next
    // service/session owner queries and adopts it instead of removing all screens.
    drop(child);
    let _ = std::fs::remove_file(&identity_path);
    Ok(())
}
pub(crate) fn run(parent: u32) -> Result<()> {
    ensure!(
        super::vault::sid(std::process::id())? == "S-1-5-18",
        "显示守护只能由系统服务启动"
    );
    super::install::verify_running(parent)?;
    process::verify_image(parent)?;
    let parent = Handle(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, parent) }?);
    let session = process::session(std::process::id())?;
    let mut owner = crate::features::host::displays::fallback::Maintainer::default();
    let mut last = String::new();
    while unsafe { WaitForSingleObject(parent.0, 1000) } == WAIT_TIMEOUT
        && process::active_session() == session
    {
        match owner.tick() {
            Ok(()) => last.clear(),
            Err(error) => {
                let message = format!("{error:#}");
                if message != last {
                    tracing::warn!(error=%message,"display maintenance deferred");
                    last = message;
                }
            }
        }
    }
    Ok(())
}
