//! Visible remote-access indicator, driven by the actual host session.
use super::DeviceCenterApp;
use crate::ui::chrome::TitleBarAlert;

impl DeviceCenterApp {
    pub(super) fn controlled_caption(&self) -> Option<TitleBarAlert> {
        caption(
            &self.host.as_ref()?.status(),
            self.devices.as_ref(),
            self.catalog.as_ref(),
        )
    }
}

pub(super) fn caption(
    status: &crate::features::host::Status,
    devices: Option<&crate::account::api::DeviceList>,
    catalog: Option<&super::catalog::Catalog>,
) -> Option<TitleBarAlert> {
    let Some(connection) = &status.connection else {
        return status.connected.then(|| TitleBarAlert {
            source: if status.assistance {
                "正在接受远程协助"
            } else {
                "正在被远程访问"
            }
            .into(),
            duration: "已连接".into(),
        });
    };
    let seconds = connection.elapsed_seconds?;
    if !status.session_active && !connection.observation_lost {
        return None;
    }
    // Prefer the server participant matching this signaling client, never
    // the first participant or another device with the same alias.
    let participant = devices.and_then(|l| {
        l.current_device.participants_info.iter().find(|p| {
            p.get("client_id").and_then(serde_json::Value::as_str)
                == Some(connection.client_id.as_str())
        })
    });
    let name = participant
        .and_then(|p| p.get("alias"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            catalog.and_then(|c| {
                c.groups
                    .entries()
                    .find(|(_, d)| d.device_id == connection.device_id)
                    .map(|(_, d)| d.alias.as_str())
            })
        });
    let name = name
        .map(clean_name)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if connection.device_id.is_empty() {
                "远程设备".into()
            } else {
                format!(
                    "设备 {}",
                    connection
                        .device_id
                        .chars()
                        .rev()
                        .take(6)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<String>()
                )
            }
        });
    let kind = if connection.observation_lost {
        "被控状态确认中"
    } else if !status.connected {
        "连接暂时中断"
    } else if status.assistance {
        "远程协助"
    } else {
        "自有设备"
    };
    Some(TitleBarAlert {
        source: format!("{kind} · {name}"),
        duration: if connection.observation_lost {
            format!("上次已连接 {}", duration(seconds))
        } else {
            format!("已连接 {}", duration(seconds))
        },
    })
}

fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(64)
        .collect::<String>()
        .trim()
        .to_owned()
}

pub(super) fn duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}
