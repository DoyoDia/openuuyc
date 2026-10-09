//! Native diagnostic transfer over the existing low-priority BINARY channel.
//! No remote paths or arbitrary file requests are accepted.
mod controller;
mod host;
pub(crate) mod service;
use super::bundle;
use anyhow::{Result, ensure};
use bytes::Bytes;
pub(crate) use controller::{Controller, Phase, Snapshot};
pub(crate) use host::bind as bind_host;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

pub(crate) const CHANNEL: &str = "BINARY_DATA_CHANNEL";
const MAGIC: &[u8; 8] = b"OUDIAG01";
const CHUNK: usize = 32 * 1024;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
type Id = [u8; 16];
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Hello {
        allowed: bool,
    },
    Request,
    Progress(bundle::Progress),
    Begin {
        size: u64,
        sha256: String,
        logs: usize,
        warnings: usize,
    },
    Ack {
        offset: u64,
    },
    Finish,
    Cancel,
    Error {
        message: String,
    },
}
enum Packet {
    Control(Id, Message),
    Data(Id, u64, Vec<u8>),
}
fn decode(data: &[u8]) -> Result<Packet> {
    ensure!(
        data.len() >= 25 && data.len() <= CHUNK + 1024 && &data[..8] == MAGIC,
        "无效诊断消息"
    );
    let id = data[8..24].try_into().unwrap();
    match data[24] {
        0 => Ok(Packet::Control(id, serde_json::from_slice(&data[25..])?)),
        1 => {
            ensure!(
                data.len() > 33 && data.len() <= CHUNK + 33,
                "无效诊断数据块"
            );
            Ok(Packet::Data(
                id,
                u64::from_le_bytes(data[25..33].try_into().unwrap()),
                data[33..].to_vec(),
            ))
        }
        _ => anyhow::bail!("未知诊断消息"),
    }
}
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}
async fn send(channel: &RTCDataChannel, id: Id, message: Message) -> Result<()> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend(id);
    bytes.push(0);
    bytes.extend(serde_json::to_vec(&message)?);
    // Vendored SCTP reserves the whole message before assigning its sequence;
    // cancellation while waiting for capacity cannot leave a reliable SSN gap.
    tokio::time::timeout(TIMEOUT, channel.send(&Bytes::from(bytes))).await??;
    Ok(())
}
async fn send_data(channel: &RTCDataChannel, id: Id, offset: u64, data: &[u8]) -> Result<()> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend(id);
    bytes.push(1);
    bytes.extend(offset.to_le_bytes());
    bytes.extend(data);
    tokio::time::timeout(TIMEOUT, channel.send(&Bytes::from(bytes))).await??;
    Ok(())
}
async fn next(
    rx: &mut tokio::sync::mpsc::Receiver<Packet>,
    cancel: &CancellationToken,
) -> Result<Packet> {
    tokio::select! {
        _=cancel.cancelled()=>anyhow::bail!("诊断连接已关闭"),
        packet=tokio::time::timeout(TIMEOUT, rx.recv())=>packet?.ok_or_else(||anyhow::anyhow!("诊断通道已关闭")),
    }
}
