//! Outgoing remote assistance and server-synchronized saved devices.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;

pub fn normalize_connect_id(value: &str) -> Result<String> {
    let id: String = value.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if id.len() != 9 || !id.bytes().all(|c| c.is_ascii_digit()) {
        bail!("请输入9位设备 ID");
    }
    Ok(id)
}

pub(crate) fn validate_connect_code(code: &str) -> Result<()> {
    if code.len() > 256 || code.chars().any(char::is_control) {
        bail!("设备验证码格式不正确");
    }
    Ok(())
}

#[derive(Clone)]
pub struct AssistRequest {
    pub connect_id: String,
    pub(crate) connect_code: String,
    /// Validated preflight result shared with the in-process viewer.
    pub(crate) control_mode: Option<String>,
    pub(crate) expected_publisher_id: Option<String>,
}
impl AssistRequest {
    pub fn new(connect_id: &str, connect_code: String) -> Result<Self> {
        let request = Self {
            connect_id: normalize_connect_id(connect_id)?,
            connect_code,
            control_mode: None,
            expected_publisher_id: None,
        };
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<()> {
        normalize_connect_id(&self.connect_id)?;
        if let Some(id) = &self.expected_publisher_id {
            crate::account::api::validate_device_id(id)?;
        }
        validate_connect_code(&self.connect_code)
    }
    pub(crate) fn has_code(&self) -> bool {
        !self.connect_code.is_empty()
    }
}
impl fmt::Debug for AssistRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistRequest")
            .field("credentials", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, Deserialize)]
pub(crate) struct ControlMode {
    pub can_remote_control: bool,
    pub control_mode: String,
}

#[derive(Clone, Default, Deserialize)]
pub(crate) struct JoinReply {
    #[serde(default)]
    pub international_connect: bool,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub share_id: String,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub signaling_list: Vec<String>,
    #[serde(default)]
    pub ws_connect_timeout_ms: i32,
    #[serde(default)]
    pub streamer_retry_delta_ms: i32,
    #[serde(default)]
    pub publisher_platform: i32,
    #[serde(default)]
    pub publisher_version_name: String,
    #[serde(default)]
    pub publisher_device_id: String,
    #[serde(default)]
    pub control_id: String,
}
impl JoinReply {
    pub(crate) fn validate(&self, controller_id: &str) -> Result<()> {
        if self.token.is_empty() || self.signaling_list.is_empty() {
            bail!("远程协助未返回有效的会话凭据");
        }
        if !matches!(self.publisher_platform, 1 | 4) {
            bail!("暂不支持观看此类设备");
        }
        if !self.publisher_device_id.is_empty() {
            crate::account::api::validate_device_id(&self.publisher_device_id)?;
            if self.publisher_device_id == controller_id {
                bail!("不能连接本机身份");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SavedKind {
    Recent,
    Favorites,
}
impl SavedKind {
    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Favorites => "favorites",
        }
    }
}

#[derive(Clone, Deserialize)]
pub(crate) struct SavedDevice {
    pub publisher_device_id: String,
    pub connect_id: String,
    pub remark: String,
    #[serde(default)]
    pub is_favorite: bool,
    #[serde(default)]
    pub last_connected_at: Option<i64>,
    #[serde(default)]
    pub favorited_at: Option<i64>,
    #[serde(skip)]
    pub saved_code: String,
}
impl SavedDevice {
    pub(crate) fn title(&self) -> &str {
        if self.remark.trim().is_empty() {
            &self.connect_id
        } else {
            &self.remark
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        normalize_connect_id(&self.connect_id)?;
        crate::account::api::validate_device_id(&self.publisher_device_id)
    }
}
#[derive(Clone, Default, Deserialize)]
pub(crate) struct SavedReply {
    #[serde(default)]
    pub saved: Vec<SavedDevice>,
}
#[derive(Clone, Default)]
pub(crate) struct SavedLists {
    pub recent: Vec<SavedDevice>,
    pub favorites: Vec<SavedDevice>,
    pub code_error: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct FavoriteItem<'a> {
    pub connect_id: &'a str,
    pub remark: &'a str,
    pub favorited_at: i64,
}

/// 官方「复制并分享」发出的协助邀请链接：
/// `uuremote://external/assistance?deviceId=<9位设备 ID>&code=<验证码>&controlledLogin=<0|1>`
///
/// controlledLogin 表示被协助端生成邀请时的登录状态。4.41.2.2602
/// C140209E20 用 QString::toInt 解析；本程序只将它作为元信息，
/// 连接权限仍由设备 ID、验证码及既有预检决定。
#[derive(Clone, PartialEq, Eq)]
pub struct AssistLink {
    pub connect_id: String,
    /// 免密连接没有验证码，官方链接允许 `code` 缺失。
    pub code: String,
    pub controlled_login: bool,
}
impl fmt::Debug for AssistLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistLink")
            .field("connect_id", &self.connect_id)
            .field("code", &"REDACTED")
            .field("controlled_login", &self.controlled_login)
            .finish()
    }
}
impl AssistLink {
    pub const MAX_URI_BYTES: usize = 1024;
    /// 限制外部激活载荷；原始空白和控制字符必须使用 URL 编码。
    pub fn transportable(uri: &str) -> bool {
        uri.len() <= Self::MAX_URI_BYTES
            && !uri
                .chars()
                .any(|c| c.is_ascii_whitespace() || c.is_control())
    }
    pub fn parse(uri: &str) -> Result<Self> {
        if !Self::transportable(uri) {
            bail!("协助链接过长或包含无效字符");
        }
        let url = url::Url::parse(uri).map_err(|_| anyhow::anyhow!("协助链接格式不正确"))?;
        if !url.scheme().eq_ignore_ascii_case("uuremote") {
            bail!("不是 UU 远程的链接");
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.fragment().is_some()
        {
            bail!("协助链接格式不正确");
        }
        let route = format!(
            "{}{}",
            url.host_str().unwrap_or_default(),
            url.path().trim_end_matches('/')
        );
        if !route.eq_ignore_ascii_case("external/assistance") {
            bail!("暂不支持这种 UU 远程链接");
        }
        let mut connect_id = None;
        let mut code = String::new();
        let mut controlled_login = false;
        let mut seen = std::collections::HashSet::new();
        for (key, value) in url.query_pairs() {
            if matches!(key.as_ref(), "deviceId" | "code" | "controlledLogin")
                && !seen.insert(key.to_string())
            {
                bail!("协助链接包含重复参数");
            }
            match key.as_ref() {
                "deviceId" => connect_id = Some(normalize_connect_id(&value)?),
                "code" => {
                    validate_connect_code(&value)?;
                    code = value.into_owned();
                }
                "controlledLogin" => {
                    controlled_login = value.trim().parse::<i32>().ok() == Some(1);
                }
                // 链接还会带上系统、地区等只给官方客户端看的信息，忽略即可。
                _ => (),
            }
        }
        Ok(Self {
            connect_id: connect_id.context("协助链接里没有设备 ID")?,
            code,
            controlled_login,
        })
    }
}
