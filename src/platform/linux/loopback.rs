//! Desktop sound capture. Not ported yet: Linux would record the default
//! sink's monitor through PipeWire, as Sunshine does. Until then the host
//! reports why the controller hears nothing instead of sending silence.
use crate::media::audio::sender::BLOCK;
use anyhow::{Result, bail};
use std::{sync::Arc, time::Instant};

pub(crate) type Samples = Arc<dyn Fn([f32; BLOCK * 2], Instant) + Send + Sync>;
const UNSUPPORTED: &str = "Linux 被控端暂不支持桌面音频";

pub(crate) enum Capture {}
impl Capture {
    pub fn producing(&self) -> bool {
        match *self {}
    }
    pub fn poll(&mut self) -> Result<()> {
        match *self {}
    }
}
pub(crate) fn open(endpoint: &Endpoint, _samples: Samples) -> Result<Capture> {
    match endpoint.never {}
}

pub(crate) struct Endpoint {
    pub id: String,
    never: std::convert::Infallible,
}
impl Endpoint {
    pub fn name(&self) -> String {
        match self.never {}
    }
}
#[derive(Default)]
pub(crate) struct Change {
    pub refresh: bool,
    pub rebuild: bool,
}
pub(crate) enum Devices {}
impl Devices {
    pub fn new() -> Result<Self> {
        bail!(UNSUPPORTED)
    }
    pub fn changes(&self, _selected: Option<&str>) -> Change {
        match *self {}
    }
    pub fn endpoint(&self, _selected: Option<&str>) -> Result<Option<Endpoint>> {
        match *self {}
    }
    pub fn list(&self) -> Result<Vec<Endpoint>> {
        match *self {}
    }
}
