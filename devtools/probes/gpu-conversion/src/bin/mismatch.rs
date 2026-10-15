//! Measure the cost of a size mismatch on the GPU conversion path.
//!
//! Production converts the compositor frame to NV12 at its native size. When
//! the negotiated output size differs (a controller quality that maps to a
//! smaller geometry), `capture/mod.rs` falls back to `OwnedNv12::pixels()` --
//! a VA readback to BGRA -- followed by a CPU bilinear `scale`. This probe
//! compares that path with the native-size fast path so the penalty is known.
#![allow(dead_code)]
use std::{io::Write, os::fd::AsRawFd, time::Instant};

mod media {
    pub mod encoding {
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub enum Codec {
            H264,
        }
        #[derive(Clone, Copy, Debug)]
        pub struct Format {
            pub codec: Codec,
            pub chroma: u8,
            pub depth: u8,
        }
        impl Format {
            pub const AVC: Self = Self {
                codec: Codec::H264,
                chroma: 1,
                depth: 8,
            };
            pub fn valid(self) -> bool {
                true
            }
        }
        #[derive(Clone, Copy)]
        pub struct Rate {
            pub target: u32,
            pub peak: u32,
            pub fps: u32,
            pub quality: i32,
        }
        pub struct Encoded {
            pub data: Vec<u8>,
            pub keyframe: bool,
            pub timestamp_100ns: i64,
            pub is_new: bool,
            pub timing: Option<()>,
            pub format: Format,
            pub color: (),
        }
        pub enum Color {
            Sdr,
        }
        impl Color {
            pub fn space(self, _: Option<()>) {}
        }
    }
}
#[path = "../../../../../src/platform/linux/vaapi_encoder.rs"]
mod encoder;
#[path = "../../../../../src/platform/linux/gpu_convert.rs"]
mod gpu_convert;
/// The probe does no CUDA work beside VA-API, so there is no `cuInit` to
/// order before the VA driver loads.
mod cuda {
    pub fn initialize_before_va() {}
}

fn rate() -> media::encoding::Rate {
    media::encoding::Rate {
        target: 8_000_000,
        peak: 8_000_000,
        fps: 30,
        quality: 4,
    }
}

/// Verbatim copy of the capture path's bilinear BGRA scaling, so the probe
/// charges the same CPU work the product does.
fn scale(source: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let (source_width, source_height) = (from.0 as usize, from.1 as usize);
    let (width, height) = (to.0 as usize, to.1 as usize);
    let mut out = vec![0u8; width * height * 4];
    if width <= source_width
        && height <= source_height
        && (source_width - width) <= 1
        && (source_height - height) <= 1
    {
        for y in 0..height {
            let start = y * source_width * 4;
            out[y * width * 4..(y + 1) * width * 4]
                .copy_from_slice(&source[start..start + width * 4]);
        }
        return out;
    }
    let step_x = ((source_width as u64) << 16) / width as u64;
    let step_y = ((source_height as u64) << 16) / height as u64;
    let max_x = source_width - 1;
    let max_y = source_height - 1;
    for y in 0..height {
        let sy = ((y as u64 * step_y) + (step_y >> 1)).saturating_sub(1 << 15);
        let y0 = ((sy >> 16) as usize).min(max_y);
        let y1 = (y0 + 1).min(max_y);
        let fy = (sy & 0xffff) as u32;
        for x in 0..width {
            let sx = ((x as u64 * step_x) + (step_x >> 1)).saturating_sub(1 << 15);
            let x0 = ((sx >> 16) as usize).min(max_x);
            let x1 = (x0 + 1).min(max_x);
            let fx = (sx & 0xffff) as u32;
            let at = |px: usize, py: usize| (py * source_width + px) * 4;
            let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
            let o = (y * width + x) * 4;
            for channel in 0..4 {
                let top = u32::from(source[a + channel]) * (0x10000 - fx)
                    + u32::from(source[b + channel]) * fx;
                let bottom = u32::from(source[c + channel]) * (0x10000 - fx)
                    + u32::from(source[d + channel]) * fx;
                let value = ((u64::from(top >> 8) * u64::from(0x10000 - fy)
                    + u64::from(bottom >> 8) * u64::from(fy))
                    >> 24) as u32;
                out[o + channel] = value.min(255) as u8;
            }
        }
    }
    out
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let w: u32 = args.get(1).map_or(Ok(2880), |s| s.parse())?;
    let h: u32 = args.get(2).map_or(Ok(1800), |s| s.parse())?;
    let tw: u32 = args.get(3).map_or(Ok(1728), |s| s.parse())?;
    let th: u32 = args.get(4).map_or(Ok(1080), |s| s.parse())?;
    let frames: u32 = args.get(5).map_or(Ok(96), |s| s.parse())?;

    let display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
    let source = display
        .create_surfaces(
            cros_libva::VA_RT_FORMAT_RGB32,
            Some(cros_libva::VA_FOURCC_BGRA),
            w,
            h,
            None,
            vec![()],
        )?
        .remove(0);
    let desc = source.export_prime_writable()?;
    let rgb = gpu_convert::RgbDma {
        fd: desc.objects[0].fd.as_raw_fd(),
        width: w,
        height: h,
        pitch: desc.layers[0].pitch[0],
        offset: desc.layers[0].offset[0],
        fourcc: u32::from_le_bytes(*b"AR24"),
        modifier: desc.objects[0].drm_format_modifier,
    };
    {
        let mut image = cros_libva::Image::derive_from(&source, (w, h))?;
        let p = *image.image();
        for y in 0..h as usize {
            let row = p.offsets[0] as usize + y * p.pitches[0] as usize;
            for x in 0..w as usize {
                let v = ((x * 255 / w as usize) & 0xff) as u8;
                image.as_mut()[row + x * 4..row + x * 4 + 4].copy_from_slice(&[
                    v,
                    (y & 0xff) as u8,
                    v,
                    255,
                ]);
            }
        }
    }
    let gpu = gpu_convert::Converter::new(display.clone())?;
    let enc_display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;

    // Fast path: encoder opened at the source size, NV12 handed over directly.
    let mut enc_native =
        encoder::Encoder::new(&enc_display, (w, h), media::encoding::Format::AVC, rate())?;
    let mut out = std::fs::File::create(format!("/tmp/uu-mismatch-{w}x{h}-{tw}x{th}.h264"))?;
    let (mut t_convert, mut t_encode) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    for n in 0..frames {
        let t = Instant::now();
        let nv = gpu.convert(&rgb, (0, 0, w, h), (w, h))?;
        t_convert += t.elapsed();
        let t = Instant::now();
        let encoded = enc_native.encode_nv12(&nv, 10_000_000 + n as i64 * 333_333, n % 30 == 0)?;
        t_encode += t.elapsed();
        for packet in encoded {
            out.write_all(&packet.data)?;
        }
    }
    println!(
        "native   {w}x{h}  convert={:.2} encode={:.2} total={:.2} ms/frame",
        t_convert.as_secs_f64() * 1000. / f64::from(frames),
        t_encode.as_secs_f64() * 1000. / f64::from(frames),
        (t_convert + t_encode).as_secs_f64() * 1000. / f64::from(frames),
    );
    drop(enc_native);

    // Mismatch path, now scaled on the GPU: convert straight to the target
    // geometry and hand the NV12 over, with no CPU readback or scale.
    let mut enc_scaled =
        encoder::Encoder::new(&enc_display, (tw, th), media::encoding::Format::AVC, rate())?;
    let (mut t_convert, mut t_encode) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    for n in 0..frames {
        let t = Instant::now();
        let nv = gpu.convert(&rgb, (0, 0, w, h), (tw, th))?;
        t_convert += t.elapsed();
        let t = Instant::now();
        let encoded = enc_scaled.encode_nv12(&nv, 10_000_000 + n as i64 * 333_333, n % 30 == 0)?;
        t_encode += t.elapsed();
        for packet in encoded {
            out.write_all(&packet.data)?;
        }
    }
    println!(
        "gpu-scale {w}x{h}->{tw}x{th}  convert={:.2} encode={:.2} total={:.2} ms/frame",
        t_convert.as_secs_f64() * 1000. / f64::from(frames),
        t_encode.as_secs_f64() * 1000. / f64::from(frames),
        (t_convert + t_encode).as_secs_f64() * 1000. / f64::from(frames),
    );
    Ok(())
}
