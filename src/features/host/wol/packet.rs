//! Official WoL payload and a bounded, local-subnet send plan.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddrV4};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Target {
    pub mac: [u8; 6],
    pub ip: Ipv4Addr,
    pub prefix: u8,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LanInfo {
    pub mac: String,
    pub inner_ip: String,
    pub subnet_mask: String,
}
pub(crate) fn mac(value: &str) -> Result<[u8; 6]> {
    ensure!(value.len() <= 17, "MAC长度无效");
    let text = value.replace([':', '-'], "");
    ensure!(
        text.len() == 12 && text.bytes().all(|c| c.is_ascii_hexdigit()),
        "MAC格式无效"
    );
    let mut result = [0; 6];
    for (i, part) in result.iter_mut().enumerate() {
        *part = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)?;
    }
    ensure!(result != [0; 6] && result[0] & 1 == 0, "目标必须是单播MAC");
    Ok(result)
}
pub(crate) fn mask(prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0))
}
pub(crate) fn valid_ip(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && !ip.is_link_local()
}
impl Target {
    pub fn parse(push: &serde_json::Value) -> Result<Option<Self>> {
        if push.get("type").and_then(|v| v.as_str()) != Some("wake_on_lan") {
            return Ok(None);
        }
        let target = push
            .get("data")
            .and_then(|v| v.get("target_device"))
            .filter(|v| v.is_object())
            .ok_or_else(|| anyhow::anyhow!("唤醒请求缺少目标"))?;
        let field = |name| {
            target
                .get(name)
                .and_then(|v| v.as_str())
                .filter(|s| s.len() <= 64)
                .ok_or_else(|| anyhow::anyhow!("唤醒请求网络字段无效"))
        };
        let ip: Ipv4Addr = field("inner_ip")?.parse()?;
        let mask_value: Ipv4Addr = field("subnet_mask")?.parse()?;
        let bits = u32::from(mask_value);
        let prefix = bits.leading_ones() as u8;
        ensure!(
            (1..=30).contains(&prefix) && mask(prefix) == mask_value && valid_ip(ip),
            "唤醒请求IP或子网掩码无效"
        );
        let host = u32::from(ip) & !bits;
        ensure!(host != 0 && host != !bits, "目标不是子网主机地址");
        Ok(Some(Self {
            mac: mac(field("mac")?)?,
            ip,
            prefix,
        }))
    }
    pub fn matches(&self, ip: Ipv4Addr, prefix: u8) -> bool {
        prefix == self.prefix
            && (u32::from(ip) & u32::from(mask(prefix)))
                == (u32::from(self.ip) & u32::from(mask(prefix)))
    }
    pub fn packet(&self) -> [u8; 102] {
        let mut data = [0xff; 102];
        for part in data[6..].chunks_exact_mut(6) {
            part.copy_from_slice(&self.mac);
        }
        data
    }
    pub fn destinations(&self) -> [SocketAddrV4; 6] {
        let broadcast = Ipv4Addr::from(u32::from(self.ip) | !u32::from(mask(self.prefix)));
        [
            SocketAddrV4::new(Ipv4Addr::BROADCAST, 0),
            SocketAddrV4::new(Ipv4Addr::BROADCAST, 7),
            SocketAddrV4::new(Ipv4Addr::BROADCAST, 9),
            SocketAddrV4::new(broadcast, 0),
            SocketAddrV4::new(broadcast, 7),
            SocketAddrV4::new(broadcast, 9),
        ]
    }
}
