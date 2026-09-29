//! Session-owned hot-unplug fallback. Native mutations remain in displays::Session.
use super::*;
use std::time::{Duration, Instant};

pub(super) fn same_source(a: &capture::Screen, b: &capture::Screen) -> bool {
    a.id == b.id && a.identity.is_some() && a.identity == b.identity
}

#[derive(Default)]
pub(super) struct State {
    empty_since: Option<Instant>,
    attempted: bool,
    pub(super) recovery: Option<Recovery>,
}
pub(super) struct Recovery {
    slot: usize,
    original: capture::Screen,
    temporary: i32,
    automatic: bool,
    returning: Option<Instant>,
    retry_at: Instant,
    probe: Option<Probe>,
}
struct Probe {
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<capture::Screen>>>,
}
impl Probe {
    fn start(screen: capture::Screen, lease: Lease, cancel: CancellationToken) -> Self {
        let task_cancel = cancel.clone();
        let task = tokio::task::spawn_blocking(move || {
            ensure!(
                lease.requested() && !task_cancel.is_cancelled(),
                "屏幕恢复已取消"
            );
            let _runtime = crate::features::host::encoder::Runtime::new()?;
            let mut desktop = capture::Desktop::open_selected(&screen)?;
            let deadline = Instant::now() + Duration::from_secs(2);
            while lease.requested() && !task_cancel.is_cancelled() && Instant::now() < deadline {
                if desktop.next(20, 2, false, false, (1280, 720))?.is_some() && desktop.available {
                    ensure!(
                        same_source(&desktop.screen, &screen),
                        "恢复期间屏幕身份变化"
                    );
                    return Ok(desktop.screen.clone());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            anyhow::bail!("原显示器尚未恢复可采集画面")
        });
        Self {
            cancel,
            task: Some(task),
        }
    }
    async fn close(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.cancel.cancel();
        // The closure owns its capture resources and checks cancellation. During
        // normal close we join it; abnormal drop still arranges owned cleanup.
        if let Some(task) = self.task.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = task.await;
                });
            }
        }
    }
}
impl State {
    pub(super) fn knows_screen(&self, id: i32) -> bool {
        self.recovery
            .as_ref()
            .is_some_and(|r| id == r.original.id || id == r.temporary)
    }
    pub(super) fn owns_slot(&self, slot: usize) -> bool {
        self.recovery.as_ref().is_some_and(|r| r.slot == slot)
    }
    pub(super) fn choose(&mut self, id: i32) {
        if let Some(r) = &mut self.recovery {
            // Controllers can echo our newly published fallback selection.
            if id != r.temporary && !(r.returning.is_some() && id == r.original.id) {
                r.automatic = false;
                r.returning = None;
                if let Some(probe) = &r.probe {
                    probe.cancel.cancel();
                }
            }
        }
    }
    pub(super) fn stop(&mut self, id: i32) {
        if let Some(r) = &mut self.recovery {
            if id == -1 || id == r.temporary || (r.returning.is_some() && id == r.original.id) {
                r.automatic = false;
                r.returning = None;
                if let Some(probe) = &r.probe {
                    probe.cancel.cancel();
                }
            }
        }
    }
    fn should_create(&mut self, empty: bool, now: Instant) -> bool {
        if !empty {
            self.empty_since = None;
            self.attempted = false;
            return false;
        }
        let since = *self.empty_since.get_or_insert(now);
        if self.attempted || now.saturating_duration_since(since) < Duration::from_millis(200) {
            return false;
        }
        self.attempted = true;
        true
    }
    pub(super) async fn close(&mut self) {
        if let Some(mut recovery) = self.recovery.take() {
            if let Some(probe) = &mut recovery.probe {
                probe.close().await;
            }
        }
    }
}

impl Screens {
    fn delivered(&self, index: usize) -> bool {
        self.slots
            .get(index)
            .and_then(|s| s.worker.as_ref())
            .is_some_and(media::Worker::has_frame)
    }
    pub(super) async fn maintain_headless(&mut self) -> Result<()> {
        if self.headless.recovery.is_none() {
            if lock(&self.reports.catalog).is_empty() && !self.headless.attempted {
                self.headless.empty_since.get_or_insert_with(Instant::now);
                // Confirm the empty topology before publishing ScreenSources.
                // Otherwise a controller could close its last tab in the gap.
                tokio::select! { _ = self.cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(Duration::from_millis(200)) => {} }
                self.refresh()?;
            }
            if !self
                .headless
                .should_create(lock(&self.reports.catalog).is_empty(), Instant::now())
            {
                return Ok(());
            }
            let current = self.reports.current.load(Ordering::Acquire);
            let candidate = self
                .slots
                .iter()
                .enumerate()
                .filter(|(index, slot)| self.registered.contains(index) && slot.awaiting_source)
                .filter_map(|(index, slot)| slot.suspended.clone().map(|s| (index, s)))
                .min_by_key(|(_, s)| s.id != current);
            let Some((index, original)) = candidate else {
                return Ok(());
            };
            self.lease
                .update(true, true, "显示器已断开，正在准备临时虚拟屏…");
            let Some(temporary) = self
                .displays
                .create_temporary(original.clone())
                .await
                .context(
                    "没有可用显示器，临时虚拟屏无法创建；请检查虚拟显示驱动或重新连接显示器",
                )?
            else {
                return Ok(());
            };
            // Record ownership before starting media: failure must still retain
            // a cleanup owner and must never create another monitor next tick.
            self.headless.recovery = Some(Recovery {
                slot: index,
                original,
                temporary: temporary.id,
                automatic: true,
                returning: None,
                retry_at: Instant::now(),
                probe: None,
            });
            self.refresh()?;
            let config = *lock(&self.slots[index].config);
            self.start_at(index, temporary, config).await?;
            return Ok(());
        }

        // Temporarily take the state so transitions may borrow the screen pool.
        // Always put it back on error; the display journal remains the final owner.
        let mut recovery = self.headless.recovery.take().unwrap();
        let result = self.advance_headless(&mut recovery, Instant::now()).await;
        match result {
            Ok(true) => {
                if let Some(probe) = &mut recovery.probe {
                    probe.close().await;
                }
                self.headless.empty_since = None;
                self.headless.attempted = false;
                Ok(())
            }
            result => {
                self.headless.recovery = Some(recovery);
                result.map(|_| ())
            }
        }
    }
    async fn advance_headless(&mut self, r: &mut Recovery, now: Instant) -> Result<bool> {
        let temporary = self.info(r.temporary).ok().map(|i| i.screen);
        let original = self
            .info(r.original.id)
            .ok()
            .map(|i| i.screen)
            .filter(|screen| same_source(screen, &r.original));
        if !r.automatic {
            if let Some(probe) = &mut r.probe {
                probe.close().await;
            }
            r.probe = None;
            // QuitSuper must still be able to restore its saved running layout.
            if self.before_super.is_some() {
                return Ok(false);
            }
            // Do not destroy a manually retained/paused fallback screen. A new
            // selection must actually deliver a frame before automatic cleanup.
            if self.active_slot(r.temporary).is_none()
                && self
                    .slots
                    .iter()
                    .enumerate()
                    .any(|(i, _)| self.delivered(i))
            {
                return self.remove_temporary(r.temporary).await.map(|_| true);
            }
            return Ok(false);
        }
        if let Some(started) = r.returning {
            if !self.connected.load(Ordering::Acquire) || !self.registered.contains(&r.slot) {
                r.returning = Some(now);
                return Ok(false);
            }
            if original.is_some()
                && self.slots[r.slot]
                    .screen
                    .as_ref()
                    .is_some_and(|s| same_source(s, &r.original))
                && self.delivered(r.slot)
            {
                self.remove_temporary(r.temporary).await?;
                tracing::info!("original display resumed; temporary display removed");
                return Ok(true);
            }
            let ended = self.slots[r.slot]
                .worker
                .as_ref()
                .is_none_or(media::Worker::ended);
            if original.is_none()
                || ended
                || (self.connected.load(Ordering::Acquire)
                    && now.duration_since(started) >= Duration::from_secs(8))
            {
                r.returning = None;
                r.retry_at = now + Duration::from_secs(10);
                if let Some(temporary) = temporary {
                    let config = *lock(&self.slots[r.slot].config);
                    self.start_at(r.slot, temporary, config).await?;
                }
            }
            return Ok(false);
        }
        if let Some(probe) = &mut r.probe {
            if original.is_none() {
                probe.cancel.cancel();
            }
            if probe.task.as_ref().is_some_and(|t| t.is_finished()) {
                let outcome = probe.task.take().unwrap().await;
                r.probe = None;
                r.retry_at = now + Duration::from_secs(10);
                let outcome = outcome.context("屏幕恢复检查中断")?;
                if let Ok(screen) = outcome {
                    if original.as_ref().is_some_and(|s| same_source(s, &screen)) {
                        r.returning = Some(now);
                        let config = *lock(&self.slots[r.slot].config);
                        self.start_at(r.slot, screen, config).await?;
                    }
                }
            }
            return Ok(false);
        }
        if !self.registered.contains(&r.slot) || !self.connected.load(Ordering::Acquire) {
            return Ok(false);
        }
        // Keep fallback playback healthy while a returning physical display is
        // still failing its preflight. A present-but-unusable original must not
        // starve recovery of the usable temporary capture.
        if self.slots[r.slot].worker.is_none() && now >= r.retry_at {
            if let Some(temporary) = temporary {
                r.retry_at = now + Duration::from_secs(10);
                let config = *lock(&self.slots[r.slot].config);
                self.start_at(r.slot, temporary, config).await?;
                return Ok(false);
            }
        }
        if let Some(original) = original {
            if now >= r.retry_at {
                r.probe = Some(Probe::start(
                    original,
                    self.lease.clone(),
                    self.cancel.child_token(),
                ));
            }
        }
        Ok(false)
    }
    async fn remove_temporary(&mut self, id: i32) -> Result<()> {
        if let Some(identity) = self.displays.temporary_identity(id)? {
            self.displays.remove(identity).await?;
        }
        for slot in &mut self.slots {
            if slot.suspended.as_ref().is_some_and(|s| s.id == id) {
                slot.suspended = None;
                slot.awaiting_source = false;
            }
        }
        self.refresh()
    }
}
