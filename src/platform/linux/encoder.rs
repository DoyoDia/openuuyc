//! Linux host encoding: the Rust H.264 core fed with captured BGRA.
//!
//! VA-API encoding would need the captured pixels uploaded to a VA surface
//! first; until that exists the software core is the only encoder offered, and
//! the probe proves it on the selected desktop like the Windows candidates.
use super::capture::{Desktop, Device};
use crate::media::encoding::software::{Encoder as SoftwareEncoder, MAXIMUM};
use crate::media::encoding::{Backend, Capability, Format, Rate};
use anyhow::{Context, Result, bail, ensure};

pub(crate) use crate::media::encoding::{Encoded, FrameTiming};

pub(crate) fn probe(
    desktop: &mut Desktop,
    is_active: impl Fn() -> bool,
) -> Result<Vec<Capability>> {
    let mut frame = None;
    for _ in 0..20 {
        if !is_active() {
            bail!("被控准备已取消");
        }
        if let Some(captured) = desktop.next(50, 1, false, false, (1280, 720))? {
            frame = Some(captured);
            break;
        }
    }
    let frame = frame.context("尚未取得所选桌面画面")?;
    let mut encoder = Encoder::software(&desktop.device, frame.width, frame.height, 30, 2_000_000)?;
    let output = encoder
        .encode(&frame.image, 0, true)?
        .iter()
        .find_map(|encoded| {
            crate::media::video_format::parse_annex_b_format(
                Format::AVC.codec.media(),
                &encoded.data,
            )
        })
        .context("编码器未输出可验证的SPS")?;
    ensure!(
        output.chroma_format_idc == Format::AVC.chroma
            && output.bit_depth_luma == Format::AVC.depth
            && output.bit_depth_chroma == Format::AVC.depth
            && (output.visible_width, output.visible_height) == (frame.width, frame.height),
        "编码器实际码流与请求格式不一致"
    );
    let capability = Capability {
        adapter: desktop.screen.adapter,
        backend: Backend::Software,
        format: Format::AVC,
        maximum: encoder.maximum_size(),
    };
    tracing::debug!(?capability, "host encoder capability verified from SPS");
    Ok(vec![capability])
}

/// Per-thread encoder runtime. The software core needs no setup.
pub(crate) struct Runtime;
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct SwitchCandidate(pub String);
impl Runtime {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self)
    }
}

pub(crate) struct Encoder(SoftwareEncoder);
impl Encoder {
    pub(crate) fn hardware_format(
        _device: &Device,
        _size: (u32, u32),
        _format: Format,
        _rate: Rate,
    ) -> Result<Self> {
        // The probe never offers a hardware candidate on Linux.
        bail!("Linux 被控端没有硬件编码器")
    }
    pub(crate) fn software(
        _device: &Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self> {
        Ok(Self(SoftwareEncoder::new(width, height, fps, bitrate)?))
    }
    pub(crate) fn implementation(&self) -> i32 {
        5
    }
    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        MAXIMUM
    }
    pub(crate) fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        self.0.configure(rate)
    }
    pub(crate) fn encode(
        &mut self,
        image: &[u8],
        timestamp: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        let expected = self.0.width as usize * self.0.height as usize * 4;
        ensure!(image.len() == expected, "采集画面尺寸与编码器不一致");
        self.0.encode(image, timestamp, keyframe)
    }
}

#[cfg(test)]
mod tests {
    /// Needs a running X session: `DISPLAY=:0 cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn probe_captures_and_encodes_the_desktop() {
        let screens = super::super::capture::screens().unwrap();
        println!("{screens:#?}");
        let targets = super::super::display::topology::Topology::query(false)
            .unwrap()
            .targets()
            .unwrap();
        println!("{targets:#?}");
        let mut desktop = super::Desktop::open_selected(&screens[0]).unwrap();
        println!("backend {}", desktop.backend_name());
        let started = std::time::Instant::now();
        let caps = super::probe(&mut desktop, || true).unwrap();
        println!("{caps:?} in {:?}", started.elapsed());
        let started = std::time::Instant::now();
        let frame = desktop.next(100, 4, true, false, (3840, 2160)).unwrap();
        println!(
            "full frame {:?} in {:?}",
            frame.as_ref().map(|f| (f.width, f.height)),
            started.elapsed()
        );
    }
}
