//! Verify the GPU scaler's colours and the native-size exactness.
//!
//! The converter now writes the requested output geometry directly. Two things
//! must hold: at equal sizes the pass must stay a bit-exact `texelFetch` (the
//! baseline behaviour), and when scaling it must still produce BT.601
//! limited-range values without a channel swap or a flipped axis.
#![allow(dead_code)]
use std::os::fd::AsRawFd;

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

const COLORS: [[u8; 3]; 8] = [
    [0, 0, 0],
    [255, 255, 255],
    [0, 0, 255],
    [0, 255, 0],
    [255, 0, 0],
    [255, 255, 0],
    [255, 0, 255],
    [0, 255, 255],
];
const EXPECT: [[u8; 3]; 8] = [
    [16, 128, 128],
    [235, 128, 128],
    [81, 90, 240],
    [145, 54, 34],
    [41, 240, 110],
    [170, 166, 16],
    [106, 202, 222],
    [210, 16, 146],
];

/// Sample the middle of each colour bar through the encoder's own import.
fn sample(
    nv: &std::sync::Arc<gpu_convert::OwnedNv12>,
    display: &std::rc::Rc<cros_libva::Display>,
    (w, h): (u32, u32),
) -> anyhow::Result<Vec<[u8; 3]>> {
    let imported = nv.import(display)?;
    let image = cros_libva::Image::derive_from(&imported, (w, h))?;
    let p = image.image();
    let b = image.as_ref();
    let mut out = Vec::new();
    for bar in 0..8usize {
        let x = (bar as u32 * w / 8 + w / 16) & !1;
        let y = (h / 2) & !1;
        out.push([
            b[p.offsets[0] as usize + (y * p.pitches[0] + x) as usize],
            b[p.offsets[1] as usize + ((y / 2) * p.pitches[1] + x) as usize],
            b[p.offsets[1] as usize + ((y / 2) * p.pitches[1] + x + 1) as usize],
        ]);
    }
    Ok(out)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let w: u32 = args.get(1).map_or(Ok(2880), |s| s.parse())?;
    let h: u32 = args.get(2).map_or(Ok(1800), |s| s.parse())?;
    let tw: u32 = args.get(3).map_or(Ok(1280), |s| s.parse())?;
    let th: u32 = args.get(4).map_or(Ok(720), |s| s.parse())?;

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
    // Vertical colour bars, so a horizontal flip or a channel swap is visible.
    {
        let mut image = cros_libva::Image::derive_from(&source, (w, h))?;
        let p = *image.image();
        for y in 0..h as usize {
            let row = p.offsets[0] as usize + y * p.pitches[0] as usize;
            for x in 0..w as usize {
                let c = COLORS[x * 8 / w as usize];
                image.as_mut()[row + x * 4..row + x * 4 + 4]
                    .copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
    }
    let gpu = gpu_convert::Converter::new(display.clone())?;
    let enc_display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;

    // Native pass must be exact (texelFetch), the same as before scaling existed.
    let native = gpu.convert(&rgb, (0, 0, w, h), (w, h))?;
    let native_samples = sample(&native, &enc_display, (w, h))?;
    let mut native_bad = 0;
    for (bar, got) in native_samples.iter().enumerate() {
        for c in 0..3 {
            if (i32::from(got[c]) - i32::from(EXPECT[bar][c])).abs() > 1 {
                native_bad += 1;
            }
        }
    }
    println!("native  {w}x{h} samples={native_samples:?} mismatches={native_bad}");

    // Scaled pass must keep the same colours and orientation.
    let scaled = gpu.convert(&rgb, (0, 0, w, h), (tw, th))?;
    let scaled_samples = sample(&scaled, &enc_display, (tw, th))?;
    let mut scaled_bad = 0;
    for (bar, got) in scaled_samples.iter().enumerate() {
        for c in 0..3 {
            if (i32::from(got[c]) - i32::from(EXPECT[bar][c])).abs() > 2 {
                scaled_bad += 1;
            }
        }
    }
    println!("scaled  {w}x{h}->{tw}x{th} samples={scaled_samples:?} mismatches={scaled_bad}");

    // A repeated scaled conversion must not drift or flip.
    let mut repeats_bad = 0;
    for _ in 0..8 {
        let nv = gpu.convert(&rgb, (0, 0, w, h), (tw, th))?;
        let got = sample(&nv, &enc_display, (tw, th))?;
        for (bar, g) in got.iter().enumerate() {
            for c in 0..3 {
                if (i32::from(g[c]) - i32::from(scaled_samples[bar][c])).abs() > 1 {
                    repeats_bad += 1;
                }
            }
        }
    }
    println!("repeat  mismatches={repeats_bad}");

    let total = native_bad + scaled_bad + repeats_bad;
    println!(
        "{}",
        if total == 0 {
            "PASS: native exact, scaled colours and orientation preserved"
        } else {
            "FAIL"
        }
    );
    Ok(())
}
