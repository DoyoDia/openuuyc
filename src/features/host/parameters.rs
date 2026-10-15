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
// 起步速率的上限。它只是给 GoogCC 的 starting_rate，链路吃不下时它会自己退回来；
// 但定得太低，一台千兆局域网也要爬一分钟才用得上链路：实测 500 Mbps 档从 3 Mbps
// 起步，要 90 秒才靠探测爬到 100 Mbps，而在这之前编码器一直按十几 Mbps 出图，
// 画面一有变化就是糊的。码率本身仍然由网络估计决定，这里只改起点的保守程度。
pub(crate) const STARTUP_CEILING: u32 = 20_000_000;
// 链路没有自证过时的起步上限，也就是抬到 20 Mbps 之前一直用的值。探测是按起步
// 速率的 p1=3 / p2=6 倍发的（vendor/goog_cc/src/probe_controller.rs:82-83），
// 20 Mbps 起步就是 60/120 Mbps 的两簇，落在 10 Mbps 级的窄上行上是约 225 KB、
// 上百毫秒的排队；中继路径、量到高 RTT、开局就丢包、以及还没选路的会话都退回这里。
pub(crate) const STARTUP_CONSERVATIVE: u32 = 3_000_000;
pub(crate) const MIN_ENCODER_RATE: u32 = 30_000;

/// 决定起步速率能抬多高的链路证据。构造 Transport 时还没选路，所以默认全是未知。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct LinkEvidence {
    /// None = 还没选路；Some(true) = 选中的是中继路径。
    pub relay: Option<bool>,
    /// 量到的 RTT；None = 还没有任何反馈。
    pub rtt: Option<Duration>,
    /// 反馈里的丢包率（0.0~1.0）。
    pub loss: f64,
}

/// 超过这个 RTT 就不当它是局域网，起步不给大的。
const STARTUP_RTT: Duration = Duration::from_millis(50);
/// 开局就丢包的链路不适合直接抬起步。
const STARTUP_LOSS: f64 = 0.01;

/// 起步速率的上界。只有直连、低 RTT、且没看到丢包时才认链路够宽；其余（含还没
/// 选路）一律保守。这个上界只管起点：只要链路真能吃下，GoogCC 的探测会自己追上去。
pub(crate) fn startup_ceiling(link: LinkEvidence) -> u32 {
    match link.relay {
        Some(false) if link.rtt.is_none_or(|rtt| rtt < STARTUP_RTT) && link.loss < STARTUP_LOSS => {
            STARTUP_CEILING
        }
        _ => STARTUP_CONSERVATIVE,
    }
}

/// `Bounds::initial` 是格式想要的起步量，这里再按链路证据封顶。
pub(crate) fn startup(bounds: Bounds, link: LinkEvidence) -> u32 {
    bounds.initial.min(startup_ceiling(link))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// 直连、5 ms RTT、没有丢包：唯一允许抬起步的证据组合。
    fn proven() -> LinkEvidence {
        LinkEvidence {
            relay: Some(false),
            rtt: Some(Duration::from_millis(5)),
            loss: 0.0,
        }
    }

    #[test]
    fn initial_is_a_fifth_of_the_ceiling_up_to_the_cap() {
        // 五分之一起步是原有逻辑，本改动只把封顶从 3 Mbps 抬到 20 Mbps。
        assert_eq!(Bounds::video(10_000_000).initial, 2_000_000);
        // 五分之一低于地板时被地板托住，所以窄档位本来就在保守值以下。
        assert_eq!(Bounds::video(1_000_000).initial, NETWORK_FLOOR);
        assert_eq!(Bounds::video(15_000_000).initial, STARTUP_CONSERVATIVE);
        // 上限 15 Mbps 是分界：再高的档位起步被 20 Mbps 截住。
        assert_eq!(Bounds::video(20_000_000).initial, 4_000_000);
        assert_eq!(Bounds::video(100_000_000).initial, STARTUP_CEILING);
        assert_eq!(Bounds::video(500_000_000).initial, STARTUP_CEILING);
    }

    #[test]
    fn only_a_proven_link_gets_the_high_start() {
        let wide = Bounds::video(500_000_000);
        assert_eq!(startup(wide, proven()), STARTUP_CEILING);
        // 还没选路。
        assert_eq!(startup(wide, LinkEvidence::default()), STARTUP_CONSERVATIVE);
        // 中继。
        assert_eq!(
            startup(
                wide,
                LinkEvidence {
                    relay: Some(true),
                    ..proven()
                }
            ),
            STARTUP_CONSERVATIVE
        );
        // 高 RTT。
        assert_eq!(
            startup(
                wide,
                LinkEvidence {
                    rtt: Some(Duration::from_millis(120)),
                    ..proven()
                }
            ),
            STARTUP_CONSERVATIVE
        );
        // 已经在丢包。
        assert_eq!(
            startup(
                wide,
                LinkEvidence {
                    loss: 0.02,
                    ..proven()
                }
            ),
            STARTUP_CONSERVATIVE
        );
        // RTT 还没量到不算反证：直连时先按上限起步，量到高 RTT 再退。
        assert_eq!(
            startup(
                wide,
                LinkEvidence {
                    rtt: None,
                    ..proven()
                }
            ),
            STARTUP_CEILING
        );
    }

    #[test]
    fn narrow_sessions_are_unaffected_by_the_evidence() {
        // 上限不到 15 Mbps 的档位，起步本来就在保守值以下，分档对它们没有影响。
        for maximum in [1_000_000, 5_000_000, 15_000_000] {
            let bounds = Bounds::video(maximum);
            assert_eq!(startup(bounds, LinkEvidence::default()), bounds.initial);
            assert_eq!(startup(bounds, proven()), bounds.initial);
        }
    }
}
