//! Rust D3D11 decoding for Windows 4:2:0 / HEVC 4:4:4 hardware playback.
//! Hardware preparation never calls the native video bridge.
mod avc;
pub(super) mod dxva;
mod hevc;
mod output;
mod params;

use super::*;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

enum Decoder {
    Avc(avc::Avc),
    Hevc(hevc::Hevc),
}
pub(super) struct Session {
    decoder: Decoder,
    output: output::Output,
    extra: Option<Bytes>,
}
impl Session {
    pub(super) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        if config.width == 0 || config.height == 0 || config.width > 16384 || config.height > 16384
        {
            return Err(DecodeError::InvalidInput);
        }
        let device = config
            .gpu_device
            .as_ref()
            .ok_or(DecodeError::InvalidInput)?
            .clone();
        if !config.extra_data.is_empty()
            && !config.extra_data.starts_with(&[0, 0, 1])
            && !config.extra_data.starts_with(&[0, 0, 0, 1])
        {
            return Err(DecodeError::Unsupported);
        }
        let decoder = match config.codec {
            VideoCodec::H264 => Decoder::Avc(avc::Avc::new(device)),
            VideoCodec::H265 => Decoder::Hevc(hevc::Hevc::new(device)),
        };
        tracing::info!(codec=?config.codec, "Rust DXVA session created");
        Ok(Self {
            decoder,
            output: output::Output::default(),
            extra: Some(config.extra_data.clone()),
        })
    }
    pub(super) fn probe(
        handle: ID3D11Device,
        codec: VideoCodec,
        w: u32,
        h: u32,
        depth: u8,
        chroma: u8,
    ) -> bool {
        Self::check_format(handle, codec, w, h, depth, chroma).is_ok()
    }
    pub(super) fn check_format(
        handle: ID3D11Device,
        codec: VideoCodec,
        w: u32,
        h: u32,
        depth: u8,
        chroma: u8,
    ) -> anyhow::Result<()> {
        let device = &handle;
        let codec = match codec {
            VideoCodec::H264 => dxva::Codec::H264,
            VideoCodec::H265 => dxva::Codec::Hevc,
        };
        dxva::Pool::probe(device, codec, w, h, depth, chroma)
    }
    pub(super) fn push(
        &mut self,
        payload: &[u8],
        token: i64,
        notification: &DecoderNotification,
    ) -> Result<(), DecodeError> {
        if notification.is_cancelled() {
            return Err(DecodeError::Closed);
        }
        let extra = self.extra.take().filter(|e| !e.is_empty());
        let combined;
        let data = if let Some(extra) = extra {
            combined = [extra.as_ref(), payload].concat();
            &combined[..]
        } else {
            payload
        };
        let decoded = match &mut self.decoder {
            Decoder::Avc(d) => d.decode(data, notification.cancellation()).map(Some),
            Decoder::Hevc(d) => d.decode(data, notification.cancellation()),
        };
        let result = decoded.and_then(|picture| {
            if let Some(picture) = picture {
                self.output.push(output::Frame {
                    picture,
                    token: token,
                })
            } else {
                self.output.discard_token(token);
                Ok(())
            }
        });
        result.map_err(|error| {
            self.reset();
            if notification.is_cancelled() {
                return DecodeError::Closed;
            }
            tracing::warn!(detail=%error, "Rust DXVA decode failed");
            match error.downcast_ref::<dxva::Failure>() {
                Some(dxva::Failure::Unsupported) => DecodeError::Unsupported,
                Some(dxva::Failure::HardwareFailure) => DecodeError::HardwareFailure,
                None if error.downcast_ref::<windows::core::Error>().is_some() => {
                    DecodeError::HardwareFailure
                }
                None => DecodeError::NeedKeyframe,
            }
        })
    }
    pub(super) fn poll(&mut self) -> Result<Option<WindowsDecodedFrame>, DecodeError> {
        let Some(frame) = self.output.poll() else {
            return Ok(None);
        };
        let picture = frame.picture;
        // Keep queued pictures in the decode DPB. The smaller shared display
        // pool is acquired only once the picture is actually ready for output.
        let pool = std::sync::Arc::clone(&picture.surface.pool);
        let surface = pool.display(picture.surface).map_err(|error| {
            tracing::warn!(detail=%error, "Rust DXVA output transfer failed");
            self.reset();
            DecodeError::HardwareFailure
        })?;
        Ok(Some(WindowsDecodedFrame::Gpu(WindowsGpuVideoFrame {
            texture: surface.texture().clone(),
            subresource: surface.subresource(),
            pts: frame.token,
            visible_x: picture.left,
            visible_y: picture.top,
            width: picture.width,
            height: picture.height,
            _lease: surface,
        })))
    }
    pub(super) fn reset(&mut self) {
        self.output.reset();
        match &mut self.decoder {
            Decoder::Avc(d) => d.reset(),
            Decoder::Hevc(d) => d.reset(),
        }
    }
    pub(super) fn poll_dropped(&mut self) -> Option<i64> {
        self.output.poll_dropped()
    }
}
