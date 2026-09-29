//! Negotiated OpenUUYC extension on TEXT; no official UU opcode is repurposed.
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Message {
    Hello {
        revision: u64,
        kbps: u32,
        effective_bps: u32,
    },
    Set {
        request: u64,
        kbps: u32,
    },
    Applied {
        request: u64,
        revision: u64,
        kbps: u32,
        effective_bps: u32,
    },
    Rejected {
        request: u64,
        message: String,
    },
}
#[derive(Serialize, Deserialize)]
struct Envelope {
    openuuyc_audio: u32,
    #[serde(flatten)]
    message: Message,
}
pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Message>> {
    if bytes.len() > 2048 || bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    if value.get("openuuyc_audio").and_then(|v| v.as_u64()) != Some(1) {
        return Ok(None);
    }
    Ok(Some(serde_json::from_value::<Envelope>(value)?.message))
}
pub(crate) fn encode(message: Message) -> Vec<u8> {
    serde_json::to_vec(&Envelope {
        openuuyc_audio: 1,
        message,
    })
    .expect("audio control is serializable")
}
