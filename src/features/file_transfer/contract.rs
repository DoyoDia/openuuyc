//! File wire semantics shared by both roles. No platform or version dispatch.
//! RPC correlation answers a request; TaskId owns a transfer within a connection.
use super::{WIRE, protocol::*};
use anyhow::{Context, Result, ensure};
use prost::Message as _;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Correlation(Option<i64>);
impl Correlation {
    pub fn new(id: i64) -> Self {
        Self(Some(id))
    }
    pub fn assigned(self) -> Option<i64> {
        self.0.filter(|id| *id != 0)
    }
    fn wire(self) -> Option<Header> {
        Some(Header {
            id: self.0.unwrap_or(0),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Operation,
    Transfer,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Send,
    Receive,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Start(Direction),
    Transfer,
    Operation,
}

#[derive(Clone, Copy, PartialEq, prost::Message)]
pub(crate) struct Capabilities {
    #[prost(bool, tag = "1")]
    pub compressed: bool,
    #[prost(bool, tag = "2")]
    pub speedy: bool,
}
impl Capabilities {
    pub fn from_levels(ftp: i32, ftp2: i32) -> Self {
        Self {
            compressed: ftp >= 2,
            speedy: ftp2 >= 2,
        }
    }
}

pub(crate) enum Payload {
    Request(Req),
    Response(Res),
}
impl Payload {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Request(Req::ReceiveRequest(_)) => Kind::Start(Direction::Receive),
            Self::Request(Req::SendRequest(_)) => Kind::Start(Direction::Send),
            Self::Request(
                Req::FileAsk(_) | Req::FileBlock(_) | Req::Complete(_) | Req::DirSizeRead(_),
            )
            | Self::Response(
                Res::ReceiveResponse(_)
                | Res::SendResponse(_)
                | Res::FileConfirm(_)
                | Res::BlockConfirm(_)
                | Res::Result(_),
            ) => Kind::Transfer,
            _ => Kind::Operation,
        }
    }
    pub fn task(&self) -> Option<&TaskId> {
        match self {
            Self::Request(Req::ReceiveRequest(v)) => v.id.as_ref(),
            Self::Request(Req::SendRequest(v)) => v.id.as_ref(),
            Self::Request(Req::FileAsk(v)) => v.id.as_ref(),
            Self::Request(Req::FileBlock(v)) => v.id.as_ref(),
            Self::Request(Req::Complete(v)) => v.id.as_ref(),
            Self::Request(Req::DirSizeRead(v)) => v.id.as_ref(),
            Self::Response(Res::ReceiveResponse(v)) => v.id.as_ref(),
            Self::Response(Res::SendResponse(v)) => v.id.as_ref(),
            Self::Response(Res::FileConfirm(v)) => v.id.as_ref(),
            Self::Response(Res::BlockConfirm(v)) => v.id.as_ref(),
            Self::Response(Res::Result(v)) => v.id.as_ref(),
            _ => None,
        }
    }
}
pub(crate) struct Message {
    pub header: Correlation,
    pub payload: Payload,
}
impl Message {
    pub fn is_directory(&self) -> bool {
        matches!(self.payload, Payload::Request(Req::ReadDir(_)))
    }
    pub fn decode(bytes: &[u8]) -> Result<Option<Self>> {
        ensure!(bytes.len() < WIRE, "文件消息过大");
        if bytes.first() == Some(&b'{') {
            return Ok(None);
        }
        let (header, payload) = match Envelope::decode(bytes)?.which {
            Some(EnvelopeKind::Request(Request {
                header,
                which: Some(RequestKind::File(v)),
            })) => (header, v.which.map(Payload::Request)),
            Some(EnvelopeKind::Response(Response {
                header,
                which: Some(ResponseKind::File(v)),
            })) => (header, v.which.map(Payload::Response)),
            _ => return Ok(None),
        };
        let payload = payload.context("文件RPC没有受支持的消息体")?;
        if payload.kind() != Kind::Operation {
            ensure!(payload.task().is_some(), "文件传输消息缺少TaskId");
        }
        Ok(Some(Self {
            header: Correlation(header.map(|h| h.id)),
            payload,
        }))
    }
}
pub(crate) fn request(header: Correlation, value: Req) -> Envelope {
    Envelope {
        which: Some(EnvelopeKind::Request(Request {
            header: header.wire(),
            which: Some(RequestKind::File(FileTransferFtpRequest {
                which: Some(value),
            })),
        })),
    }
}
pub(crate) fn response(header: Correlation, value: Res) -> Envelope {
    Envelope {
        which: Some(EnvelopeKind::Response(Response {
            header: header.wire(),
            which: Some(ResponseKind::File(FileTransferFtpResponse {
                which: Some(value),
            })),
        })),
    }
}

pub(crate) fn file_id(
    id: &Option<TaskId>,
    task: i32,
    index: Option<i32>,
    confirmation: bool,
) -> Result<i32> {
    let id = id.as_ref().context("文件消息缺少TaskId")?;
    ensure!(id.task_id == task, "文件任务编号不符");
    ensure!(
        index.is_none_or(|index| id.file_index == index || (confirmation && id.file_index == 0)),
        "文件索引与当前任务阶段不符"
    );
    Ok(id.file_index)
}
// A zero timestamp is unspecified. Size zero still means an empty file.
pub(crate) fn metadata(info: &FileInfo, ask: &FileTransferAsk) -> Result<()> {
    ensure!(
        info.size == ask.file_size
            && (ask.last_modified == 0 || info.modified_time == ask.last_modified),
        "文件与清单不一致"
    );
    Ok(())
}
pub(crate) fn received_bytes(position: u64, length: usize, size: u64) -> Result<u64> {
    let end = position
        .checked_add(length as u64)
        .context("文件位置溢出")?;
    ensure!(end <= size, "文件块超过声明长度");
    Ok(end)
}
pub(crate) fn collision_policy(value: i32) -> Result<i32> {
    match value {
        0 => Ok(2),
        1..=4 => Ok(value),
        _ => anyhow::bail!("未知同名策略：{value}"),
    }
}

/// Only an outstanding block can advance progress. A zero reported length is
/// unspecified, never a zero-byte acknowledgement of an arbitrary block.
#[derive(Default)]
pub(crate) struct PendingBlocks(HashMap<i32, usize>);
impl PendingBlocks {
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn insert(&mut self, id: i32, size: usize) -> Result<()> {
        ensure!(!self.0.contains_key(&id), "文件块编号重复");
        self.0.insert(id, size);
        Ok(())
    }
    pub fn confirm(&mut self, value: &FileTransferBlockConfirm) -> Result<usize> {
        super::success(value.err)?;
        let size = *self
            .0
            .get(&value.block_id)
            .context("未知或重复的文件块确认")?;
        ensure!(
            value.block_len == 0 || usize::try_from(value.block_len).ok() == Some(size),
            "文件块确认长度不符"
        );
        self.0.remove(&value.block_id);
        Ok(size)
    }
}
