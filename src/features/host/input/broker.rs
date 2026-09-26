//! One protected service boundary; physical state remains in the session agent.
use super::{
    engine::{Engine, Geometry},
    wire::Event,
};
use crate::platform::windows::host_service::{
    pipe::{NAME, Pipe},
    process,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
enum Request {
    Hello {
        policy: super::wire::Policy,
        origin: u32,
    },
    Input {
        event: Event,
        geometry: Geometry,
    },
    Release,
    Alive,
    Sync(Geometry),
    Configure(super::config::Configuration),
    Close,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    backend: String,
    error: Option<String>,
    sas: bool,
    mouse_policy: super::config::MousePolicy,
}
pub(crate) struct Remote {
    pipe: Pipe,
    backend: String,
    last: Instant,
    failed: bool,
    geometry: Option<Geometry>,
    mouse_policy: super::config::MousePolicy,
}
impl Remote {
    pub fn connect(
        policy: super::wire::Policy,
        permitted: impl Fn() -> bool,
    ) -> Result<Option<Self>> {
        let Some(pipe) = Pipe::client(NAME)? else {
            return Ok(None);
        };
        let pid = pipe.peer_pid(false)?;
        if let Err(error) = crate::platform::windows::host_service::install::verify_running(pid) {
            if error.is::<crate::platform::windows::host_service::install::NeedsUpdate>() {
                return Ok(None);
            }
            return Err(error);
        }
        pipe.send(
            &Request::Hello {
                policy,
                origin: std::process::id(),
            },
            &permitted,
        )?;
        let reply: Reply = pipe.receive_timeout(Duration::from_secs(10), &permitted)?;
        if let Some(error) = reply.error {
            anyhow::bail!(error)
        }
        Ok(Some(Self {
            pipe,
            backend: reply.backend,
            last: Instant::now(),
            failed: false,
            geometry: None,
            mouse_policy: reply.mouse_policy,
        }))
    }
    pub fn backend(&self) -> &str {
        &self.backend
    }
    pub fn healthy(&self) -> bool {
        !self.failed
    }
    fn request(&mut self, request: Request, permitted: impl Fn() -> bool) -> Result<()> {
        ensure!(!self.failed, "被控服务连接已失效");
        let result = (|| {
            self.pipe.send(&request, &permitted)?;
            let reply: Reply = self.pipe.receive(&permitted)?;
            Ok(reply)
        })();
        // A lost response does not authorize replaying the command through another
        // backend. Disconnect is the only recovery for this input generation.
        match result {
            Ok(reply) => {
                self.last = Instant::now();
                self.backend = reply.backend;
                self.mouse_policy = reply.mouse_policy;
                if let Some(error) = reply.error {
                    anyhow::bail!(error)
                }
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
    pub fn apply(
        &mut self,
        event: Event,
        geometry: Geometry,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        self.request(Request::Input { event, geometry }, permitted)
    }
    pub fn release(&mut self) -> Result<()> {
        self.request(Request::Release, || true)
    }
    pub fn synchronize(&mut self, geometry: Geometry) -> Result<()> {
        if self.geometry.as_ref() != Some(&geometry) {
            self.request(Request::Sync(geometry.clone()), || true)?;
            self.geometry = Some(geometry);
        }
        Ok(())
    }
    pub fn configure(&mut self, configuration: super::config::Configuration) -> Result<()> {
        self.request(Request::Configure(configuration), || true)
    }
    pub fn mouse_policy(&self) -> super::config::MousePolicy {
        self.mouse_policy.clone()
    }
    pub fn tick(&mut self) -> Result<()> {
        if self.last.elapsed() >= Duration::from_millis(100) {
            self.request(Request::Alive, || true)?;
        }
        Ok(())
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        if !self.failed {
            let _ = self.pipe.send(&Request::Close, || true);
        }
    }
}

fn service_pid() -> Result<u32> {
    use windows::{Win32::System::Services::*, core::w};
    unsafe {
        let manager = OpenSCManagerW(None, None, SC_MANAGER_CONNECT)?;
        let service = OpenServiceW(manager, w!("OpenUUYCInputService"), SERVICE_QUERY_STATUS);
        let _ = CloseServiceHandle(manager);
        let service = service?;
        let mut state = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0;
        let bytes = std::slice::from_raw_parts_mut(
            (&mut state as *mut SERVICE_STATUS_PROCESS).cast(),
            std::mem::size_of_val(&state),
        );
        let result =
            QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed);
        let _ = CloseServiceHandle(service);
        result?;
        ensure!(state.dwCurrentState == SERVICE_RUNNING, "被控服务未运行");
        Ok(state.dwProcessId)
    }
}

/// Runs in the active Windows session under a service-created SYSTEM token.
/// A private SYSTEM-only pipe and the launching process identity own its lifetime.
pub(crate) fn agent(name: &str, parent: u32) -> Result<()> {
    ensure!(
        name.starts_with(r"\\.\pipe\OpenUUYC.Input.Agent.") && name.len() < 200,
        "输入会话管道无效"
    );
    ensure!(service_pid()? == parent, "输入会话父进程无效");
    process::verify_image(parent)?;
    let pipe = Pipe::client(name)?.ok_or_else(|| anyhow::anyhow!("输入会话已结束"))?;
    ensure!(pipe.peer_pid(false)? == parent, "输入会话服务身份无效");
    let session = process::session(std::process::id())?;
    let hardware =
        session == unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() };
    let permitted = || {
        process::active_session() == session
            && pipe.queued_bytes().is_ok()
            && (!hardware
                || session
                    == unsafe {
                        windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId()
                    })
    };
    let Request::Hello { policy, origin } = pipe.receive(permitted)? else {
        anyhow::bail!("缺少输入会话握手")
    };
    let mut engine = Engine::new(hardware, policy, origin)?;
    pipe.send(
        &Reply {
            backend: engine.backend().into(),
            error: None,
            sas: false,
            mouse_policy: engine.mouse_policy(),
        },
        permitted,
    )?;
    let mut last = Instant::now();
    while permitted() && last.elapsed() < Duration::from_millis(500) {
        if !pipe.available()? {
            engine.tick()?;
            continue;
        }
        let request: Request = pipe.receive(permitted)?;
        last = Instant::now();
        let close = matches!(request, Request::Close);
        let result = match request {
            Request::Input { event, geometry } => {
                event.validate()?;
                geometry.validate()?;
                // The broker is request/response serialized. Extra incoming data
                // during execution is its cancellation/close request.
                engine.apply(event, geometry, || {
                    permitted() && pipe.queued_bytes().is_ok_and(|n| n == 0)
                })
            }
            Request::Release | Request::Close => engine.release(),
            Request::Alive => engine.tick(),
            Request::Sync(geometry) => {
                geometry.validate()?;
                engine.synchronize(geometry)
            }
            Request::Configure(configuration) => engine.configure(configuration),
            Request::Hello { .. } => anyhow::bail!("重复输入会话握手"),
        };
        if result.is_err() {
            let _ = engine.release();
        }
        pipe.send(
            &Reply {
                backend: engine.backend().into(),
                error: result.err().map(|e| e.to_string()),
                sas: engine.take_sas(),
                mouse_policy: engine.mouse_policy(),
            },
            permitted,
        )?;
        if close {
            break;
        }
    }
    engine.release()
}

pub(crate) fn serve(pipe: Pipe, permitted: impl Fn() -> bool) -> Result<()> {
    let pid = pipe.peer_pid(true)?;
    crate::platform::windows::host_service::install::verify_client(pid)?;
    let session = process::session(pid)?;
    ensure!(
        session == process::active_session(),
        "输入请求不属于当前Windows会话"
    );
    let permitted =
        || permitted() && process::active_session() == session && pipe.queued_bytes().is_ok();
    let Request::Hello { policy, origin } = pipe.receive::<Request>(&permitted)? else {
        anyhow::bail!("缺少被控服务握手")
    };
    ensure!(origin == pid, "输入来源进程不匹配");
    let name = format!(r"\\.\pipe\OpenUUYC.Input.Agent.{}", uuid::Uuid::new_v4());
    let agent_pipe = Pipe::server(&name, false)?;
    let agent = process::Agent::start(session, &name)?;
    let started = Instant::now();
    agent_pipe
        .accept(|| permitted() && agent.alive() && started.elapsed() < Duration::from_secs(10))?;
    ensure!(
        agent_pipe.peer_pid(true)? == agent.pid,
        "输入会话进程身份无效"
    );
    agent_pipe.send(&Request::Hello { policy, origin }, &permitted)?;
    pipe.send(&agent_pipe.receive::<Reply>(&permitted)?, &permitted)?;
    let mut last = Instant::now();
    let result = (|| -> Result<()> {
        while permitted() && agent.alive() && last.elapsed() < Duration::from_millis(500) {
            if !pipe.available()? {
                continue;
            }
            let request: Request = pipe.receive(&permitted)?;
            let close = matches!(request, Request::Close);
            last = Instant::now();
            agent_pipe.send(&request, &permitted)?;
            let mut reply: Reply = agent_pipe.receive(&permitted)?;
            if reply.sas {
                reply.sas = false;
                ensure!(permitted(), "安全注意序列请求已撤销");
                if let Err(error) =
                    crate::platform::windows::host_service::service::secure_attention()
                {
                    reply.error = Some(error.to_string());
                }
            }
            pipe.send(&reply, &permitted)?;
            if close {
                break;
            }
        }
        Ok(())
    })();
    // Closing the app pipe cancels even an idle hold. Let the agent release before
    // the job handle provides a final crash/timeout boundary.
    let _ = agent_pipe.send(&Request::Close, || agent.alive());
    let _ = agent_pipe.receive::<Reply>(|| agent.alive());
    result
}
