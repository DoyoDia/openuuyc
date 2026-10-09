//! Resolve the save directory in the file executor; the session owns control permission.
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
    #[prost(int32, tag = "8")]
    capture_permission: i32,
    #[prost(int32, tag = "9")]
    operation_permission: i32,
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
        match filesystem::directory(":/Default") {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(error) => {
                // An unavailable user directory must not suppress the global
                // control report or masquerade as a session permission denial.
                tracing::warn!(%error, "host file save directory unavailable");
                String::new()
            }
        }
    } else {
        String::new()
    };
    tracing::debug!(
        file_allowed = allowed,
        directory_available = !directory.is_empty(),
        "host settings directory prepared"
    );
    message.metrics = Some(Metrics {
        // The official producer lists which metrics are present in its reply.
        requested: vec![2],
        setting: Some(Setting {
            directory,
            // This is the whole-session permission, not a file-access flag.
            // Only the network owner can fill it from the live session lease.
            control_allowed: 0,
            capture_permission: 0,
            operation_permission: 0,
        }),
    });
    message.sequence = 0;
    message.timestamp = 0;
    Ok(Some(Packet {
        data: message.encode_to_vec(),
        file: false,
    }))
}
pub(crate) struct Settings(Message);
impl Settings {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Self>> {
        Ok(message(bytes)?
            .filter(|m| m.metrics.as_ref().is_some_and(|v| v.setting.is_some()))
            .map(Self))
    }

    // Called only after the network owner checks that its session lease is current.
    pub(crate) fn authorized(mut self, file_allowed: bool) -> Packet {
        let setting = self.0.metrics.as_mut().unwrap().setting.as_mut().unwrap();
        setting.control_allowed = 2;
        // Windows grants these operations through the current host lease;
        // these are authorization states, not capture/HID readiness reports.
        setting.capture_permission = 2;
        setting.operation_permission = 2;
        if !file_allowed {
            setting.directory.clear();
        }
        Packet {
            data: self.0.encode_to_vec(),
            file: false,
        }
    }
}
