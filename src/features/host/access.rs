//! Device access policy, account lifetime and individual connection leases.
use super::{capture, lock, settings};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Status {
    pub ready: bool,
    pub connected: bool,
    pub session_active: bool,
    pub message: String,
    pub error: Option<String>,
    pub settings_error: Option<String>,
    pub input_backend: Option<String>,
    pub input_error: Option<String>,
    #[serde(default)]
    pub audio: super::audio::Status,
    #[serde(default)]
    pub microphone: super::microphone::Status,
    pub saving: bool,
    pub encoder: Option<(i32, (u32, u32))>,
    pub capture: Option<String>,
    pub screen: Option<capture::Screen>,
    pub video: Option<ActiveEncoding>,
    pub streams: std::collections::BTreeMap<usize, StreamStatus>,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct StreamStatus {
    pub encoder: Option<(i32, (u32, u32))>,
    pub capture: Option<String>,
    pub screen: Option<capture::Screen>,
    pub video: Option<ActiveEncoding>,
}

fn update_media_summary(status: &mut Status) {
    let current = status
        .streams
        .values()
        .find(|s| s.video.is_some())
        .or_else(|| status.streams.values().next());
    status.encoder = current.and_then(|s| s.encoder);
    status.capture = current.and_then(|s| s.capture.clone());
    status.screen = current.and_then(|s| s.screen.clone());
    status.video = current.and_then(|s| s.video);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ActiveEncoding {
    pub backend: super::format::Backend,
    pub adapter: u64,
    pub format: super::format::Format,
    pub size: (u32, u32),
    pub fps: u32,
    pub target_bps: u32,
}

#[derive(Clone)]
pub(crate) struct AccessRequest {
    generation: u64,
}

struct Ownership {
    permission: u64,
    settings_revision: u64,
    encoding: super::EncodingSettings,
    audio_device: watch::Sender<Option<super::audio::Device>>,
    audio_defaults: super::audio::DefaultDevices,
    audio_quality: crate::media::audio::encoder::Quality,
    audio_inventory: super::audio::Inventory,
    media: u64,
    stream_generations: std::collections::BTreeMap<usize, u64>,
    allowed: bool,
    active: bool,
    dirty: bool,
    last_controlled: Option<std::time::Instant>,
    remote_action: Option<bool>,
    status: Status,
}

#[derive(Clone)]
pub(crate) struct Handle {
    desired: watch::Sender<Option<AccessRequest>>,
    ownership: Arc<Mutex<Ownership>>,
    store: Option<settings::Store>,
    saving: Arc<tokio::sync::Mutex<()>>,
    media: Arc<super::desktop::Cache>,
    scope: String,
    account: String,
}

impl Default for Handle {
    fn default() -> Self {
        Self {
            desired: watch::channel(None).0,
            ownership: Arc::new(Mutex::new(Ownership {
                permission: 0,
                settings_revision: 0,
                encoding: Default::default(),
                audio_device: watch::channel(None).0,
                audio_defaults: Default::default(),
                audio_quality: Default::default(),
                audio_inventory: Default::default(),
                media: 0,
                stream_generations: Default::default(),
                allowed: false,
                active: true,
                dirty: false,
                last_controlled: None,
                remote_action: None,
                status: Status::default(),
            })),
            store: None,
            saving: Arc::new(tokio::sync::Mutex::new(())),
            media: Arc::default(),
            scope: String::new(),
            account: String::new(),
        }
    }
}

fn finish_session(state: &mut Ownership) {
    if state.status.connected {
        state.last_controlled = Some(std::time::Instant::now());
    }
    state.status.connected = false;
    state.status.session_active = false;
    state.status.encoder = None;
    state.status.capture = None;
    state.status.screen = None;
    state.status.video = None;
    state.status.input_backend = None;
    state.status.input_error = None;
    state.status.audio = Default::default();
    state.status.microphone = Default::default();
    state.status.streams.clear();
    state.stream_generations.clear();
}

impl Handle {
    pub(crate) fn bind_account(&mut self, account: String) {
        self.account = account;
    }
    pub(crate) fn take_remote_action(&self) -> Option<bool> {
        lock(&self.ownership).remote_action.take()
    }
    pub(crate) async fn apply_remote(
        &self,
        snapshot: crate::platform::host_service::resident::Snapshot,
    ) {
        self.media.replace(snapshot.capabilities).await;
        let mut state = lock(&self.ownership);
        if !state.active {
            return;
        }
        if state.dirty {
            return;
        }
        state.allowed = snapshot.allowed;
        state.encoding = snapshot.encoding;
        state.audio_device.send_replace(snapshot.audio_device);
        state.audio_defaults = snapshot.audio_defaults;
        state.audio_quality = snapshot.audio_quality;
        state.status = snapshot.status;
    }
    pub(crate) fn remote_failed(&self, error: String) {
        let mut state = lock(&self.ownership);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.error = Some(error);
        state.status.message = "后台连接中断".into();
    }
    pub(crate) async fn prepare_startup(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        if crate::platform::host_service::resident::managed() {
            return Ok(());
        }
        tokio::task::spawn_blocking(super::displays::recovery::recover_abandoned).await??;
        let screens = capture::screens()?;
        let Some(screen) = screens
            .iter()
            .find(|s| s.primary)
            .or(screens.first())
            .cloned()
        else {
            return Ok(());
        };
        let owner = self.clone();
        self.media
            .prepare(screen, move || {
                !cancel.is_cancelled() && lock(&owner.ownership).active
            })
            .await?;
        Ok(())
    }
    pub(crate) async fn invalidate_media(&self) {
        self.media.invalidate().await;
    }
    pub(crate) fn load(account: &str, device: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut handle = Self::default();
        handle.scope = format!("{:x}", Sha256::digest(format!("{account}\0{device}")));
        let result = settings::Store::new(account, device).and_then(|store| {
            handle.store = Some(store.clone());
            store.load()
        });
        match result {
            Ok((allowed, encoding, audio_device, audio_defaults, audio_quality)) => {
                handle.set_allowed(allowed);
                let mut state = lock(&handle.ownership);
                state.encoding = encoding;
                state.audio_device.send_replace(audio_device);
                state.audio_defaults = audio_defaults;
                state.audio_quality = audio_quality;
                state.dirty = false;
                state.status.saving = false;
            }
            Err(error) => lock(&handle.ownership).status.settings_error = Some(error.to_string()),
        }
        handle
    }
    pub(crate) fn allowed(&self) -> bool {
        lock(&self.ownership).allowed
    }
    pub(crate) fn audio_device(&self) -> Option<super::audio::Device> {
        lock(&self.ownership).audio_device.borrow().clone()
    }
    pub(crate) fn audio_defaults(&self) -> super::audio::DefaultDevices {
        lock(&self.ownership).audio_defaults
    }
    pub(crate) fn audio_quality(&self) -> crate::media::audio::encoder::Quality {
        lock(&self.ownership).audio_quality
    }
    pub(crate) fn set_audio_quality(
        &self,
        quality: crate::media::audio::encoder::Quality,
    ) -> anyhow::Result<()> {
        quality.validate()?;
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.audio_quality != quality {
            state.audio_quality = quality;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn set_audio_defaults(
        &self,
        selected: super::audio::DefaultDevices,
    ) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.audio_defaults != selected {
            state.audio_defaults = selected;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn set_audio_device(
        &self,
        device: Option<super::audio::Device>,
    ) -> anyhow::Result<()> {
        if let Some(device) = &device {
            device.validate()?;
        }
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        let changed = *state.audio_device.borrow() != device;
        if changed {
            state.audio_device.send_replace(device);
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn audio_devices(&self) -> super::audio::Inventory {
        lock(&self.ownership).audio_inventory.clone()
    }
    pub(crate) fn request_audio_devices(&self) -> bool {
        let mut state = lock(&self.ownership);
        let list = &mut state.audio_inventory;
        if list.pending
            || list
                .updated
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(2))
        {
            return false;
        }
        list.pending = true;
        true
    }
    pub(crate) fn audio_devices_failed(&self) {
        let mut state = lock(&self.ownership);
        state.audio_inventory.pending = false;
        state.audio_inventory.updated = Some(std::time::Instant::now());
        state.audio_inventory.error = Some("读取播放设备失败".into());
    }
    pub(crate) async fn refresh_audio_devices(&self) {
        let result =
            tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<super::audio::Device>> {
                let devices = crate::platform::loopback::Devices::new()?;
                let mut result: Vec<_> = devices
                    .list()?
                    .into_iter()
                    .map(|device| super::audio::Device {
                        name: device.name(),
                        id: device.id,
                    })
                    .collect();
                result.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
                Ok(result)
            })
            .await;
        let mut state = lock(&self.ownership);
        let list = &mut state.audio_inventory;
        list.pending = false;
        list.updated = Some(std::time::Instant::now());
        match result {
            Ok(Ok(devices)) => {
                list.devices = devices;
                list.error = None;
            }
            Ok(Err(error)) => list.error = Some(format!("读取播放设备失败：{error:#}")),
            Err(_) => list.error = Some("读取播放设备任务中断".into()),
        }
    }
    pub(crate) fn encoding_settings(&self) -> super::EncodingSettings {
        lock(&self.ownership).encoding
    }
    pub(crate) fn set_encoding_settings(
        &self,
        encoding: super::EncodingSettings,
    ) -> anyhow::Result<()> {
        encoding.validate()?;
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.encoding != encoding {
            state.encoding = encoding;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn capabilities(&self) -> Option<Arc<super::desktop::Capabilities>> {
        self.media.snapshot()
    }
    pub(crate) fn set_allowed(&self, allowed: bool) {
        let mut state = lock(&self.ownership);
        if !state.active || state.allowed == allowed {
            return;
        }
        state.allowed = allowed;
        state.settings_revision = state.settings_revision.wrapping_add(1);
        state.permission = state.permission.wrapping_add(1);
        state.dirty = true;
        finish_session(&mut state);
        state.status.ready = false;
        state.status.error = None;
        state.status.settings_error = None;
        state.status.saving = true;
        state.status.message = if allowed {
            "正在启用被控…"
        } else {
            "已禁止被控"
        }
        .into();
        self.desired.send_replace(allowed.then_some(AccessRequest {
            generation: state.permission,
        }));
    }
    pub(crate) async fn persist_settings(&self) {
        let _saving = self.saving.lock().await;
        loop {
            let (revision, allowed, encoding, audio_device, audio_defaults, audio_quality) = {
                let state = lock(&self.ownership);
                if !state.dirty {
                    return;
                }
                (
                    state.settings_revision,
                    state.allowed,
                    state.encoding,
                    state.audio_device.borrow().clone(),
                    state.audio_defaults,
                    state.audio_quality,
                )
            };
            let result = if crate::platform::host_service::resident::managed() {
                crate::platform::host_service::resident::request(
                    crate::platform::host_service::resident::Request::Settings {
                        account: self.account.clone(),
                        allowed,
                        encoding,
                        audio_device,
                        audio_defaults,
                        audio_quality,
                    },
                )
                .await
                .map(|_| ())
            } else if let Some(store) = self.store.clone() {
                tokio::task::spawn_blocking(move || {
                    store.save(
                        allowed,
                        encoding,
                        audio_device,
                        audio_defaults,
                        audio_quality,
                    )
                })
                .await
                .map_err(|_| anyhow::anyhow!("被控设置保存任务中断"))
                .and_then(|r| r)
            } else {
                Err(anyhow::anyhow!("被控设置存储不可用"))
            };
            let mut state = lock(&self.ownership);
            if state.settings_revision != revision {
                continue;
            }
            state.dirty = false;
            state.status.saving = false;
            state.status.settings_error = result.err().map(|e| e.to_string());
            return;
        }
    }
    pub(crate) fn retry(&self) {
        if crate::platform::host_service::resident::managed() {
            lock(&self.ownership).remote_action = Some(true);
            return;
        }
        let mut state = lock(&self.ownership);
        if !state.active || !state.allowed {
            return;
        }
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.error = None;
        state.status.message = "正在启用被控…".into();
        self.desired.send_replace(Some(AccessRequest {
            generation: state.permission,
        }));
    }
    pub(crate) fn retire(&self) {
        let mut state = lock(&self.ownership);
        state.active = false;
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.message = "本机离线".into();
        self.desired.send_replace(None);
    }
    pub(crate) fn room_closed(&self) {
        let mut state = lock(&self.ownership);
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.message = "本机离线".into();
        self.desired
            .send_replace((state.active && state.allowed).then_some(AccessRequest {
                generation: state.permission,
            }));
    }
    pub(crate) fn disconnect(&self) {
        if crate::platform::host_service::resident::managed() {
            lock(&self.ownership).remote_action = Some(false);
            return;
        }
        let mut state = lock(&self.ownership);
        if !state.status.session_active {
            return;
        }
        state.media = state.media.wrapping_add(1);
        finish_session(&mut state);
        state.status.message = "等待连接".into();
    }
    pub(crate) fn requested(&self) -> bool {
        let state = lock(&self.ownership);
        state.active && state.allowed
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<AccessRequest>> {
        self.desired.subscribe()
    }
    pub(crate) fn status(&self) -> Status {
        lock(&self.ownership).status.clone()
    }
    pub(crate) fn update(&self, ready: bool, connected: bool, message: impl Into<String>) {
        Self::update_locked(&mut lock(&self.ownership), ready, connected, message.into());
    }
    fn update_locked(state: &mut Ownership, ready: bool, connected: bool, message: String) {
        if connected || state.status.connected {
            state.last_controlled = Some(std::time::Instant::now());
        }
        state.status.ready = ready;
        state.status.connected = connected;
        state.status.message = message;
    }
    pub(crate) fn lease(&self, request: &AccessRequest) -> Lease {
        Lease {
            handle: self.clone(),
            permission: request.generation,
            media: None,
            track: 0,
            stream_generation: None,
            stop: None,
        }
    }
    pub(crate) fn last_controlled_interval(&self) -> i64 {
        lock(&self.ownership)
            .last_controlled
            .map_or(-1, |at| at.elapsed().as_secs().min(i64::MAX as u64) as i64)
    }
}

#[derive(Clone)]
pub(crate) struct Lease {
    handle: Handle,
    permission: u64,
    media: Option<u64>,
    track: usize,
    stream_generation: Option<u64>,
    stop: Option<tokio_util::sync::CancellationToken>,
}

/// Unique connection owner. Callback clones only hold the checked lease.
pub(crate) struct SessionLease(Lease);
impl std::ops::Deref for SessionLease {
    type Target = Lease;
    fn deref(&self) -> &Lease {
        &self.0
    }
}
impl Drop for SessionLease {
    fn drop(&mut self) {
        self.0.finish();
    }
}
impl Lease {
    pub(crate) fn audio_quality(&self) -> crate::media::audio::encoder::Quality {
        self.handle.audio_quality()
    }
    pub(crate) fn audio_defaults(&self) -> super::audio::DefaultDevices {
        self.handle.audio_defaults()
    }
    pub(crate) fn audio_device_changes(&self) -> watch::Receiver<Option<super::audio::Device>> {
        lock(&self.handle.ownership).audio_device.subscribe()
    }
    pub(crate) fn with_cancellation(&self, stop: tokio_util::sync::CancellationToken) -> Self {
        let mut lease = self.clone();
        lease.stop = Some(stop);
        lease
    }
    pub(crate) fn display_scope(&self) -> &str {
        &self.handle.scope
    }
    pub(crate) fn begin_stream(&self, track: usize) -> anyhow::Result<Self> {
        let mut state = lock(&self.handle.ownership);
        anyhow::ensure!(self.current(&state), "本次被控许可已失效");
        let generation = state.stream_generations.entry(track).or_default();
        *generation = generation.wrapping_add(1);
        let mut lease = self.clone();
        lease.track = track;
        lease.stream_generation = Some(*generation);
        Ok(lease)
    }
    pub(crate) fn clear_stream(&self) {
        let mut state = lock(&self.handle.ownership);
        if self.current_owner(&state) {
            state.status.streams.remove(&self.track);
            let generation = state.stream_generations.entry(self.track).or_default();
            *generation = generation.wrapping_add(1);
            update_media_summary(&mut state.status);
        }
    }
    pub(crate) fn video(&self, encoding: Option<ActiveEncoding>) {
        self.modify(|state| {
            state.status.streams.entry(self.track).or_default().video = encoding;
            update_media_summary(&mut state.status);
        });
    }
    pub(crate) async fn prepare_media(
        &self,
        screen: capture::Screen,
    ) -> anyhow::Result<Arc<super::desktop::Capabilities>> {
        let lease = self.clone();
        self.handle
            .media
            .prepare(screen, move || lease.requested())
            .await
    }
    fn current(&self, state: &Ownership) -> bool {
        self.current_owner(state) && self.stop.as_ref().is_none_or(|stop| !stop.is_cancelled())
    }
    fn current_owner(&self, state: &Ownership) -> bool {
        state.active
            && state.allowed
            && self.permission == state.permission
            && self.media.is_none_or(|m| m == state.media)
            && self
                .stream_generation
                .is_none_or(|g| state.stream_generations.get(&self.track) == Some(&g))
    }
    fn modify(&self, update: impl FnOnce(&mut Ownership)) {
        let mut state = lock(&self.handle.ownership);
        if self.current(&state) {
            update(&mut state);
        }
    }
    pub(crate) fn peer_lease(&self) -> anyhow::Result<SessionLease> {
        let mut state = lock(&self.handle.ownership);
        anyhow::ensure!(self.current(&state), "本次被控许可已失效");
        state.media = state.media.wrapping_add(1);
        finish_session(&mut state);
        state.status.session_active = true;
        state.status.error = None;
        Ok(SessionLease(Self {
            handle: self.handle.clone(),
            permission: self.permission,
            media: Some(state.media),
            track: 0,
            stream_generation: None,
            stop: None,
        }))
    }
    pub(crate) fn finish(&self) {
        let mut state = lock(&self.handle.ownership);
        if self.current_owner(&state) {
            if self.media.is_some() {
                state.media = state.media.wrapping_add(1);
            }
            finish_session(&mut state);
            state.status.message = "等待连接".into();
        }
    }
    pub(crate) fn source(&self, screen: &capture::Screen, backend: &str) {
        self.modify(|state| {
            let stream = state.status.streams.entry(self.track).or_default();
            stream.capture = Some(backend.into());
            stream.screen = Some(screen.clone());
            update_media_summary(&mut state.status);
        });
    }
    pub(crate) fn encoder(
        &self,
        implementation: i32,
        maximum: (u32, u32),
        screen: &capture::Screen,
        capture: &str,
    ) {
        self.modify(|state| {
            let stream = state.status.streams.entry(self.track).or_default();
            stream.encoder = Some((implementation, maximum));
            stream.capture = Some(capture.into());
            stream.screen = Some(screen.clone());
            update_media_summary(&mut state.status);
        });
    }
    pub(crate) fn requested(&self) -> bool {
        self.current(&lock(&self.handle.ownership))
    }
    pub(crate) fn update(&self, ready: bool, connected: bool, message: impl Into<String>) {
        self.modify(|state| Handle::update_locked(state, ready, connected, message.into()));
    }
    pub(crate) fn frame(&self) {
        self.modify(|state| {
            state.status.error = None;
            state.status.message = "正在被远程访问".into();
        });
    }
    pub(crate) fn audio_status(&self, audio: super::audio::Status) {
        self.modify(|state| state.status.audio = audio);
    }
    pub(crate) fn microphone_status(&self, microphone: super::microphone::Status) {
        self.modify(|state| state.status.microphone = microphone);
    }
    pub(crate) fn input_status(&self, backend: Option<&str>, error: Option<String>) {
        self.modify(|state| {
            // Input failures otherwise only change a status field; record each
            // transition so a controller that "cannot move anything" can be
            // explained from the log.
            if state.status.input_backend.as_deref() != backend {
                tracing::info!(backend, "host input backend");
            }
            if state.status.input_error != error {
                match &error {
                    Some(error) => tracing::warn!(backend, %error, "host input failed"),
                    None => tracing::info!(backend, "host input recovered"),
                }
            }
            state.status.input_backend = backend.map(str::to_owned);
            state.status.input_error = error;
        });
    }
    pub(crate) fn recovery_failed(&self, error: impl Into<String>) {
        let mut state = lock(&self.handle.ownership);
        if self.media == Some(state.media) {
            state.status.error = Some(error.into());
        }
    }
    pub(crate) fn fail(&self, error: impl Into<String>) {
        self.modify(|state| state.status.error = Some(error.into()));
    }
}
