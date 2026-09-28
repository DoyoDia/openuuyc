use anyhow::{Result, bail};

const FPS_CHOICES: [FrameRateChoice; 5] = [
    FrameRateChoice::Auto,
    FrameRateChoice::Fps144,
    FrameRateChoice::Fps90,
    FrameRateChoice::Fps60,
    FrameRateChoice::Fps30,
];
const FPS_LEVELS_ASCENDING: [u32; 4] = [30, 60, 90, 144];

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct LocalDisplayInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

impl LocalDisplayInfo {
    pub const FALLBACK: Self = Self {
        width: 1920,
        height: 1080,
        refresh_hz: 60,
    };
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameRateChoice {
    Auto,
    Fps144,
    Fps90,
    Fps60,
    Fps30,
}

impl FrameRateChoice {
    pub fn value(self, display: LocalDisplayInfo) -> u32 {
        match self {
            Self::Auto => max_frame_rate_level(display.refresh_hz),
            Self::Fps144 => 144,
            Self::Fps90 => 90,
            Self::Fps60 => 60,
            Self::Fps30 => 30,
        }
    }

    pub fn available(_display: LocalDisplayInfo) -> Vec<Self> {
        // The ordinary desktop menu exposes all four explicit levels. The
        // receiver refresh rate limits fps_count, not the user's level list.
        FPS_CHOICES.to_vec()
    }

    pub fn label(self, display: LocalDisplayInfo) -> String {
        let value = self.value(display);
        if self == Self::Auto {
            format!("自动 {value} FPS")
        } else {
            format!("{value} FPS")
        }
    }
    pub(crate) fn requires_faster_screen(self, display: LocalDisplayInfo, refresh_hz: u32) -> bool {
        // Levels are ceilings: a 120Hz screen fits in the 144 tier, and a
        // 75Hz screen in the 90 tier. Only a higher tier needs a virtual mode.
        refresh_hz > 0
            && self.value(display).min(display.refresh_hz) > max_frame_rate_level(refresh_hz)
    }
}

impl std::str::FromStr for FrameRateChoice {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "144" | "144fps" => Ok(Self::Fps144),
            "90" | "90fps" => Ok(Self::Fps90),
            "60" | "60fps" => Ok(Self::Fps60),
            "30" | "30fps" => Ok(Self::Fps30),
            _ => bail!("unsupported frame-rate choice: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CodecPreference {
    Auto,
    H264,
    H265,
    Av1,
}

impl CodecPreference {
    pub(crate) fn accepts(self, codec: VideoCodec) -> bool {
        match self {
            Self::Auto => true,
            Self::H264 => codec == VideoCodec::H264,
            Self::H265 => codec == VideoCodec::H265,
            Self::Av1 => codec == VideoCodec::Av1,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动 AV1/H.265/H.264",
            Self::H264 => "H.264",
            Self::H265 => "H.265",
            Self::Av1 => "AV1",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Av1,
            Self::Av1 => Self::H265,
            Self::H265 => Self::H264,
            Self::H264 => Self::Auto,
        }
    }
}

impl std::str::FromStr for CodecPreference {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "h264" | "avc" => Ok(Self::H264),
            "h265" | "hevc" => Ok(Self::H265),
            "av1" => Ok(Self::Av1),
            _ => bail!("unsupported codec preference: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TransportChoice {
    Auto,
    P2p,
    Relay,
}

impl TransportChoice {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动（LAN/P2P/relay）",
            Self::P2p => "仅 LAN/P2P",
            Self::Relay => "仅 relay",
        }
    }
}

impl std::str::FromStr for TransportChoice {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "p2p" | "direct" | "lan" => Ok(Self::P2p),
            "relay" | "turn" => Ok(Self::Relay),
            _ => bail!("unsupported transport choice: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ConnectionMediaOptions {
    pub audio_only: bool,
    pub muted: bool,
    pub frame_rate: FrameRateChoice,
    pub codec: CodecPreference,
    pub hardware_decode: bool,
    pub transport: TransportChoice,
}

impl Default for ConnectionMediaOptions {
    fn default() -> Self {
        Self {
            audio_only: false,
            muted: false,
            frame_rate: FrameRateChoice::Auto,
            codec: CodecPreference::Auto,
            hardware_decode: true,
            transport: TransportChoice::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct ConnectionMediaProfile {
    pub audio_only: bool,
    pub muted: bool,
    pub local_display: LocalDisplayInfo,
    pub stream_fps: u32,
    pub decoder_fps_cap: u32,
    pub codec: CodecPreference,
    pub hardware_decode: bool,
}

impl ConnectionMediaOptions {
    pub(crate) fn resolve(self, display: LocalDisplayInfo) -> Result<ConnectionMediaProfile> {
        let stream_fps = self.frame_rate.value(display);
        Ok(ConnectionMediaProfile {
            audio_only: self.audio_only,
            muted: self.muted,
            local_display: display,
            stream_fps,
            decoder_fps_cap: display.refresh_hz.max(stream_fps),
            codec: self.codec,
            hardware_decode: self.hardware_decode,
        })
    }
}

fn max_frame_rate_level(refresh_hz: u32) -> u32 {
    FPS_LEVELS_ASCENDING
        .into_iter()
        .find(|level| refresh_hz <= *level)
        .unwrap_or(144)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

impl std::str::FromStr for VideoCodec {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Ok(Self::H264),
            "h265" | "hevc" => Ok(Self::H265),
            "av1" => Ok(Self::Av1),
            _ => bail!("unsupported video codec: {value}"),
        }
    }
}
