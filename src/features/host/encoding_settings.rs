//! Local encoder constraints applied once when admitting a new connection.
use super::format::{Backend, Capability, Codec};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum EncoderMode {
    #[default]
    Automatic,
    Hardware,
    Software,
}
impl EncoderMode {
    pub const ALL: [Self; 3] = [Self::Automatic, Self::Hardware, Self::Software];
    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "硬件优先（自动回退）",
            Self::Hardware => "仅硬件编码",
            Self::Software => "仅软件编码",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum EncoderCodec {
    #[default]
    Automatic,
    H264,
    H265,
    Av1,
}
impl EncoderCodec {
    pub const ALL: [Self; 4] = [Self::Automatic, Self::Av1, Self::H265, Self::H264];
    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "自动协商 AV1 / H.265 / H.264",
            Self::H264 => "仅 H.264",
            Self::H265 => "仅 H.265",
            Self::Av1 => "仅 AV1",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EncodingSettings {
    pub mode: EncoderMode,
    pub codec: EncoderCodec,
}
impl EncodingSettings {
    pub fn accepts(self, capability: &Capability) -> bool {
        let software = capability.backend == Backend::Software;
        (match self.mode {
            EncoderMode::Automatic => true,
            EncoderMode::Hardware => !software,
            EncoderMode::Software => software,
        }) && match self.codec {
            EncoderCodec::Automatic => true,
            EncoderCodec::H264 => capability.format.codec == Codec::H264,
            EncoderCodec::H265 => capability.format.codec == Codec::H265,
            EncoderCodec::Av1 => capability.format.codec == Codec::Av1,
        }
    }
    pub fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.mode != EncoderMode::Software
                || matches!(
                    self.codec,
                    EncoderCodec::Automatic | EncoderCodec::H264 | EncoderCodec::Av1
                ),
            "软件编码支持 H.264 和 AV1，请选择自动、H.264 或 AV1"
        );
        Ok(())
    }
    pub fn select(self, capabilities: &[Capability]) -> anyhow::Result<Vec<Capability>> {
        self.validate()?;
        let selected: Vec<_> = capabilities
            .iter()
            .filter(|capability| self.accepts(capability))
            .cloned()
            .collect();
        anyhow::ensure!(
            !selected.is_empty(),
            "当前屏幕没有符合本机编码设置的可用编码器"
        );
        Ok(selected)
    }
}
