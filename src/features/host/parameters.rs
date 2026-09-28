//! Shared streaming budgets for actual output geometry and negotiated format.
//! Quality is a bandwidth ceiling, not a promise of constant traffic or fidelity.
use super::format::{Codec, Format};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bounds {
    /// Preferred allocation, reduced when the link cannot satisfy all streams.
    pub reservation: u32,
    pub maximum: u32,
    pub initial: u32,
    pub probe: u32,
}
impl Bounds {
    fn video(maximum: u32) -> Self {
        Self {
            reservation: NETWORK_FLOOR.min(maximum),
            maximum,
            initial: (maximum / 5)
                .clamp(NETWORK_FLOOR, STARTUP_CEILING)
                .min(maximum),
            probe: 0,
        }
    }
    pub fn network_maximum(self) -> u32 {
        self.maximum.max(self.probe)
    }
}

pub(crate) const NETWORK_FLOOR: u32 = 300_000;
pub(crate) const STARTUP_CEILING: u32 = 3_000_000;
pub(crate) const MIN_ENCODER_RATE: u32 = 30_000;

// A quality decision threshold, never a network allocation guarantee.
fn downgrade_threshold(maximum: u32) -> u32 {
    (maximum / 5 * 2).clamp(200_000, 15_000_000)
}

pub(crate) use crate::media::geometry::dimensions;

// 60 FPS ceilings in Mbps at 720p, 1080p, 1440p and 4K. Each codec has
// independent quality anchors; compression savings are content-dependent.
// 10-bit preserves gradients without a blanket raw-bit-depth surcharge.
fn standard(format: Format, quality: i32, source: (u32, u32), fps: u32) -> Bounds {
    let rates = match (format.codec, quality) {
        (Codec::H264, 1) => [1.5, 1.5, 1.5, 1.5],
        (Codec::H264, 3) => [8.0, 14.0, 24.0, 24.0],
        (Codec::H264, 4) => [12.0, 20.0, 36.0, 60.0],
        (Codec::H264, _) => [5.5, 8.0, 8.0, 8.0],
        (Codec::H265, 1) => [1.3, 1.3, 1.3, 1.3],
        (Codec::H265, 3) => [7.0, 12.0, 22.0, 22.0],
        (Codec::H265, 4) => [10.0, 17.0, 32.0, 48.0],
        (Codec::H265, _) => [4.5, 7.0, 7.0, 7.0],
        (Codec::Av1, 1) => [1.2, 1.2, 1.2, 1.2],
        (Codec::Av1, 3) => [6.0, 11.0, 18.0, 18.0],
        (Codec::Av1, 4) => [8.0, 15.0, 28.0, 42.0],
        (Codec::Av1, _) => [4.0, 6.0, 6.0, 6.0],
    };
    let size = crate::media::geometry::output_size(source.0, source.1, quality);
    let pixels = u64::from(size.0) * u64::from(size.1);
    let anchors = [1280u64 * 720, 1920 * 1080, 2560 * 1440, 3840 * 2160];
    let base = if pixels <= anchors[0] {
        rates[0] * (pixels as f64 / anchors[0] as f64).max(0.25)
    } else {
        (1..anchors.len())
            .find(|&i| pixels <= anchors[i])
            .map_or(rates[3], |i| {
                let fraction =
                    (pixels - anchors[i - 1]) as f64 / (anchors[i] - anchors[i - 1]) as f64;
                rates[i - 1] + (rates[i] - rates[i - 1]) * fraction
            })
    };
    // Like Moonlight's default-rate curve, high FPS grows sublinearly: adjacent
    // frames share more information. No 30/60/90/120 tier discontinuities.
    let ratio = f64::from(fps.clamp(1, 144)) / 60.0;
    let color = if format.chroma == 3 { 2.0 } else { 1.0 };
    let rate = (base * 1_000_000.0 * color * if fps <= 60 { ratio } else { ratio.sqrt() })
        .round()
        .clamp(500_000.0, 500_000_000.0) as u32;
    Bounds::video(rate)
}

pub(crate) fn fixed(
    format: Format,
    quality: i32,
    custom: u32,
    size: (u32, u32),
    fps: u32,
) -> Bounds {
    if quality != 6 {
        return standard(format, quality, size, fps);
    }
    Bounds::video(custom.clamp(1_000_000, 500_000_000))
}

pub(crate) fn automatic(
    format: Format,
    quality: i32,
    source: (u32, u32),
    fps: u32,
    initial: bool,
) -> Bounds {
    let mut result = standard(format, quality, source, fps);
    result.probe = if quality >= 4 {
        result.maximum
    } else {
        let next = standard(format, quality + 1, source, fps);
        if initial {
            next.maximum
        } else {
            (((f64::from(downgrade_threshold(next.maximum)) + f64::from(next.maximum)) * 0.8)
                as u32)
                .min(next.maximum)
        }
    };
    result
}

pub(crate) struct AutoQuality {
    current: i32,
    maximum: i32,
    startup: i32,
    probes: VecDeque<u32>,
    lower: VecDeque<u32>,
    at: Option<Instant>,
    policy: Option<(Format, (u32, u32), u32, u64)>,
}

#[derive(Clone, Copy)]
pub(crate) struct NetworkSample {
    pub probe: u32,
    pub lower: u32,
    pub loss: f64,
    pub generation: u64,
}
impl AutoQuality {
    pub fn new(startup: i32, maximum: i32) -> Self {
        let maximum = maximum.clamp(1, 4);
        let startup = startup.clamp(1, maximum);
        Self {
            current: startup,
            maximum,
            startup,
            probes: VecDeque::with_capacity(15),
            lower: VecDeque::with_capacity(15),
            at: None,
            policy: None,
        }
    }
    pub fn quality(&self) -> i32 {
        self.current
    }
    pub fn limit(&mut self, maximum: i32) {
        self.maximum = maximum.clamp(1, 4);
        self.current = self.current.min(self.maximum);
    }
    pub fn observe(
        &mut self,
        now: Instant,
        format: Format,
        source: (u32, u32),
        fps: u32,
        has_frames: bool,
        sample: NetworkSample,
    ) -> Option<i32> {
        let policy = (format, source, fps, sample.generation);
        if self.policy != Some(policy) {
            self.policy = Some(policy);
            self.probes.clear();
            self.lower.clear();
            self.at = None;
        }
        if self
            .at
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(1))
        {
            return None;
        }
        self.at = Some(now);
        if !has_frames {
            return None;
        }
        for (window, value) in [
            (&mut self.probes, sample.probe),
            (&mut self.lower, sample.lower),
        ] {
            if window.len() == 15 {
                window.pop_front();
            }
            window.push_back(value);
        }
        if self.startup > 2 && self.probes.len() < 5 {
            return None;
        }
        let mean = |window: &VecDeque<u32>| -> u32 {
            (window.iter().map(|v| u64::from(*v)).sum::<u64>() / window.len() as u64) as u32
        };
        let probe = mean(&self.probes);
        let lower = mean(&self.lower);
        let bounds = standard(format, self.current, source, fps);
        let mut selected = self.current;
        if probe > bounds.maximum && sample.loss <= f64::EPSILON && self.current < self.maximum {
            let next = standard(format, self.current + 1, source, fps);
            if f64::from(probe)
                > (f64::from(downgrade_threshold(next.maximum)) + f64::from(next.maximum)) * 0.5
            {
                selected += 1;
            }
        }
        // The lower-limit decision takes precedence over a simultaneous probe.
        if lower != 0 && lower <= downgrade_threshold(bounds.maximum) {
            selected = (self.current - 1).max(2);
        }
        selected = selected.min(self.maximum);
        if selected == self.current {
            None
        } else {
            self.current = selected;
            Some(selected)
        }
    }
}

/// The explicitly enabled congestion-window admission path is separate from
/// the SDK's disabled generic frame dropper (T34ACBA -> T34772E).
#[derive(Default)]
pub(crate) struct WindowAdmission {
    counter: u64,
}
impl WindowAdmission {
    pub fn next(&mut self, target: u32, minimum: u32, ratio: f64) -> (u32, bool) {
        if !ratio.is_finite() || ratio <= 0.01 || target <= minimum {
            return (target, false);
        }
        let reduce = (target - minimum).min((f64::from(target) * ratio) as u32);
        if reduce == 0 {
            return (target, false);
        }
        let period = u64::from((target / reduce).max(2));
        let drop = self.counter % period == 0;
        self.counter = self.counter.wrapping_add(1);
        (target - target / period as u32, drop)
    }
}
