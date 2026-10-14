//! 一行一个 JSON 的请求与应答。
//!
//! 请求里没有一个字节能指定「造哪块屏」：辅助程序只认分辨率，连接器名字由它自己挑、
//! 自己记，销毁时也必须报回它发出去的那个名字。这样客户端拿不到、也猜不出任何内核
//! 对象的名字，被利用的面就只有「活动会话里的用户能加虚拟屏」这一条。
use serde::{Deserialize, Serialize};

/// 分辨率上限与下限，和主程序校验虚拟显示模式时用的区间一致。
pub const MAX_DIMENSION: u32 = 16384;
pub const MIN_DIMENSION: u32 = 2;
pub const MAX_HZ: u32 = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// 要一块虚拟屏。只给尺寸，不给名字。
    Create { width: u32, height: u32, hz: u32 },
    /// 收回一块之前 `Create` 出来的屏。
    Destroy { connector: String },
    /// 问现在还能加几块、已经加了哪几块。诊断页和「是否可用」都用它。
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 内核给这块屏起的连接器名（`Virtual-N`），客户端拿它去指挥合成器。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

impl Reply {
    pub fn done() -> Self {
        Self {
            ok: true,
            error: None,
            connector: None,
            status: None,
        }
    }
    pub fn connector(connector: String) -> Self {
        Self {
            connector: Some(connector),
            ..Self::done()
        }
    }
    pub fn status(status: Status) -> Self {
        Self {
            status: Some(status),
            ..Self::done()
        }
    }
    pub fn failed(error: impl std::fmt::Display) -> Self {
        Self {
            ok: false,
            error: Some(format!("{error:#}")),
            connector: None,
            status: None,
        }
    }
    /// 把「成功」收成结果，失败时带上辅助程序给的原因。
    pub fn into_result(self) -> anyhow::Result<Self> {
        if self.ok {
            return Ok(self);
        }
        anyhow::bail!(
            "{}",
            self.error.unwrap_or_else(|| "辅助程序拒绝了请求".into())
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// 一次能同时给出的虚拟显示器数量，加满就得先撤一块。
    pub capacity: u32,
    /// 本连接（或本机）现在占用的连接器名。
    pub leased: Vec<String>,
}
