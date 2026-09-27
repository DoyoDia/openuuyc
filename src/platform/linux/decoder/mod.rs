//! Linux video decoding: VA-API in hardware, Rust H.264 in software.
//! Both paths hand the picture over on the CPU; VA-API surfaces are copied
//! out rather than shared with the renderer, so there is no zero-copy yet.
#![cfg(not(windows))]
use crate::media::VideoCodec;
use crate::media::decode_api::{DecodeError, DecoderMode, DecoderNotification, VideoDecoderConfig};
use bytes::Bytes;

mod vaapi;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuFormat {
    Nv12,
    I444,
}

pub struct CpuVideoFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub format: CpuFormat,
    pub data: Bytes,
}

/// VA-API opens its own display; the decoder shares no device with the
/// renderer, so there is none to pass in.
pub(crate) type GpuDevice = std::convert::Infallible;

pub enum PlatformDecodedFrame {
    Cpu(CpuVideoFrame),
}

/// Whether the local VA-API driver decodes this format in hardware.
pub fn probe_hardware(codec: VideoCodec, width: u32, height: u32, depth: u8, chroma: u8) -> bool {
    vaapi::probe(codec, width, height, depth, chroma)
}

enum Backend {
    /// VA-API, which decodes one access unit per call.
    Hardware(Box<vaapi::Session>),
    Software(Box<openuuyc_h264::stream::Decoder>),
}

pub struct LinuxVideoDecoder {
    backend: Backend,
    pending: VecDeque<PlatformDecodedFrame>,
    notification: DecoderNotification,
}

fn software_error(error: openuuyc_h264::Error) -> DecodeError {
    use openuuyc_h264::Error;
    match error {
        Error::Cancelled | Error::Closed => DecodeError::Closed,
        Error::Unsupported(_) => DecodeError::Unsupported,
        Error::Allocation => DecodeError::Backend,
        Error::NeedKeyframe | Error::Truncated | Error::Invalid(_) => {
            tracing::debug!(%error, "Rust H264 input requires a new keyframe");
            DecodeError::NeedKeyframe
        }
    }
}

impl LinuxVideoDecoder {
    pub fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        let backend = match config.mode {
            // Hardware surfaces still come back on the CPU.
            DecoderMode::Hardware => Backend::Hardware(Box::new(vaapi::Session::open(config)?)),
            DecoderMode::Software => {
                if config.codec != VideoCodec::H264 {
                    return Err(DecodeError::Unsupported);
                }
                if config.width == 0
                    || config.height == 0
                    || config.width > 16384
                    || config.height > 16384
                {
                    return Err(DecodeError::InvalidInput);
                }
                let mut decoder = openuuyc_h264::stream::Decoder::new();
                decoder.seed(&config.extra_data).map_err(software_error)?;
                Backend::Software(Box::new(decoder))
            }
        };
        Ok(Self {
            backend,
            pending: VecDeque::new(),
            notification: DecoderNotification::default(),
        })
    }

    pub fn probe_format(
        _device: GpuDevice,
        codec: VideoCodec,
        width: u32,
        height: u32,
        depth: u8,
        chroma: u8,
    ) -> bool {
        vaapi::probe(codec, width, height, depth, chroma)
    }

    pub fn poll_owned_frame(&mut self) -> Result<Option<PlatformDecodedFrame>, DecodeError> {
        if self.notification.is_cancelled() {
            self.pending.clear();
            return Err(DecodeError::Closed);
        }
        Ok(self.pending.pop_front())
    }

    pub fn poll_dropped_token(&mut self) -> Option<i64> {
        None
    }

    pub fn reset_for_keyframe(&mut self) -> Result<(), DecodeError> {
        self.pending.clear();
        match &mut self.backend {
            Backend::Hardware(session) => {
                session.reset();
                Ok(())
            }
            Backend::Software(decoder) => decoder.reset().map_err(software_error),
        }
    }
}

impl LinuxVideoDecoder {
    pub fn set_notification(&mut self, notification: DecoderNotification) {
        self.notification = notification;
    }

    pub fn push_packet(&mut self, payload: &[u8], token: i64) -> Result<(), DecodeError> {
        if payload.len() > i32::MAX as usize {
            return Err(DecodeError::InvalidInput);
        }
        let decoder = match &mut self.backend {
            Backend::Hardware(session) => {
                let frame = session.decode(payload, self.notification.cancellation())?;
                self.pending
                    .push_back(PlatformDecodedFrame::Cpu(CpuVideoFrame {
                        pts: token,
                        width: frame.width,
                        height: frame.height,
                        format: CpuFormat::Nv12,
                        data: Bytes::from(frame.data),
                    }));
                return Ok(());
            }
            Backend::Software(decoder) => decoder,
        };
        let outputs = decoder
            .submit_with_cancel(payload, token as u64, self.notification.cancellation())
            .map_err(software_error)?;
        for output in outputs {
            let picture = output.picture;
            let mut packed = Vec::new();
            picture.pack_into(&mut packed).map_err(software_error)?;
            if self.notification.is_cancelled() {
                self.pending.clear();
                return Err(DecodeError::Closed);
            }
            self.pending
                .push_back(PlatformDecodedFrame::Cpu(CpuVideoFrame {
                    pts: output.token as i64,
                    width: picture.crop.width as u32,
                    height: picture.crop.height as u32,
                    format: if picture.chroma == openuuyc_h264::picture::Chroma::Yuv444 {
                        CpuFormat::I444
                    } else {
                        CpuFormat::Nv12
                    },
                    data: Bytes::from(packed),
                }));
        }
        Ok(())
    }
}

impl Drop for LinuxVideoDecoder {
    fn drop(&mut self) {
        self.pending.clear();
    }
}
