//! Publisher business-channel routing and observed source reporting.
use super::super::lock;
use super::{ReportRoutes, ReportTarget};
use anyhow::Result;
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

pub(super) async fn bind_channel(
    channel: Arc<RTCDataChannel>,
    screens: Arc<tokio::sync::Mutex<super::screens::Screens>>,
    cancel: CancellationToken,
    kcp: crate::transport::uu_kcp::UuKcpControl,
    report_target: ReportTarget,
    input: crate::features::host::input::Receiver,
    microphone: crate::features::host::microphone::Receiver,
) {
    let control = channel.label() == "CONTROL_DATA_CHANNEL";
    let text = channel.label() == "TEXT_DATA_CHANNEL";
    if !control && !text {
        return;
    }
    let input_generation = if control {
        Some(input.bind(channel.id()))
    } else {
        None
    };
    let (handshake_source, handshake_config, handshake_caps) = {
        let state = screens.lock().await;
        (
            state.handshake_source(),
            state.slots[0].config.clone(),
            state.slots[0].negotiated.clone(),
        )
    };
    tracing::info!(
        channel = channel.label(),
        stream_id = channel.id(),
        "host business channel bound"
    );
    if control {
        kcp.set_control_stream(channel.id(), true);
    }
    let weak = Arc::downgrade(&channel);
    let opening = cancel.clone();
    let opening_kcp = kcp.clone();
    let close_kcp = kcp.clone();
    let close_channel = Arc::downgrade(&channel);
    let closing_target = report_target.clone();
    let closing_input = input.clone();
    let closing_microphone = microphone.clone();
    channel.on_close(Box::new(move || {
        if control {
            if let Some(channel) = close_channel.upgrade() {
                if closing_input.close(channel.id(), input_generation.unwrap()) {
                    close_kcp.set_control_stream(channel.id(), false);
                }
            }
        }
        closing_target.send_if_modified(|routes| {
            let current = if control {
                &mut routes.control
            } else {
                &mut routes.text
            };
            if current
                .as_ref()
                .is_some_and(|target| target.ptr_eq(&close_channel))
            {
                if !control {
                    closing_microphone.transport_lost();
                }
                *current = None;
                routes.revision = routes.revision.wrapping_add(1);
                true
            } else {
                false
            }
        });
        Box::pin(async {})
    }));
    let opening_target = report_target.clone();
    let opening_input = input.clone();
    channel.on_open(Box::new(move || {
        Box::pin(async move {
            let Some(channel) = weak.upgrade() else {
                return;
            };
            if opening.is_cancelled() {
                return;
            }
            if control && opening_input.generation(channel.id()) != input_generation {
                return;
            }
            opening_target.send_modify(|routes| {
                if control {
                    routes.control = Some(Arc::downgrade(&channel));
                } else {
                    routes.text = Some(Arc::downgrade(&channel));
                }
                routes.revision = routes.revision.wrapping_add(1);
            });
            if text {
                return;
            }
            if !opening_input
                .ready(channel.id(), input_generation.unwrap())
                .await
                || opening.is_cancelled()
            {
                return;
            }
            tracing::info!(channel = channel.label(), "host business channel opened");
            let bytes = crate::features::stream_control::publisher::echo(1, 0, true);
            let result = send_business(&channel, control, &opening_kcp, bytes).await;
            if let Err(error) = result {
                tracing::warn!(%error,control,"host opening business message failed");
            }
        })
    }));
    let weak = Arc::downgrade(&channel);
    channel.on_message(Box::new(move |message| {
        let weak = weak.clone();
        let screens = screens.clone();
        let handshake_source = handshake_source.clone();
        let handshake_config = handshake_config.clone();
        let handshake_caps = handshake_caps.clone();
        let cancel = cancel.clone();
        let kcp = kcp.clone();
        let report_target = report_target.clone();
        let input = input.clone();
        let microphone = microphone.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return;
            }
            let Some(channel) = weak.upgrade() else {
                return;
            };
            if text
                && !report_target
                    .borrow()
                    .text
                    .as_ref()
                    .is_some_and(|current| current.ptr_eq(&weak))
            {
                return;
            }
            if control {
                if input.generation(channel.id()) != input_generation {
                    return;
                }
                match input.receive(channel.id(), &message.data) {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error,"rejected malformed host input envelope");
                        return;
                    }
                }
            }
            if text {
                match input.receive_action(&message.data) {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error,"rejected malformed host input action");
                        return;
                    }
                }
                match crate::features::stream_control::publisher::microphone_policy(&message.data) {
                    Ok(Some(policy)) => {
                        let result = microphone.policy(policy.enabled).await;
                        if let Err(error) = &result {
                            tracing::warn!(%error,"host microphone request failed");
                        }
                        if let Some(channel) = weak.upgrade() {
                            if !cancel.is_cancelled() {
                                let response = policy.response(
                                    result
                                        .as_ref()
                                        .err()
                                        .map_or(0, crate::features::host::microphone::error_code),
                                );
                                if let Err(error) =
                                    channel.send_text_bytes(&Bytes::from(response)).await
                                {
                                    microphone.transport_lost();
                                    tracing::debug!(%error,"host microphone response failed");
                                }
                            }
                        }
                        return;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error,"invalid microphone envelope");
                        return;
                    }
                }
            }
            tracing::debug!(
                control,
                bytes = message.data.len(),
                tag = message.data.first().copied(),
                "host business message received"
            );
            let responses = if control {
                crate::features::stream_control::publisher::receive(
                    &message.data,
                    true,
                    &handshake_source,
                    &mut lock(&handshake_config),
                    &handshake_caps,
                )
            } else {
                crate::features::stream_control::publisher::receive_session(
                    &mut *screens.lock().await,
                    &message.data,
                    false,
                )
                .await
            };
            match responses {
                Ok(responses) => {
                    report_target.send_if_modified(|routes| routes.received(&responses));
                    if let Some(channel) = weak.upgrade() {
                        if control && !input.ready(channel.id(), input_generation.unwrap()).await {
                            return;
                        }
                        for response in responses.messages {
                            if cancel.is_cancelled() {
                                break;
                            }
                            let result = send_business(&channel, control, &kcp, response).await;
                            if let Err(error) = result {
                                tracing::warn!(%error,control,"host business response failed");
                            }
                        }
                    }
                }
                Err(error) => tracing::warn!(%error,"rejected malformed host business message"),
            }
        })
    }));
}

pub(super) async fn send_business(
    channel: &RTCDataChannel,
    control: bool,
    kcp: &crate::transport::uu_kcp::UuKcpControl,
    bytes: Vec<u8>,
) -> Result<usize> {
    if control {
        return kcp.send_control(channel, bytes).await;
    }
    Ok(channel.send_text_bytes(&Bytes::from(bytes)).await?)
}

pub(super) async fn publish_state(
    screens: Arc<tokio::sync::Mutex<super::screens::Screens>>,
    reports: Arc<super::screens::Reports>,
    transports: Vec<crate::features::host::transport::Transport>,
    mut target: tokio::sync::watch::Receiver<ReportRoutes>,
    cancel: CancellationToken,
    kcp: crate::transport::uu_kcp::UuKcpControl,
    initial: crate::features::host::capture::Screen,
    mut mouse_policy: tokio::sync::watch::Receiver<
        crate::features::host::input::config::MousePolicy,
    >,
    audio: super::super::audio::Audio,
    mut microphone: tokio::sync::watch::Receiver<super::super::microphone::Status>,
) {
    use crate::features::stream_control::publisher;
    use std::sync::atomic::Ordering;
    let mut changed = reports.changed.subscribe();
    let mut last_screen = None;
    let mut last_capture = None;
    let mut last_visible = None;
    let mut last_quality = None;
    let mut last_probe = None::<u32>;
    let mut last_locked = None;
    let mut last_mouse_policy = None;
    let mut last_microphone = None;
    let mut last_virtual_speaker = None;
    let mut revision = None;
    let mut secure_revision = None;
    let sequence = &reports.sequence;
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let refresh = tokio::select! {
            _=cancel.cancelled()=>break,
            result=changed.changed()=>{if result.is_err(){break;} false},
            result=target.changed()=>{if result.is_err(){break;} false},
            result=mouse_policy.changed()=>{if result.is_err(){break;} false},
            result=microphone.changed()=>{if result.is_err(){break;} false},
            _=timer.tick()=>true,
        };
        if refresh && let Ok(mut state) = screens.try_lock() {
            if let Err(error) = state.maintain().await {
                tracing::debug!(%error,"host screen inventory refresh failed");
            }
        }
        let routes = target.borrow_and_update().clone();
        if revision != Some(routes.revision) {
            revision = Some(routes.revision);
            last_screen = None;
            last_capture = None;
            last_visible = None;
            last_quality = None;
            last_probe = None;
            last_locked = None;
            last_mouse_policy = None;
            last_microphone = None;
            last_virtual_speaker = None;
        }
        if secure_revision != Some(routes.secure_revision) {
            secure_revision = Some(routes.secure_revision);
            last_locked = None;
        }
        let send = |control: bool, bytes: Vec<u8>| {
            let channel = if control {
                &routes.control
            } else {
                &routes.text
            }
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
            let cancel = &cancel;
            let kcp = &kcp;
            let sequence = &sequence;
            async move {
                let Some(channel) = channel else {
                    return false;
                };
                let bytes = match publisher::stamp_report(
                    &bytes,
                    sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                ) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(%error, "invalid host report");
                        return false;
                    }
                };
                let result = tokio::select! {
                    _=cancel.cancelled()=>return false,
                    result=send_business(&channel, control, kcp, bytes)=>result,
                };
                if let Err(error) = result {
                    tracing::debug!(%error, control, "host publication channel unavailable");
                    false
                } else {
                    true
                }
            }
        };
        let mic = microphone.borrow_and_update().clone();
        if mic.enabled && mic.error.is_none() {
            if last_virtual_speaker != Some(mic.speaker_active)
                && send(
                    false,
                    publisher::audio_device_event(if mic.speaker_active { 25 } else { 26 }),
                )
                .await
            {
                last_virtual_speaker = Some(mic.speaker_active);
            }
        } else {
            last_virtual_speaker = None;
        }
        let mic_state = (mic.enabled, mic.active, mic.error.clone());
        if last_microphone.as_ref() != Some(&mic_state) {
            let action = if mic.error.is_some() && mic.enabled {
                Some(27)
            } else if mic.enabled {
                Some(if mic.active { 23 } else { 24 })
            } else if last_microphone
                .as_ref()
                .is_some_and(|(enabled, _, _)| *enabled)
            {
                Some(24)
            } else {
                None
            };
            if action.is_none() || send(false, publisher::audio_device_event(action.unwrap())).await
            {
                last_microphone = Some(mic_state);
            }
        }
        // Native layout changes and capture restart form one screen transition.
        // Do not publish a half-restored catalog while that owner is awaiting I/O.
        let (media, catalog, requested) = {
            let _state = tokio::select! {
                _ = cancel.cancelled() => break,
                state = screens.lock() => state,
            };
            (
                reports.media(),
                lock(&reports.catalog).clone(),
                reports.current.load(Ordering::Acquire),
            )
        };
        let current = if catalog.iter().any(|s| s.screen.id == requested) {
            requested
        } else {
            catalog.first().map_or(-1, |s| s.screen.id)
        };
        let mapping = media
            .iter()
            .filter(|(_, state)| state.capturing)
            .map(|(index, state)| (state.screen.id, *index))
            .collect();
        let policy = mouse_policy.borrow_and_update().clone();
        if last_mouse_policy.as_ref() != Some(&policy) {
            if let Some(message) = policy.message() {
                if send(true, message).await {
                    last_mouse_policy = Some(policy);
                }
            } else {
                last_mouse_policy = Some(policy);
            }
        }
        let screen = publisher::screen_states(&catalog, &mapping, current);
        if last_screen.as_ref() != Some(&screen)
            && send(routes.control_screens, screen.clone()).await
        {
            tracing::debug!(current, ?mapping, "host screen catalog published");
            last_screen = Some(screen);
        }
        let selected = media
            .iter()
            .find(|(_, s)| s.capturing && s.screen.id == current)
            .or_else(|| media.iter().find(|(_, s)| s.capturing));
        let capture = selected.map(|(_, s)| s.screen.id);
        if last_capture != Some(capture)
            && send(
                false,
                publisher::capture_change(
                    selected.map_or(&initial, |(_, s)| &s.screen),
                    selected.is_some(),
                ),
            )
            .await
        {
            last_capture = Some(capture);
        }
        let visible = media.iter().any(|(_, s)| s.capturing && s.visible);
        let permission = (visible, audio.allowed());
        if last_visible != Some(permission)
            && send(false, publisher::permissions(visible, permission.1)).await
        {
            last_visible = Some(permission);
        }
        if routes.control.is_some()
            && let Some(locked) = crate::features::host::capture::session_locked()
            && last_locked != Some(locked)
            && send(true, publisher::secure_desktop(locked)).await
        {
            last_locked = Some(locked);
        }
        if let Some((index, state)) = selected
            && let Some(encoder) = state.encoder
            && state.quality > 0
        {
            let probe = transports[*index].automatic_rates().0;
            let key = (
                state.screen.id,
                state.quality,
                state.fps,
                state.encoder,
                state.capture.clone(),
                state.screen.width,
                state.screen.height,
            );
            if (last_quality.as_ref() != Some(&key)
                || last_probe.is_none_or(|old| old.abs_diff(probe) >= 102_400))
                && send(
                    false,
                    publisher::quality_report(
                        state.quality,
                        probe,
                        (state.screen.width, state.screen.height),
                        state.fps,
                        encoder,
                        &state.capture,
                    ),
                )
                .await
            {
                last_quality = Some(key);
                last_probe = Some(probe);
            }
        }
    }
}
