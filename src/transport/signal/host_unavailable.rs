//! The host session type the signaling loop names, where the host role is not
//! built. The type is uninhabited: the device room is still kept alive for
//! presence, but no controlled-connection session can exist to feed it events,
//! so every branch that would hold one is unreachable.
use super::SignalSession;
use anyhow::Result;

pub(crate) enum Session {}

impl Session {
    pub(crate) async fn next(&mut self) -> std::convert::Infallible {
        match *self {}
    }

    pub(crate) async fn apply(
        &mut self,
        event: std::convert::Infallible,
        _signal: &mut SignalSession,
    ) -> Result<()> {
        match event {}
    }

    pub(crate) async fn event(
        &mut self,
        _event: &str,
        _args: &[serde_json::Value],
        _binary: &[Vec<u8>],
        _signal: &mut SignalSession,
    ) -> Result<()> {
        match *self {}
    }

    pub(crate) async fn signaling_restored(&mut self, _signal: &mut SignalSession) -> Result<()> {
        match *self {}
    }

    pub(crate) async fn close(self) {
        match self {}
    }
}
