//! One CONTROL business ingress shared by SCTP and mixed KCP.
//! Transport callbacks validate their binding, then use this same receiver.
//! Input remains synchronous; file and display execution stays with its owner.
use super::{ReportTarget, channels, screens};
use crate::features::host::{VideoConfig, capture, lock};
use crate::transport::uu_kcp::{ControlReceiver, UuKcpControl};
use anyhow::Context;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) struct ControlIngress {
    pub screen: Option<capture::Screen>,
    pub config: Arc<Mutex<VideoConfig>>,
    pub negotiated: Arc<crate::features::host::format::Negotiated>,
    pub input: crate::features::host::input::Receiver,
    pub files: crate::features::host::files::Receiver,
    pub screens: Arc<tokio::sync::Mutex<screens::Screens>>,
    pub report_target: ReportTarget,
    pub kcp: UuKcpControl,
    pub cancel: CancellationToken,
}
impl ControlIngress {
    pub fn start(self) -> (ControlReceiver, tokio::task::JoinHandle<()>) {
        let Self {
            screen,
            config,
            negotiated,
            input,
            files,
            screens,
            report_target,
            kcp,
            cancel,
        } = self;
        enum ControlWork {
            Response(Vec<u8>),
            CaptureSetting(Vec<u8>),
        }
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<(u16, u64, ControlWork)>();
        let control_screen = screen.clone();
        let control_config = config.clone();
        let control_stop = cancel.clone();
        let control_negotiated = negotiated.clone();
        let control_target = report_target.clone();
        let control_input = input.clone();
        let control_files = files.clone();
        let control_receiver: crate::transport::uu_kcp::ControlReceiver =
            Arc::new(move |stream_id, bytes| {
                if control_stop.is_cancelled() {
                    return Ok(());
                }
                let Some(generation) = control_input.generation(stream_id) else {
                    return Ok(());
                };
                if control_input.receive(stream_id, bytes)? {
                    return Ok(());
                }
                match control_files.receive_control(bytes) {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "host CONTROL file request rejected");
                        return Ok(());
                    }
                }
                if crate::features::stream_control::publisher::is_capture_setting(bytes)? {
                    // KCP delivery is synchronous. Reuse the response worker to
                    // serialize display/settings work without blocking ACK/input
                    // delivery or spawning an unordered task for each request.
                    control_tx
                        .send((
                            stream_id,
                            generation,
                            ControlWork::CaptureSetting(bytes.to_vec()),
                        ))
                        .context("host CONTROL setting queue closed")?;
                    return Ok(());
                }
                let responses = crate::features::stream_control::publisher::receive(
                    bytes,
                    true,
                    control_screen.as_ref(),
                    &mut lock(&control_config),
                    &control_negotiated,
                )?;
                if let Some(caps) = responses.files {
                    control_files.capabilities(caps);
                }
                control_target.send_if_modified(|routes| routes.received(&responses));
                for response in responses.messages {
                    control_tx
                        .send((stream_id, generation, ControlWork::Response(response)))
                        .context("host CONTROL response queue closed")?;
                }
                Ok(())
            });
        let control_kcp = kcp.clone();
        let control_stop = cancel.clone();
        let response_input = input.clone();
        let control_screens = screens.clone();
        let control_target = report_target.clone();
        let control_responses = tokio::spawn(async move {
            loop {
                let item = tokio::select! { _=control_stop.cancelled()=>break, item=control_rx.recv()=>item };
                let Some((id, generation, work)) = item else {
                    break;
                };
                if !response_input.ready(id, generation).await {
                    continue;
                }
                let (messages, setting_response) = match work {
                    ControlWork::Response(bytes) => (vec![bytes], false),
                    ControlWork::CaptureSetting(bytes) => {
                        let mut state = tokio::select! {
                            _ = control_stop.cancelled() => break,
                            state = control_screens.lock() => state,
                        };
                        if response_input.generation(id) != Some(generation) {
                            continue;
                        }
                        match crate::features::stream_control::publisher::receive_session(
                            &mut state, &bytes, false,
                        )
                        .await
                        {
                            Ok(responses) => {
                                control_target
                                    .send_if_modified(|routes| routes.received(&responses));
                                (responses.messages, true)
                            }
                            Err(error) => {
                                tracing::warn!(%error, "host CONTROL capture setting failed");
                                continue;
                            }
                        }
                    }
                };
                for bytes in messages {
                    if !response_input.ready(id, generation).await || control_stop.is_cancelled() {
                        break;
                    }
                    let result = if setting_response {
                        channels::send_setting_response(&control_target, bytes).await
                    } else {
                        let channel = control_target
                            .borrow()
                            .control
                            .as_ref()
                            .and_then(std::sync::Weak::upgrade)
                            .filter(|channel| channel.id() == id);
                        let Some(channel) = channel else {
                            continue;
                        };
                        // Reply via the negotiated carrier, never replay across carriers.
                        control_kcp.send_control(&channel, bytes).await
                    };
                    if let Err(error) = result {
                        tracing::warn!(%error,"host CONTROL response failed");
                    }
                }
            }
        });
        (control_receiver, control_responses)
    }
}
