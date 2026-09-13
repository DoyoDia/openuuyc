//! Account-device power commands. See docs/official-device-power.md.
use crate::api::DeviceInfo;
use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PowerAction {
    Wake,
    Shutdown,
    Reboot,
}

impl PowerAction {
    pub const ALL: [Self; 3] = [Self::Wake, Self::Shutdown, Self::Reboot];

    pub fn label(self) -> &'static str {
        match self {
            Self::Wake => "开机",
            Self::Shutdown => "关机",
            Self::Reboot => "重启",
        }
    }

    pub fn check(self, device: &DeviceInfo) -> Result<()> {
        device.validated_device_id()?;
        if !matches!(device.platform, 1 | 4) || !device.controlled_support {
            bail!("该设备不支持电脑电源操作");
        }
        if !device.controllable {
            bail!("该设备未开放远程控制");
        }
        match self {
            Self::Wake => {
                if device.status != "DISCONNECTED" {
                    bail!("只有离线设备可以发送开机请求");
                }
                if !device.support_wol {
                    bail!("该设备未提供远程开机能力");
                }
            }
            Self::Shutdown | Self::Reboot => {
                // G's shipped feature matrix declares these for platform 1
                // starting at 1.0.0, but not for macOS (4). Cloud (51) excluded.
                if device.platform != 1 {
                    bail!("原版能力表未开放该平台的关机和重启");
                }
                let version = device.version_name.split('.').collect::<Vec<_>>();
                if version.len() < 3
                    || !version.iter().all(|part| part.parse::<u32>().is_ok())
                    || version[0].parse::<u32>().unwrap_or(0) < 1
                {
                    bail!("设备版本未知或不支持该电源操作，请刷新或更新被控端");
                }
                if !device.is_connected() {
                    bail!("只有在线设备可以关机或重启");
                }
            }
        }
        Ok(())
    }
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
pub(crate) struct PowerReceipt {
    pub current_device_in_same_network: Option<bool>,
    pub assist_count: Option<u32>,
    pub router_wol: Option<bool>,
}

impl PowerReceipt {
    pub fn summary(&self, action: PowerAction) -> String {
        if action != PowerAction::Wake {
            return "请求已受理，等待设备状态变化".into();
        }
        let mut routes = Vec::new();
        if self.current_device_in_same_network == Some(true) {
            routes.push("当前设备同网".to_owned());
        }
        if let Some(count) = self.assist_count {
            routes.push(format!("{count} 台协助设备"));
        }
        if self.router_wol == Some(true) {
            routes.push("路由器唤醒".to_owned());
        }
        if routes.is_empty() {
            "开机请求已受理，等待上线".into()
        } else {
            format!("开机请求已受理，等待上线 · {}", routes.join(" · "))
        }
    }
}
