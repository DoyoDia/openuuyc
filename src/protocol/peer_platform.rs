//! ConnectOptions.ClientType and account platform IDs are different enums.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum PeerPlatform {
    Windows,
    Mac,
    Android,
    Ios,
    Unknown,
}
impl PeerPlatform {
    pub fn from_connect_client_type(value: i32) -> Self {
        match value {
            1 => Self::Ios,
            2 => Self::Android,
            3 => Self::Windows,
            4 => Self::Mac,
            _ => Self::Unknown,
        }
    }
    pub fn account_code(self) -> Option<i32> {
        match self {
            Self::Windows => Some(1),
            Self::Android => Some(2),
            Self::Ios => Some(3),
            Self::Mac => Some(4),
            Self::Unknown => None,
        }
    }
}
