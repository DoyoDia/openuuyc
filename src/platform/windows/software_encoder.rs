//! GPU readback for the unified Rust software codec. No codec algorithms here.
use super::{
    format::{Encoded, Format, Rate},
    gpu_conversion::Conversion,
};
use anyhow::{Context, Result, ensure};
use openuuyc_codec::encoder::{Config, Encoder as Core};
use std::sync::{Arc, atomic::AtomicBool};
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*};

pub(crate) struct Encoder {
    core: Core,
    conversion: Option<Conversion>,
    staging: ID3D11Texture2D,
    context: ID3D11DeviceContext,
    size: (u32, u32),
    format: Format,
    rate: Rate,
}
impl Encoder {
    pub fn new(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        let core = Core::new(Config {
            width: size.0,
            height: size.1,
            format: format.software(),
            fps: rate.fps,
            bitrate: rate.target,
        })?;
        let conversion = if core.input_format() == openuuyc_codec::PixelFormat::Bgra {
            None
        } else {
            Some(Conversion::new(device, size, format)?)
        };
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        if let Some(conversion) = &conversion {
            unsafe { conversion.output.GetDesc(&mut desc) };
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        }
        Ok(Self {
            core,
            conversion,
            staging: staging.context("软件编码读回纹理")?,
            context: unsafe { device.GetImmediateContext()? },
            size,
            format,
            rate,
        })
    }
    pub fn request_keyframe(&mut self) {
        self.core.request_keyframe();
    }
    pub fn maximum_size(&self) -> (u32, u32) {
        openuuyc_codec::encoder::maximum_size(self.format.codec.media())
    }
    pub fn configure(&mut self, rate: Rate) -> Result<bool> {
        if rate.target != self.rate.target || rate.fps != self.rate.fps {
            self.core.configure(rate.fps, rate.target)?;
        }
        let key = self.rate.quality != rate.quality;
        self.rate = rate;
        Ok(key)
    }
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        key: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Vec<Encoded>> {
        if key {
            self.core.request_keyframe();
        }
        let source = if let Some(conversion) = &mut self.conversion {
            conversion.convert(texture)?;
            &conversion.output
        } else {
            texture
        };
        unsafe {
            self.context.CopyResource(&self.staging, source);
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let prepared = (|| {
            let (row, rows) = self
                .core
                .input_format()
                .layout(self.size.0 as usize, self.size.1 as usize)
                .context("软件读回大小溢出")?;
            let pitch = mapped.RowPitch as usize;
            ensure!(
                !mapped.pData.is_null() && pitch >= row,
                "软件编码读回行跨度无效"
            );
            let len = pitch.checked_mul(rows).context("软件读回大小溢出")?;
            // SAFETY: Map owns the staging allocation until Unmap below. Pitch
            // and row count are checked against its configured pixel layout.
            let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
            self.core.prepare(bytes, pitch, cancel)
        })();
        unsafe {
            self.context.Unmap(&self.staging, 0);
        }
        prepared?;
        Ok(self
            .core
            .encode(timestamp, key, cancel)?
            .into_iter()
            .map(|p| Encoded {
                data: p.data,
                keyframe: p.keyframe,
                timestamp_100ns: p.timestamp_100ns,
                is_new: true,
                timing: None,
                format: self.format,
                color: self.format.color(None),
            })
            .collect())
    }
}
