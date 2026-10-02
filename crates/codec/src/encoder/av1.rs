//! AV1 streaming configuration and reusable input planes, independent of D3D.
use super::{Config, Packet};
use anyhow::{Context, Result};
use openuuyc_av1::encoder::prelude::config::SceneDetectionSpeed;
use openuuyc_av1::encoder::prelude::{
    ChromaSampling, ColorDescription, ColorPrimaries, MatrixCoefficients, PixelRange, Rational,
    TransferCharacteristics, Tune,
};
use openuuyc_av1::encoder::streaming::{Encoder as StreamEncoder, Samples};
use openuuyc_av1::encoder::{Config as CoreConfig, EncoderConfig};
use std::sync::{Arc, atomic::AtomicBool};

enum Planes {
    Eight([Vec<u8>; 3]),
    Ten([Vec<u16>; 3]),
}
pub(super) struct Encoder {
    core: StreamEncoder,
    planes: Planes,
    config: Config,
}
impl Encoder {
    pub fn new(config: Config) -> Result<Self> {
        let mut enc = EncoderConfig::with_speed_preset(10);
        enc.width = config.width as usize;
        enc.height = config.height as usize;
        enc.bit_depth = config.format.depth as usize;
        enc.chroma_sampling = if config.format.chroma == 1 {
            ChromaSampling::Cs420
        } else {
            ChromaSampling::Cs444
        };
        enc.pixel_range = if config.format.depth == 10 {
            PixelRange::Full
        } else {
            PixelRange::Limited
        };
        enc.color_description = Some(if config.format.depth == 10 {
            ColorDescription {
                color_primaries: ColorPrimaries::BT2020,
                transfer_characteristics: TransferCharacteristics::SMPTE2084,
                matrix_coefficients: MatrixCoefficients::BT2020NCL,
            }
        } else {
            ColorDescription {
                color_primaries: ColorPrimaries::BT601,
                transfer_characteristics: TransferCharacteristics::BT470M,
                matrix_coefficients: MatrixCoefficients::BT601,
            }
        });
        enc.level_idx = Some(17);
        enc.low_latency = true;
        enc.speed_settings.rdo_lookahead_frames = 0;
        enc.speed_settings.scene_detection_mode = SceneDetectionSpeed::None;
        enc.set_key_frame_interval(0, 0);
        enc.bitrate = i32::try_from(config.bitrate).context("AV1目标码率超限")?;
        enc.time_base = Rational {
            num: 1,
            den: u64::from(config.fps),
        };
        enc.quantizer = 255;
        enc.min_quantizer = 1;
        enc.tune = Tune::Psnr;
        enc.reservoir_frame_delay = Some(12);
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(4);
        // Independent tiles use the bounded worker pool within the current frame.
        enc.tiles = threads;
        let core_config = CoreConfig::new()
            .with_encoder_config(enc)
            .with_threads(threads);
        let y = config.width as usize * config.height as usize;
        let uv = if config.format.chroma == 1 { y / 4 } else { y };
        let planes = if config.format.depth == 8 {
            Planes::Eight([vec![0; y], vec![0; uv], vec![0; uv]])
        } else {
            Planes::Ten([vec![0; y], vec![0; uv], vec![0; uv]])
        };
        Ok(Self {
            core: StreamEncoder::new(core_config)?,
            planes,
            config,
        })
    }

    pub fn configure(&mut self, fps: u32, bitrate: u32) -> Result<()> {
        self.core.reconfigure(
            i32::try_from(bitrate)?,
            Rational {
                num: 1,
                den: u64::from(fps),
            },
        )?;
        Ok(())
    }
    pub fn prepare(&mut self, data: &[u8], pitch: usize, cancel: &AtomicBool) -> Result<()> {
        let size = (self.config.width as usize, self.config.height as usize);
        let half = self.config.format.chroma == 1;
        match &mut self.planes {
            Planes::Eight(p) => {
                openuuyc_av1::encoder::streaming::unpack_8(data, pitch, size, half, p, cancel)?
            }
            Planes::Ten(p) => {
                openuuyc_av1::encoder::streaming::unpack_10(data, pitch, size, half, p, cancel)?
            }
        }
        Ok(())
    }
    pub fn encode(
        &mut self,
        timestamp: i64,
        key: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Option<Packet>> {
        self.core.set_cancellation(cancel.clone());
        let samples = match &self.planes {
            Planes::Eight(p) => Samples::Eight([&p[0], &p[1], &p[2]]),
            Planes::Ten(p) => Samples::Ten([&p[0], &p[1], &p[2]]),
        };
        let p = self
            .core
            .encode(samples, key)
            .context("AV1 software encoding failed")?;
        Ok(Some(Packet {
            data: p.data,
            keyframe: p.keyframe,
            timestamp_100ns: timestamp,
        }))
    }
}
