//! File RPC admission and TEXT/FILE ownership for the ordinary-user executor.
use super::{Lease, lock};
use crate::features::file_transfer::host::{self as executor, agent::Backend};
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::{RTCDataChannel, data_channel_state::RTCDataChannelState};
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Status {
    pub active: bool,
    pub error: Option<String>,
}
#[derive(Default)]
struct State {
    capabilities: executor::Capabilities,
    channels: HashMap<String, Weak<RTCDataChannel>>,
    generation: u64,
    incoming: Option<mpsc::Sender<(u64, Vec<u8>)>>,
    receiver: Option<mpsc::Receiver<(u64, Vec<u8>)>>,
}
#[derive(Clone)]
pub(crate) struct Receiver(Arc<Mutex<State>>);
impl Default for Receiver {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(32);
        Self(Arc::new(Mutex::new(State {
            incoming: Some(tx),
            receiver: Some(rx),
            ..Default::default()
        })))
    }
}
impl Receiver {
    pub fn capabilities(&self, capabilities: executor::Capabilities) {
        let mut state = lock(&self.0);
        if state.capabilities != capabilities {
            tracing::debug!(
                compressed = capabilities.compressed,
                speedy = capabilities.speedy,
                "host file capabilities negotiated"
            );
            state.capabilities = capabilities;
        }
    }
    pub fn bind(&self, channel: &Arc<RTCDataChannel>) {
        let mut s = lock(&self.0);
        if s.channels
            .get(channel.label())
            .is_some_and(|old| !old.ptr_eq(&Arc::downgrade(channel)))
        {
            s.generation = s.generation.wrapping_add(1);
        }
        s.channels
            .insert(channel.label().to_owned(), Arc::downgrade(channel));
    }
    pub fn close(&self, channel: &Arc<RTCDataChannel>) {
        let mut s = lock(&self.0);
        if s.channels
            .get(channel.label())
            .is_some_and(|old| old.ptr_eq(&Arc::downgrade(channel)))
        {
            s.channels.remove(channel.label());
            s.generation = s.generation.wrapping_add(1);
        }
    }
    // The caller validates the current CONTROL stream/input generation. iOS
    // sends file RPCs here, including through the synchronous mixed-KCP path.
    // Admit only file envelopes and never block ACK/input delivery on disk or IPC.
    pub fn receive_control(&self, bytes: &[u8]) -> Result<bool> {
        let decoded = executor::decode(bytes)?;
        if decoded.is_none() && !executor::settings_requested(bytes)? {
            return Ok(false);
        }
        let delivery = {
            let state = lock(&self.0);
            state.incoming.clone().map(|tx| (tx, state.generation))
        };
        if let Some((tx, generation)) = delivery {
            tx.try_send((generation, bytes.to_vec()))
                .map_err(|_| anyhow::anyhow!("文件接收队列已满或关闭"))?;
            if decoded.is_some_and(|v| v.is_directory()) {
                tracing::debug!(
                    channel = "CONTROL_DATA_CHANNEL",
                    bytes = bytes.len(),
                    "host directory request received"
                );
            }
        }
        Ok(true)
    }
    pub async fn receive(&self, channel: &Arc<RTCDataChannel>, bytes: &[u8]) -> Result<bool> {
        let decoded = executor::decode(bytes)?;
        if decoded.is_none() && !executor::settings_requested(bytes)? {
            return Ok(false);
        }
        if decoded.is_some_and(|v| v.is_directory()) {
            tracing::debug!(
                channel = channel.label(),
                bytes = bytes.len(),
                "host directory request received"
            );
        }
        let delivery = {
            let s = lock(&self.0);
            if !s
                .channels
                .get(channel.label())
                .is_some_and(|old| old.ptr_eq(&Arc::downgrade(channel)))
            {
                return Ok(true);
            }
            s.incoming.clone().map(|tx| (tx, s.generation))
        };
        if let Some((tx, generation)) = delivery {
            tx.send((generation, bytes.to_vec()))
                .await
                .map_err(|_| anyhow::anyhow!("文件接收队列已关闭"))?;
        }
        Ok(true)
    }
}
pub(super) async fn run(
    receiver: Receiver,
    lease: Lease,
    connected: Arc<AtomicBool>,
    cancel: CancellationToken,
    scope: String,
    sequence: Arc<std::sync::atomic::AtomicI64>,
) {
    let mut input = lock(&receiver.0)
        .receiver
        .take()
        .expect("file receiver has one owner");
    let mut backend: Option<(u64, Backend)> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pending: Option<(u64, Vec<u8>)> = None;
    loop {
        if cancel.is_cancelled() || !lease.requested() {
            break;
        }
        let (generation, ready) = {
            let s = lock(&receiver.0);
            (
                s.generation,
                ["TEXT_DATA_CHANNEL", "FILE_DATA_CHANNEL"].iter().all(|n| {
                    s.channels
                        .get(*n)
                        .and_then(Weak::upgrade)
                        .is_some_and(|c| c.ready_state() == RTCDataChannelState::Open)
                }),
            )
        };
        if backend
            .as_ref()
            .is_some_and(|(g, _)| *g != generation || !ready)
        {
            lease.file_notices(backend.take().unwrap().1.close().await);
            pending = None;
        }
        if backend.is_none() && ready && pending.is_some() {
            backend = Some((
                generation,
                Backend::new(scope.clone(), cancel.child_token()),
            ));
        }
        let allowed = lease.file_access() && connected.load(Ordering::Acquire);
        let revision = backend.as_ref().map_or(0, |(_, b)| {
            b.policy(allowed, lock(&receiver.0).capabilities)
        });
        let sender = backend.as_ref().map(|(_, b)| b.input.clone());
        if let Some((_, b)) = &backend {
            lease.file_notices(b.notices.take());
            lease.file_status(Status {
                active: allowed && b.ready.load(Ordering::Acquire),
                error: lock(&b.error).clone(),
            });
        }

        tokio::select! {
            _=cancel.cancelled()=>break,
            _=tick.tick()=>{},
            packet=input.recv(), if pending.is_none() && ready=>{
                let Some((g,packet))=packet else{break};
                if g==generation{pending=Some((if backend.is_some(){revision}else{u64::MAX},packet));}
            },
            permit=async{match &sender{Some(tx)=>tx.reserve().await,None=>std::future::pending().await}}, if pending.is_some()=>{
                let Ok(permit)=permit else{break};
                let packet=pending.take().unwrap();
                if packet.0==revision||packet.0==u64::MAX{permit.send((revision,packet.1));}
            },
            packet=async{match &mut backend{Some((_,b))=>b.output.recv().await,None=>std::future::pending().await}}=>{
                if let Some((packet_revision,packet))=packet {
                    if packet_revision!=revision {continue;}
                    if let Err(e)=send(&receiver,generation,packet,&cancel,&lease,&sequence).await{lease.file_status(Status{active:false,error:Some(e.to_string())});break;}
                } else {break;}
            }
        }
    }
    if let Some((_, b)) = backend {
        lease.file_notices(b.close().await);
    }
    lock(&receiver.0).incoming = None;
    lease.file_status(Status::default());
}
async fn send(
    receiver: &Receiver,
    generation: u64,
    packet: executor::Packet,
    cancel: &CancellationToken,
    lease: &Lease,
    sequence: &std::sync::atomic::AtomicI64,
) -> Result<()> {
    let rejection = executor::failure_reply(&packet.data)?;
    tracing::debug!(
        file_channel = packet.file,
        bytes = packet.data.len(),
        rejection,
        "host file response sending"
    );
    if !lease.file_access() && !rejection {
        return Ok(());
    }
    let channel = {
        let s = lock(&receiver.0);
        ensure!(s.generation == generation, "文件通道已替换");
        s.channels
            .get(if packet.file {
                "FILE_DATA_CHANNEL"
            } else {
                "TEXT_DATA_CHANNEL"
            })
            .and_then(Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("文件通道已关闭"))?
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while channel.buffered_amount().await + packet.data.len() > 2 * 1024 * 1024 {
        if !lease.file_access() && !rejection {
            return Ok(());
        }
        ensure!(
            !cancel.is_cancelled() && tokio::time::Instant::now() < deadline,
            "文件发送队列已取消或超时"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    ensure!(
        !cancel.is_cancelled() && channel.ready_state() == RTCDataChannelState::Open,
        "文件通道未就绪"
    );
    if !lease.file_access() && !rejection {
        return Ok(());
    }
    let data = packet.stamped(sequence.fetch_add(1, Ordering::Relaxed))?;
    let bytes = bytes::Bytes::from(data);
    tokio::time::timeout_at(deadline, async {
        if packet.file {
            channel.send(&bytes).await
        } else {
            channel.send_text_bytes(&bytes).await
        }
    })
    .await??;
    Ok(())
}
