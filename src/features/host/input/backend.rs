//! Local and service execution share the same engine and ownership contract.
use super::{
    broker::Remote,
    engine::{Engine, Geometry},
    wire::Event,
};
use anyhow::Result;
pub(super) enum Backend {
    Local(Engine),
    Service(Remote),
}
impl Backend {
    pub fn new(
        policy: super::wire::Policy,
        require_service: bool,
        permitted: impl Fn() -> bool,
    ) -> Result<Self> {
        Ok(match Remote::connect(policy, permitted)? {
            Some(remote) => Self::Service(remote),
            None => {
                anyhow::ensure!(!require_service, "系统被控服务尚未恢复");
                Self::Local(Engine::new(false, policy, std::process::id())?)
            }
        })
    }
    pub fn service(&self) -> bool {
        matches!(self, Self::Service(_))
    }
    pub fn healthy(&self) -> bool {
        match self {
            Self::Local(_) => true,
            Self::Service(remote) => remote.healthy(),
        }
    }
    pub fn backend(&self) -> &str {
        match self {
            Self::Local(e) => e.backend(),
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
            Self::Service(e) => e.apply(event, geometry, permitted),
        }
    }
    pub fn release(&mut self) -> Result<()> {
        match self {
            Self::Local(e) => e.release(),
            Self::Service(e) => e.release(),
        }
    }
    pub fn synchronize(&mut self, geometry: Geometry) -> Result<()> {
        match self {
            Self::Local(e) => e.synchronize(geometry),
            Self::Service(e) => e.synchronize(geometry),
        }
    }
    pub fn configure(&mut self, configuration: super::config::Configuration) -> Result<()> {
        match self {
            Self::Local(e) => e.configure(configuration),
            Self::Service(e) => e.configure(configuration),
        }
    }
    pub fn mouse_policy(&self) -> super::config::MousePolicy {
        match self {
            Self::Local(e) => e.mouse_policy(),
            Self::Service(e) => e.mouse_policy(),
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        match self {
            Self::Local(e) => e.tick(),
            Self::Service(e) => e.tick(),
        }
    }
}
