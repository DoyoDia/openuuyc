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
/// 链接由被协助方的客户端生成，或由 `https://uuyc.163.com/invite/{邀请码}`
/// 落地页把服务端的 `source_data` 原样拼进查询串。官方客户端用 `strtol` 按
/// 十进制解析 `controlledLogin`，判 `== 1` 记为真，说的是被协助端生成邀请时
/// 的登录状态；本程序的连接只由设备 ID 和验证码决定，所以照抄它的宽松程度就
/// 够了——非数字一律当假。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistLink {
    pub connect_id: String,
    /// 免密连接没有验证码，官方链接允许 `code` 缺失。
    pub code: String,
    pub controlled_login: bool,
}
impl AssistLink {
    /// 链接能否原样塞进逐行收发的通道（Linux 的单实例控制通道）。正常链接里的
    /// 空白一定被百分号编码过，出现空白就说明这一行会被拆散。
    pub fn transportable(uri: &str) -> bool {
        !uri
            .chars()
            .any(|c| c.is_ascii_whitespace() || c.is_control())
    }
    pub fn parse(uri: &str) -> Result<Self> {
        if !Self::transportable(uri) {
            bail!("协助链接包含空白字符");
        }
        let url = url::Url::parse(uri).map_err(|_| anyhow::anyhow!("协助链接格式不正确"))?;
        if !url.scheme().eq_ignore_ascii_case("uuremote") {
            bail!("不是 UU 远程的链接");
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
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "deviceId" => connect_id = Some(normalize_connect_id(&value)?),
                "code" => {
                    validate_connect_code(&value)?;
                    code = value.into_owned();
                }
                "controlledLogin" => controlled_login = value.trim() == "1",
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

#[cfg(test)]
mod link_tests {
    use super::AssistLink;

    #[test]
    fn reads_an_invite_link() {
        let link = AssistLink::parse(
            "uuremote://external/assistance?deviceId=654677613&code=Z4TFVC3K&controlledLogin=1",
        )
        .unwrap();
        assert_eq!(link.connect_id, "654677613");
        assert_eq!(link.code, "Z4TFVC3K");
        assert!(link.controlled_login);
    }

    #[test]
    fn passwordless_links_carry_no_code() {
        let link = AssistLink::parse("uuremote://external/assistance?deviceId=123456789").unwrap();
        assert_eq!(link.code, "");
        // 官方用 strtol 解析，非 1 的值一律当假。
        assert!(!link.controlled_login);
    }

    #[test]
    fn decodes_percent_escapes() {
        let link = AssistLink::parse(
            "uuremote://external/assistance?deviceId=123456789&code=A%20b%2Bc&controlledLogin=0",
        )
        .unwrap();
        assert_eq!(link.code, "A b+c");
        assert!(!link.controlled_login);
    }

    #[test]
    fn rejects_other_routes_and_shapes() {
        for uri in [
            "uuremote://external/cloudDeviceAdd?deviceId=123456789",
            "uuremote://external/device/control?anonymous_device_id=x",
            "openuuyc-notification://open/0123456789abcdef0123456789abcdef",
            "https://uuyc.163.com/invite/Z4TFVC3K",
            "uuremote://external/assistance",
            "uuremote://external/assistance?deviceId=12345",
            "uuremote://external/assistance?deviceId=654677613&code=a b",
            " uuremote://external/assistance?deviceId=654677613",
        ] {
            assert!(AssistLink::parse(uri).is_err(), "{uri} 不该被接受");
        }
    }
}
