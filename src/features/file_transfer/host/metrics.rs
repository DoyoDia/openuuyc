//! File-page settings are resolved in the same Windows user context as file IO.
use super::*;
use prost::Message as _;

#[derive(prost::Message)]
struct Message {
    #[prost(int64, tag = "1")]
    sequence: i64,
    #[prost(int64, tag = "2")]
    timestamp: i64,
    #[prost(message, optional, tag = "13")]
    metrics: Option<Metrics>,
}
#[derive(prost::Message)]
struct Metrics {
    #[prost(int32, repeated, tag = "1")]
    requested: Vec<i32>,
    #[prost(message, optional, tag = "4")]
    setting: Option<Setting>,
}
#[derive(prost::Message)]
struct Setting {
    #[prost(string, tag = "5")]
    directory: String,
    #[prost(int32, tag = "10")]
    control_allowed: i32,
}

fn message(bytes: &[u8]) -> Result<Option<Message>> {
    ensure!(bytes.len() < WIRE, "文件设置消息过大");
    if bytes.first() == Some(&b'{') {
        return Ok(None);
    }
    // Inspect only field keys/lengths: this also runs on FILE blocks, whose
    // contents must not be copied or decoded again just to test for settings.
    let mut fields = bytes;
    let mut payload = None;
    while !fields.is_empty() {
        let (tag, wire) = prost::encoding::decode_key(&mut fields)?;
        if (3..=28).contains(&tag) {
            payload = Some(tag);
        }
        prost::encoding::skip_field(wire, tag, &mut fields, Default::default())?;
    }
    // Honor the envelope oneof, including when another payload follows tag 13.
    if payload != Some(13) {
        return Ok(None);
    }
    Ok(Some(Message::decode(bytes)?))
}
pub(super) fn requested(bytes: &[u8]) -> Result<bool> {
    Ok(message(bytes)?
        .and_then(|m| m.metrics)
        .is_some_and(|m| m.setting.is_none() && m.requested.contains(&2)))
}
pub(super) fn reply(bytes: &[u8], allowed: bool) -> Result<Option<Packet>> {
    let Some(mut message) = message(bytes)? else {
        return Ok(None);
    };
    if !message
        .metrics
        .as_ref()
        .is_some_and(|m| m.setting.is_none() && m.requested.contains(&2))
    {
        return Ok(None);
    }
    let directory = if allowed {
        filesystem::directory(":/Default")?
            .to_string_lossy()
            .into_owned()
    } else {
        String::new()
    };
    message.metrics = Some(Metrics {
        // The official producer lists which metrics are present in its reply.
        requested: vec![2],
        setting: Some(Setting {
            directory,
            control_allowed: if allowed { 2 } else { 1 },
        }),
    });
    message.sequence = 0;
    message.timestamp = 0;
    tracing::debug!(allowed, "host file settings response prepared");
    Ok(Some(Packet {
        data: message.encode_to_vec(),
        file: false,
    }))
}
pub(super) fn denied(bytes: &[u8]) -> Result<bool> {
    Ok(message(bytes)?
        .and_then(|m| m.metrics)
        .and_then(|m| m.setting)
        .is_some_and(|s| s.control_allowed == 1))
}
