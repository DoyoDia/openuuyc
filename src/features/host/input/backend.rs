//! Local and service execution share the same engine and ownership contract.
#[cfg(windows)]
use super::broker::Remote;
use super::{engine::Engine, geometry::Geometry, wire::Event};
use anyhow::Result;
pub(super) enum Backend {
    Local(Engine),
    #[cfg(windows)]
    Service(Remote),
}
impl Backend {
    pub fn new(
        policy: super::wire::Policy,
        require_service: bool,
        permitted: impl Fn() -> bool,
    ) -> Result<Self> {
        #[cfg(windows)]
        if let Some(remote) = Remote::connect(policy, permitted)? {
            return Ok(Self::Service(remote));
        }
        #[cfg(not(windows))]
        let _ = permitted;
        anyhow::ensure!(!require_service, "系统被控服务尚未恢复");
        Ok(Self::Local(Engine::new(false, policy, std::process::id())?))
    }
    pub fn service(&self) -> bool {
        match self {
            Self::Local(_) => false,
            #[cfg(windows)]
            Self::Service(_) => true,
        }
    }
    pub fn healthy(&self) -> bool {
        match self {
            Self::Local(_) => true,
            #[cfg(windows)]
            Self::Service(remote) => remote.healthy(),
        }
    }
    pub fn backend(&self) -> &str {
        match self {
            Self::Local(e) => e.backend(),
            #[cfg(windows)]
            Self::Service(e) => e.backend(),
        }
    }
    pub fn apply(
        &mut self,
        event: Event,
        geometry: Geometry,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        match self {
            Self::Local(e) => {
                e.apply(event, geometry, permitted)?;
                anyhow::ensure!(!e.take_sas(), "Ctrl+Alt+Del需要安装并启用被控服务");
                Ok(())
            }
            #[cfg(windows)]
            Self::Service(e) => e.apply(event, geometry, permitted),
        }
    }
    pub fn release(&mut self) -> Result<()> {
        match self {
            Self::Local(e) => e.release(),
            #[cfg(windows)]
            Self::Service(e) => e.release(),
        }
    }
    pub fn synchronize(&mut self, geometry: Geometry) -> Result<()> {
        match self {
            Self::Local(e) => e.synchronize(geometry),
            #[cfg(windows)]
            Self::Service(e) => e.synchronize(geometry),
        }
    }
    pub fn configure(&mut self, configuration: super::config::Configuration) -> Result<()> {
        match self {
            Self::Local(e) => e.configure(configuration),
            #[cfg(windows)]
            Self::Service(e) => e.configure(configuration),
        }
    }
    pub fn mouse_policy(&self) -> super::config::MousePolicy {
        match self {
            Self::Local(e) => e.mouse_policy(),
            #[cfg(windows)]
            Self::Service(e) => e.mouse_policy(),
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        match self {
            Self::Local(e) => e.tick(),
            #[cfg(windows)]
            Self::Service(e) => e.tick(),
        }
    }
}
