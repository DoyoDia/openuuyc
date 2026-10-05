//! Wire-specific reply validation. Editor state only observes completed effects.
use super::super::wire::PbRpcRequestPayload;
use super::*;
use anyhow::Context as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Applied,
    NotLoggedIn,
    Locked,
    Rejected,
    Unknown(i32),
}
impl From<i32> for Outcome {
    fn from(code: i32) -> Self {
        match code {
            0 => Self::Applied,
            2 => Self::NotLoggedIn,
            3 => Self::Locked,
            4 => Self::Rejected,
            other => Self::Unknown(other),
        }
    }
}
impl Outcome {
    pub(super) fn message(self) -> String {
        match self {
            Self::NotLoggedIn => "被控端未登录，批注已关闭".into(),
            Self::Locked => "被控端已锁定，请解锁后清空或重新开启批注".into(),
            Self::Rejected => "远端未完成批注操作，请清空或重新开启批注".into(),
            Self::Unknown(code) => format!("批注操作未完成（{code}），请清空或重新开启"),
            Self::Applied => unreachable!("successful operation has no error message"),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DrawKind {
    Stroke,
    Clear,
    Toggle,
}
impl DrawKind {
    fn request(request: &PbDrawRequest) -> Option<Self> {
        Some(match request.payload.as_ref()? {
            PbDrawRequestKind::Stroke(_) => Self::Stroke,
            PbDrawRequestKind::Clear(_) => Self::Clear,
            PbDrawRequestKind::Toggle(_) => Self::Toggle,
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Reply {
    Official(DrawKind),
    Native,
}
pub(super) struct InFlight {
    pub effect: Pending,
    pub reply: Reply,
    pub sent: Instant,
}
impl Annotation {
    pub(in crate::features::stream_control) fn response(
        &mut self,
        seq: i64,
        response: PbDrawResponse,
    ) {
        let Some(payload) = response.payload else {
            return;
        };
        let (kind, code) = match payload {
            PbDrawResponseKind::Stroke(r) => (DrawKind::Stroke, r.error_code),
            PbDrawResponseKind::Clear(r) => (DrawKind::Clear, r.error_code),
            PbDrawResponseKind::Toggle(r) => (DrawKind::Toggle, r.error_code),
        };
        if self
            .pending
            .get(&seq)
            .is_some_and(|p| p.reply == Reply::Official(kind))
        {
            self.complete(seq, code.into());
        }
    }
}
impl StreamControlHandle {
    pub(super) fn send_draw(
        &self,
        s: &mut StreamControlState,
        request: PbDrawRequest,
        pending: Pending,
    ) -> Result<i64> {
        if s.annotation.native_token.is_some() {
            return self.send_native_draw(
                s,
                crate::protocol::annotation::Command::new(
                    crate::protocol::annotation::Operation::Draw(request),
                ),
                pending,
            );
        }
        ensure_ready(s)?;
        if s.annotation.pending.len() >= MAX_PENDING {
            bail!("批注回执仍在等待，请稍后再试");
        }
        let kind = DrawKind::request(&request).context("批注请求缺少操作")?;
        let seq = s.next_sequence;
        s.next_sequence = seq.wrapping_add(1);
        let payload = encode_envelope(
            seq,
            PbPayload::RpcRequest(
                PbRpcRequest {
                    request_header: Some(PbRequestHeader { request_id: seq }),
                    payload: Some(PbRpcRequestPayload::Draw(request)),
                }
                .encode_to_vec(),
            ),
        );
        self.enqueue_annotation(s, seq, payload, pending, Reply::Official(kind))
    }
    pub(super) fn enqueue_annotation(
        &self,
        s: &mut StreamControlState,
        seq: i64,
        payload: Vec<u8>,
        effect: Pending,
        reply: Reply,
    ) -> Result<i64> {
        s.annotation.pending.insert(
            seq,
            InFlight {
                effect,
                reply,
                sent: Instant::now(),
            },
        );
        let outgoing = OutgoingControlMessage {
            sequence: seq,
            payload,
            protocol: protocol(s),
            completion: None,
            annotation_generation: Some(s.annotation.generation),
        };
        if self.outgoing.send(outgoing).is_err() {
            s.annotation.uncertain("批注发送任务已停止".into());
            bail!("批注发送任务已停止");
        }
        Ok(seq)
    }
}
