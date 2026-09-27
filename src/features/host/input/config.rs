//! Current product input configuration, separate from numeric platform enums.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Rule {
    pub mode: u8,
    pub simulation: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Configuration {
    pub keyboard_interrupts: bool,
    pub games: BTreeMap<String, Rule>,
}
impl Default for Configuration {
    fn default() -> Self {
        Self {
            keyboard_interrupts: true,
            games: Default::default(),
        }
    }
}
impl Configuration {
    pub fn from_response(
        mut configs: BTreeMap<String, crate::account::api::VersionedConfigure>,
    ) -> Result<Self> {
        let mut value = Self::default();
        let mut take = |name: &str| -> Result<Option<Value>> {
            let Some(entry) = configs.remove(name).filter(|entry| entry.status == 0) else {
                return Ok(None);
            };
            let Some(value) = entry.data else {
                return Ok(None);
            };
            let bytes = match value {
                Value::String(s) => s.into_bytes(),
                other => serde_json::to_vec(&other)?,
            };
            ensure!(bytes.len() <= 256 * 1024, "输入策略配置过大");
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| anyhow::anyhow!("输入策略配置无效"))
        };
        if let Some(config) = take("win_keylock_optimize")?
            && let Some(enabled) = config.get("enable").and_then(Value::as_bool)
        {
            value.keyboard_interrupts = enabled;
        }
        if let Some(config) = take("app_white_list")?
            && let Some(rows) = config.get("gamewhitelist").and_then(Value::as_array)
        {
            for row in rows {
                let Some(name) = row.get("filename").and_then(Value::as_str).filter(|name| {
                    !name.is_empty() && name.len() <= 512 && !name.contains(['\\', '/', '\0'])
                }) else {
                    continue;
                };
                // S5D3060 defaults an omitted input_mode to zero; S6AE5A0
                // rejects values outside 0/1/2 and normalizes them to mode 2.
                let mode = row.get("input_mode").and_then(Value::as_i64).unwrap_or(0);
                let mode = if (0..=2).contains(&mode) {
                    mode as u8
                } else {
                    2
                };
                let simulation = row
                    .get("simulation_mouse_switch")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    != 0;
                value
                    .games
                    .entry(name.to_lowercase())
                    .or_insert(Rule { mode, simulation });
            }
        }
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.games.len() <= 8192, "输入策略条目过多");
        for (name, rule) in &self.games {
            ensure!(
                !name.is_empty() && name.len() <= 512 && rule.mode <= 2,
                "输入策略条目无效"
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= 384 * 1024,
            "输入策略过大"
        );
        Ok(())
    }
}
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct MousePolicy {
    pub mode: Option<u8>,
    pub restore: Option<[f64; 2]>,
}
impl MousePolicy {
    pub fn message(&self) -> Option<Vec<u8>> {
        let mode = self.mode?;
        let mut value = serde_json::json!({"action":"special_game_mouse","mouse_mode":mode,"force_mode":mode!=2});
        if mode == 2
            && let Some([x, y]) = self.restore
        {
            value["coordinate_x_scale"] = x.into();
            value["coordinate_y_scale"] = y.into();
        }
        Some(serde_json::to_vec(&value).expect("validated finite mouse policy"))
    }
}
