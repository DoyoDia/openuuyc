//! Authenticated controlled-session input ownership.
mod backend;
// The installed Windows service's input agent, reached over a named pipe.
#[cfg(windows)]
pub(crate) mod broker;
pub(crate) mod config;
#[cfg(windows)]
mod engine;
#[cfg(target_os = "linux")]
#[path = "engine_linux.rs"]
mod engine;
mod geometry;
pub(crate) mod wire;

use super::{Lease, lock};
use anyhow::{Context, Result};
use backend::Backend;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub(crate) use geometry::{Geometry, Screen};

struct Envelope {
    generation: u64,
    event: wire::Event,
    geometry: Geometry,
}
#[derive(Default)]
struct Gate {
    stream: Option<u16>,
    binding: u64,
    generation: u64,
    faulted: bool,
    /// Why the last input was dropped, logged when it changes.
    refused: Option<&'static str>,
}
struct Shared {
    gate: Mutex<Gate>,
    lease: Lease,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    geometry: Box<dyn Fn() -> Geometry + Send + Sync>,
    ready: tokio::sync::watch::Sender<Option<u64>>,
    policy: wire::Policy,
    configuration: tokio::sync::watch::Sender<Arc<config::Configuration>>,
    mouse_policy: tokio::sync::watch::Sender<config::MousePolicy>,
}
#[derive(Clone)]
pub(crate) struct Receiver {
    shared: Arc<Shared>,
    tx: mpsc::SyncSender<Envelope>,
}
pub(crate) struct Session {
    receiver: Receiver,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    pub fn new(
        lease: Lease,
        cancel: CancellationToken,
        connected: Arc<AtomicBool>,
        policy: wire::Policy,
        geometry: impl Fn() -> Geometry + Send + Sync + 'static,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Envelope>(64);
        let shared = Arc::new(Shared {
            gate: Default::default(),
            lease,
            cancel,
            connected,
            geometry: Box::new(geometry),
            ready: tokio::sync::watch::channel(None).0,
            policy,
            configuration: tokio::sync::watch::channel(Arc::new(Default::default())).0,
            mouse_policy: tokio::sync::watch::channel(Default::default()).0,
        });
        let state = shared.clone();
        let thread = std::thread::Builder::new()
            .name("host-input".into())
            .spawn(move || run(state, rx))
            .context("无法启动被控输入线程")?;
        Ok(Self {
            receiver: Receiver { shared, tx },
            thread: Some(thread),
        })
    }
    pub fn receiver(&self) -> Receiver {
        self.receiver.clone()
    }
    pub fn close(&mut self) {
        self.receiver.shared.cancel.cancel();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::error!("host input thread panicked");
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}
impl Receiver {
    pub fn configure(&self, configuration: config::Configuration) {
        if self.shared.lease.requested() {
            self.shared
                .configuration
                .send_replace(Arc::new(configuration));
        }
    }
    pub fn mouse_policy(&self) -> tokio::sync::watch::Receiver<config::MousePolicy> {
        self.shared.mouse_policy.subscribe()
    }
    pub fn bind(&self, stream: u16) -> u64 {
        let mut gate = lock(&self.shared.gate);
        gate.binding = gate.binding.wrapping_add(1);
        gate.generation = gate.generation.wrapping_add(1);
        gate.stream = Some(stream);
        gate.faulted = false;
        self.shared.ready.send_replace(None);
        gate.binding
    }
    pub fn generation(&self, stream: u16) -> Option<u64> {
        let gate = lock(&self.shared.gate);
        (gate.stream == Some(stream)).then_some(gate.binding)
    }
    pub async fn ready(&self, stream: u16, generation: u64) -> bool {
        let mut ready = self.shared.ready.subscribe();
        loop {
            {
                let gate = lock(&self.shared.gate);
                if gate.stream != Some(stream) || gate.binding != generation {
                    return false;
                }
            }
            if *ready.borrow_and_update() == Some(generation) {
                return true;
            }
            tokio::select! {_=self.shared.cancel.cancelled()=>return false,result=ready.changed()=>{if result.is_err(){return false}}}
        }
    }
    pub fn close(&self, stream: u16, generation: u64) -> bool {
        let mut gate = lock(&self.shared.gate);
        if gate.stream == Some(stream) && gate.binding == generation {
            gate.stream = None;
            gate.generation = gate.generation.wrapping_add(1);
            self.shared.ready.send_replace(None);
            true
        } else {
            false
        }
    }
    pub fn transport_lost(&self) {
        let mut gate = lock(&self.shared.gate);
        gate.generation = gate.generation.wrapping_add(1);
        self.shared.ready.send_replace(None);
    }
    /// Return true for input envelopes, including rejected/unknown JSON actions.
    /// Never route malformed input into another business handler or log payloads.
    pub fn receive(&self, stream: u16, bytes: &[u8]) -> Result<bool> {
        let json = bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
        let parsed = if json {
            wire::json(bytes)
        } else {
            if bytes.len() > wire::MAX_MESSAGE {
                return Ok(false);
            }
            match crate::features::stream_control::publisher::input_event(bytes)? {
                Some(bytes) => wire::touch(&bytes),
                None => return Ok(false),
            }
        };
        let event = match parsed {
            Ok(Some(e)) => e,
            Ok(None) => return Ok(true),
            Err(error) => {
                self.shared
                    .lease
                    .input_status(None, Some(error.to_string()));
                return Ok(true);
            }
        };
        self.enqueue(Some(stream), event);
        Ok(true)
    }
    pub fn receive_action(&self, bytes: &[u8]) -> Result<bool> {
        if bytes.len() > wire::MAX_MESSAGE || bytes.first() == Some(&b'{') {
            return Ok(false);
        }
        let Some(event) = crate::features::stream_control::publisher::input_action(bytes)? else {
            return Ok(false);
        };
        self.enqueue(None, event);
        Ok(true)
    }
    fn enqueue(&self, stream: Option<u16>, event: wire::Event) {
        let mut gate = lock(&self.shared.gate);
        let refused = if gate.stream.is_none() {
            Some("no input channel bound")
        } else if stream.is_some_and(|id| gate.stream != Some(id)) {
            Some("input on an unbound channel")
        } else if gate.faulted {
            Some("input generation faulted")
        } else if !self.shared.permitted() {
            Some("input not permitted")
        } else {
            None
        };
        if gate.refused != refused {
            gate.refused = refused;
            match refused {
                Some(reason) => {
                    tracing::warn!(reason, ?stream, bound = ?gate.stream, "host input dropped")
                }
                None => tracing::debug!(?stream, "host input accepted"),
            }
        }
        if refused.is_some() {
            return;
        }
        let item = Envelope {
            generation: gate.generation,
            event,
            geometry: (self.shared.geometry)(),
        };
        if self.tx.try_send(item).is_err() {
            // A lost up cannot be repaired by dropping one event. Invalidate this
            // channel's complete input generation; reconnect establishes a new one.
            gate.faulted = true;
            gate.generation = gate.generation.wrapping_add(1);
            self.shared
                .lease
                .input_status(None, Some("输入队列已中断，请重新连接".into()));
        }
    }
}
impl Shared {
    fn permitted(&self) -> bool {
        !self.cancel.is_cancelled()
            && self.lease.requested()
            && self.connected.load(Ordering::Acquire)
    }
}
fn run(shared: Arc<Shared>, rx: mpsc::Receiver<Envelope>) {
    let mut engine: Option<Backend> = None;
    let mut generation = None;
    let mut require_service = false;
    let mut binding = None;
    let mut attempts = 0u8;
    let mut prepare_at = std::time::Instant::now();
    let mut configuration = shared.configuration.subscribe();
    let mut skipped = None;
    while !shared.cancel.is_cancelled() && shared.lease.requested() {
        let (current, current_binding) = {
            let gate = lock(&shared.gate);
            (
                (gate.stream.is_some() && shared.permitted()).then_some(gate.generation),
                gate.stream.map(|_| gate.binding),
            )
        };
        // A transport outage or failed service invalidates queued input, but must
        // not replenish the bounded recovery budget for this channel binding.
        if binding != current_binding {
            binding = current_binding;
            attempts = 0;
            prepare_at = std::time::Instant::now();
        }
        if current != generation {
            shared.mouse_policy.send_replace(Default::default());
            if let Some(engine) = &mut engine {
                if let Err(error) = engine.release() {
                    shared
                        .lease
                        .input_status(Some(engine.backend()), Some(error.to_string()));
                }
            }
            generation = current;
        }
        if let Some(current) = current {
            if engine.is_none() && attempts < 5 && std::time::Instant::now() >= prepare_at {
                attempts += 1;
                match Backend::new(shared.policy, require_service, || shared.permitted()) {
                    Ok(mut value) => {
                        require_service |= value.service();
                        let settings = (**configuration.borrow_and_update()).clone();
                        let configured = value.configure(settings);
                        if let Err(error) = configured {
                            shared
                                .lease
                                .input_status(Some(value.backend()), Some(error.to_string()));
                        }
                        engine = Some(value);
                        let mut gate = lock(&shared.gate);
                        if gate.generation == current {
                            gate.faulted = false;
                        }
                    }
                    Err(error) => {
                        prepare_at =
                            std::time::Instant::now() + Duration::from_secs(1u64 << (attempts - 1));
                        shared.lease.input_status(None, Some(error.to_string()));
                        let mut gate = lock(&shared.gate);
                        if shared.permitted() {
                            gate.faulted = true;
                        }
                    }
                }
            }
            let gate = lock(&shared.gate);
            if gate.generation == current && shared.permitted() {
                shared.ready.send_if_modified(|ready| {
                    if *ready == Some(gate.binding) {
                        false
                    } else {
                        *ready = Some(gate.binding);
                        true
                    }
                });
            }
        }
        let item = match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(item) => Some(item),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if configuration.has_changed().unwrap_or(false)
            && let Some(engine) = &mut engine
        {
            let settings = (**configuration.borrow_and_update()).clone();
            if let Err(error) = engine.configure(settings) {
                shared
                    .lease
                    .input_status(Some(engine.backend()), Some(error.to_string()));
            }
        }
        if let Some(engine) = &mut engine
            && generation.is_some()
        {
            if let Err(error) = engine.synchronize((shared.geometry)()) {
                let _ = engine.release();
                shared
                    .lease
                    .input_status(Some(engine.backend()), Some(error.to_string()));
            }
        }
        if let Some(item) = item {
            let stale = if generation != Some(item.generation) {
                Some("stale input generation")
            } else if !shared.permitted() {
                Some("input not permitted")
            } else if item.geometry != (shared.geometry)() {
                Some("screen layout changed")
            } else if engine.is_none() {
                Some("no input backend")
            } else {
                None
            };
            if stale != skipped {
                skipped = stale;
                if let Some(reason) = stale {
                    tracing::warn!(reason, "host input skipped");
                }
            }
            if stale.is_some() {
                continue;
            }
            let Some(engine) = engine.as_mut() else {
                continue;
            };
            // The gate is checked on every physical transition, including Unicode
            // batches; closing a channel must not finish an old queued sequence.
            let permitted = || {
                let gate = lock(&shared.gate);
                shared.permitted()
                    && !gate.faulted
                    && Some(gate.generation) == generation
                    && gate.stream.is_some()
            };
            let result = engine.apply(item.event, item.geometry, permitted);
            if let Err(error) = result {
                let release = engine.release();
                shared.lease.input_status(
                    Some(engine.backend()),
                    Some(match release {
                        Ok(()) => error.to_string(),
                        Err(release) => format!("{error}；释放输入失败：{release}"),
                    }),
                );
            } else {
                shared.lease.input_status(Some(engine.backend()), None);
            }
        }
        if let Some(engine) = &mut engine {
            if let Err(error) = engine.tick() {
                shared
                    .lease
                    .input_status(Some(engine.backend()), Some(error.to_string()));
                let _ = engine.release();
            }
            if generation.is_some() {
                let policy = engine.mouse_policy();
                shared.mouse_policy.send_if_modified(|current| {
                    if *current == policy {
                        false
                    } else {
                        *current = policy;
                        true
                    }
                });
            }
        }
        if engine.as_ref().is_some_and(|engine| !engine.healthy()) {
            engine.take();
            prepare_at =
                std::time::Instant::now() + Duration::from_secs(1u64 << attempts.saturating_sub(1));
            let mut gate = lock(&shared.gate);
            gate.generation = gate.generation.wrapping_add(1);
            gate.faulted = true;
            shared.ready.send_replace(None);
            generation = None;
        }
    }
    if let Some(mut engine) = engine {
        if let Err(error) = engine.release() {
            tracing::warn!(%error,"host input final release failed");
        }
    }
}
