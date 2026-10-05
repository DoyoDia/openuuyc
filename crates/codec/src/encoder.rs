//! Immediate-output software encoding; no input queue or future-frame lookahead.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod av1;
mod h264;
use crate::{Codec, Format, PixelFormat};
use anyhow::{ensure, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Software streaming limits, also used by capability negotiation.
pub const fn maximum_size(codec: Codec) -> (u32, u32) {
    match codec {
        Codec::H264 => (3840, 2160),
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        Codec::Av1 => (1920, 1080),
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        Codec::Av1 => (0, 0),
        Codec::H265 => (0, 0),
    }
}
pub const fn maximum_fps(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => 144,
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        Codec::Av1 => 30,
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        Codec::Av1 => 0,
        Codec::H265 => 0,
    }
}
#[derive(Clone, Copy)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub fps: u32,
    pub bitrate: u32,
}
impl Config {
    fn validate(self) -> Result<()> {
        ensure!(
            self.format.can_encode(),
            "unsupported software encoding format"
        );
        let min = if self.format.codec == Codec::Av1 {
            16
        } else {
            2
        };
        let maximum = maximum_size(self.format.codec);
        ensure!(
            self.width >= min
                && self.height >= min
                && self.width <= maximum.0
                && self.height <= maximum.1
                && self.width % 2 == 0
                && self.height % 2 == 0,
            "invalid software encoding dimensions"
        );
        ensure!(
            (1..=maximum_fps(self.format.codec)).contains(&self.fps)
                && (1..=500_000_000).contains(&self.bitrate),
            "invalid software encoding rate"
        );
        Ok(())
    }
}
pub struct Packet {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_100ns: i64,
}
enum Kernel {
    H264(h264::Encoder),
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    Av1(av1::Encoder),
}
pub struct Encoder {
    kernel: Kernel,
    config: Config,
    key_requested: bool,
    needs_reset: bool,
    prepared: bool,
}
impl Encoder {
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            kernel: Self::create(config)?,
            config,
            key_requested: true,
            needs_reset: false,
            prepared: false,
        })
    }
    fn create(config: Config) -> Result<Kernel> {
        match config.format.codec {
            Codec::H264 => Ok(Kernel::H264(h264::Encoder::new(config)?)),
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Codec::Av1 => Ok(Kernel::Av1(av1::Encoder::new(config)?)),
            #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
            Codec::Av1 => anyhow::bail!("software AV1 encoding requires x86 or x86_64"),
            Codec::H265 => anyhow::bail!("software HEVC encoding is unavailable"),
        }
    }
    pub fn input_format(&self) -> PixelFormat {
        self.config.format.encoder_input()
    }
    pub fn request_keyframe(&mut self) {
        self.key_requested = true;
    }
    pub fn configure(&mut self, fps: u32, bitrate: u32) -> Result<()> {
        let next = Config {
            fps,
            bitrate,
            ..self.config
        };
        next.validate()?;
        if self.needs_reset {
            self.kernel = Self::create(next)?;
            self.needs_reset = false;
            self.key_requested = true;
        } else {
            match &mut self.kernel {
                Kernel::H264(e) => e.configure(fps, bitrate)?,
                #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
                Kernel::Av1(e) => e.configure(fps, bitrate)?,
            }
        }
        self.config = next;
        Ok(())
    }
    /// Convert borrowed packed pixels into the reusable planes. The caller may
    /// unmap its GPU staging resource before the actual encoding starts.
    pub fn prepare(&mut self, data: &[u8], pitch: usize, cancel: &AtomicBool) -> Result<()> {
        self.prepared = false;
        ensure!(
            !cancel.load(Ordering::Acquire),
            "software encoding cancelled"
        );
        if self.needs_reset {
            self.kernel = Self::create(self.config)?;
            self.needs_reset = false;
        }
        match &mut self.kernel {
            Kernel::H264(e) => e.prepare(data, pitch)?,
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Kernel::Av1(e) => e.prepare(data, pitch, cancel)?,
        }
        ensure!(
            !cancel.load(Ordering::Acquire),
            "software encoding cancelled"
        );
        self.prepared = true;
        Ok(())
    }
    pub fn encode(
        &mut self,
        timestamp: i64,
        key: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Option<Packet>> {
        self.key_requested |= key;
        ensure!(
            std::mem::take(&mut self.prepared),
            "no prepared software frame"
        );
        ensure!(
            !cancel.load(Ordering::Acquire),
            "software encoding cancelled"
        );
        let result = match &mut self.kernel {
            Kernel::H264(e) => e.encode(timestamp, self.key_requested),
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Kernel::Av1(e) => e.encode(timestamp, self.key_requested, cancel),
        };
        if result.is_err() || cancel.load(Ordering::Acquire) {
            self.needs_reset = true;
            self.key_requested = true;
            ensure!(
                !cancel.load(Ordering::Acquire),
                "software encoding cancelled"
            );
        } else if result.as_ref().is_ok_and(Option::is_some) {
            self.key_requested = false;
        }
        result
    }
}
