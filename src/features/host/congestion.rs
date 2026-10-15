//! Sender transport feedback -> GoogCC. Times are taken at actual packet egress.
use goog_cc::{
    AlrDetectorConfig, GoogCcConfig, GoogCcNetworkController,
    experiments::FieldTrials,
    network_control::{NetworkControllerConfig, NetworkControllerInterface},
    transport::*,
    units::{DataRate, DataSize, TimeDelta, Timestamp},
};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};
use webrtc::rtcp::transport_feedbacks::transport_layer_cc::{
    PacketStatusChunk, SymbolTypeTcc, TransportLayerCc,
};

/// 交给 goog_cc 的现场试验。抽成函数是为了让测试能直接检查这些字段：写在这里的
/// 每一项都必须真的有读取方，否则就是永远不会生效的死配置。
fn field_trials() -> FieldTrials {
    let mut field_trials = FieldTrials::default();
    // T541A30 defaults robust estimator OFF in this UU SDK.
    field_trials.robust_throughput_estimator_settings.enabled = false;
    field_trials.rapid_recovery_experiment = true;
    field_trials.no_bitrate_increase_in_alr = false;
    field_trials.add_pacing_to_congestion_window_pushback = true;
    // alr_experiment_settings 那一组（WebRTC-ProbingScreenshareBwe）在这个 Rust
    // port 里没有任何读取方：libwebrtc 是靠 UpdateAlrExperimentSettings 把它们推给
    // AlrDetector 的，移植时这一步丢了。真正生效的是 goog_cc_network_control.rs:176
    // 读的 alr_detector_parameters，所以原来写在那里的 85/95/-60 从来没生效过，
    // ALR 一直按库默认的 0.80/0.40/-0.60 判定。
    //
    // 这里显式写出实际生效的值，而不是把 95 搬过来：start_budget_level_ratio 从
    // 0.40 抬到 0.95 会让 ALR 变得难进，而 ALR 正是 app-limited 的共享桌面唯一的
    // 上探通道——周期性 ALR 探测的前提就是 in_alr（probe_controller.rs:635），
    // 进不去 ALR 就只剩 AIMD 那条 1.5 × 实测接收速率的天花板。要动这个阈值得先有
    // 弱网数据，不在这个改动里动。
    field_trials.alr_detector_parameters = AlrDetectorConfig {
        bandwidth_usage_ratio: 0.80,
        start_budget_level_ratio: 0.40,
        stop_budget_level_ratio: -0.60,
    };
    // ALR 周期性探测的频率与步长暂时留在库默认（5 秒 / 2 倍）：这个旋钮直接
    // 决定弱网下每次探测压进瓶颈多少数据，单独一个提交再打开，好单独评估。
    field_trials.probing_configuration.probe_max_allocation = true;
    field_trials.min_alloc_as_lower_bound = false;
    field_trials.ignore_probes_lower_than_network_state_estimate = false;
    field_trials.limit_probes_lower_than_throughput_estimate = false;
    field_trials.safe_reset_on_route_change.enabled = false;
    field_trials.loss_based_control.enabled = false;
    field_trials.loss_based_bwe_v2.enabled = false;
    field_trials
}

pub(crate) struct Controller {
    core: GoogCcNetworkController,
    origin: Instant,
    maximum: u32,
    bounds: super::parameters::Bounds,
    video_configured: bool,
    sequence: i64,
    history: BTreeMap<i64, Tracked>,
    flight: usize,
    reference: Option<i64>,
    remote_offset: Option<i64>,
    available: bool,
    generation: u64,
    route_at: Timestamp,
    pub target: u32,
    pub pacing: u32,
    pub loss: f64,
    pub rtt: TimeDelta,
    pub cwnd: usize,
    pub cwnd_reduce_ratio: f64,
    pub probes: VecDeque<ProbeClusterConfig>,
    pressure: Pressure,
    burst: super::burst::Burst,
    pub feedback_count: u64,
}
struct Tracked {
    packet: SentPacket,
    reported: Option<bool>,
    burst: Option<u64>,
}

impl Controller {
    pub fn new(bounds: super::parameters::Bounds) -> Self {
        let maximum = bounds.network_maximum();
        let field_trials = field_trials();
        let at = Timestamp::from_micros(1_000_000);
        // ALR 判定阈值只有 alr_detector_parameters 会被读到，打出来免得再有人
        // 以为 field_trials 里那组 screenshare 参数有效。
        tracing::info!(
            maximum_bps = maximum,
            startup_bps = bounds.initial,
            alr_usage_ratio = field_trials.alr_detector_parameters.bandwidth_usage_ratio,
            alr_start_ratio = field_trials.alr_detector_parameters.start_budget_level_ratio,
            alr_stop_ratio = field_trials.alr_detector_parameters.stop_budget_level_ratio,
            alr_interval_ms = field_trials.probing_configuration.alr_interval.ms(),
            alr_scale = field_trials.probing_configuration.alr_scale,
            "host congestion controller created"
        );
        let core = GoogCcNetworkController::new(
            NetworkControllerConfig {
                field_trials,
                constraints: constraints(at, maximum, Some(bounds.initial)),
                stream_based_config: StreamsConfig {
                    at_time: at,
                    pacing_factor: Some(1.6),
                    requests_alr_probing: Some(true),
                    max_total_allocated_bitrate: Some(DataRate::from_bits_per_sec(maximum.into())),
                    min_total_allocated_bitrate: Some(DataRate::from_bits_per_sec(
                        bounds.reservation.into(),
                    )),
                    ..Default::default()
                },
            },
            GoogCcConfig {
                feedback_only: true,
            },
        );
        Self {
            core,
            origin: Instant::now(),
            maximum,
            bounds,
            video_configured: bounds.maximum > 0,
            sequence: rand::random::<u16>().into(),
            history: BTreeMap::new(),
            flight: 0,
            reference: None,
            remote_offset: None,
            available: false,
            generation: 0,
            route_at: at,
            target: bounds.initial,
            pacing: bounds.initial.saturating_mul(16) / 10,
            loss: 0.0,
            rtt: TimeDelta::from_millis(50),
            cwnd: usize::MAX,
            cwnd_reduce_ratio: 0.,
            probes: VecDeque::new(),
            pressure: Pressure::default(),
            burst: super::burst::Burst::default(),
            feedback_count: 0,
        }
    }
    fn now(&self) -> Timestamp {
        Timestamp::from_micros(
            1_000_000
                + self
                    .origin
                    .elapsed()
                    .as_micros()
                    .min(i64::MAX as u128 - 1_000_000) as i64,
        )
    }
    fn apply(&mut self, update: NetworkControlUpdate) {
        if let Some(rate) = update.target_rate {
            let previous = self.target;
            self.target = rate.target_rate.bps_or(0).clamp(0, self.maximum as i64) as u32;
            self.cwnd_reduce_ratio = rate.cwnd_reduce_ratio.clamp(0., 1.);
            self.loss = f64::from(rate.network_estimate.loss_rate_ratio).clamp(0.0, 1.0);
            if rate.network_estimate.round_trip_time.is_finite() {
                self.rtt = rate.network_estimate.round_trip_time;
            }
            if self.target != previous {
                // ALR 状态和「限制估计值的原因」决定这次探测能不能发出去：探测被
                // 静默抑制时（probe_controller.rs:576 里 DelayBasedLimitedDelay-
                // Increased），只有这两项能说明为什么。
                let (alr, cause) = self.core.probing_state();
                tracing::debug!(
                    target_bps = self.target,
                    previous_bps = previous,
                    bounded_target_bps = self.core.bounded_target_rate().bps_or(0),
                    alr,
                    cause = ?cause,
                    delay_state = ?self.core.delay_state(),
                    loss = self.loss,
                    rtt_ms = self.rtt.ms(),
                    "host bandwidth estimate changed"
                );
            }
            if previous >= 600_000 && self.target < previous / 2 {
                tracing::info!(
                    previous_bps = previous,
                    target_bps = self.target,
                    bounded_target_bps = self.core.bounded_target_rate().bps_or(0),
                    loss = self.loss,
                    rtt_ms = self.rtt.ms(),
                    delay_overuse = self.delay_overuse(),
                    link_pressure = self.link_pressure(),
                    cwnd_reduce_ratio = self.cwnd_reduce_ratio,
                    "host bandwidth estimate dropped"
                );
            }
        }
        if let Some(pacer) = update.pacer_config {
            self.pacing = pacer
                .data_rate()
                .bps_or(self.maximum as i64)
                .clamp(1, u32::MAX as i64) as u32;
        }
        if let Some(cwnd) = update.congestion_window {
            self.cwnd = cwnd.bytes_or(i64::MAX).max(0) as usize;
        }
        self.probes.extend(update.probe_cluster_configs);
    }
    pub fn network(&mut self, available: bool) {
        if self.available == available {
            return;
        }
        self.available = available;
        self.generation = self.generation.wrapping_add(1);
        let update = self.core.on_network_availability(NetworkAvailability {
            at_time: self.now(),
            network_available: available,
        });
        self.apply(update);
        if !available {
            self.probes.clear();
        }
    }
    pub fn route_changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.route_at = self.now();
        let update = self.core.on_network_route_change(NetworkRouteChange {
            at_time: self.now(),
            constraints: constraints(self.now(), self.maximum, Some(self.bounds.initial)),
        });
        self.history.clear();
        self.flight = 0;
        self.reference = None;
        self.remote_offset = None;
        self.pressure = Pressure::default();
        self.burst = super::burst::Burst::default();
        self.probes.clear();
        self.apply(update);
    }
    pub fn configure(&mut self, bounds: super::parameters::Bounds, video: bool) {
        // File/audio connections may attach their first video later. Seed that
        // first video once, but never treat a quality/FPS revision as a route
        // change: a starting_rate overwrites both live bandwidth estimators.
        let start = (video && !self.video_configured).then_some(bounds.initial);
        self.video_configured |= video;
        if bounds == self.bounds && start.is_none() {
            return;
        }
        let maximum = bounds.network_maximum();
        self.maximum = maximum;
        self.bounds = bounds;
        let update = self
            .core
            .on_target_rate_constraints(constraints(self.now(), maximum, start));
        self.apply(update);
        let update = self.core.on_streams_config(StreamsConfig {
            at_time: self.now(),
            max_total_allocated_bitrate: Some(DataRate::from_bits_per_sec(maximum.into())),
            min_total_allocated_bitrate: Some(DataRate::from_bits_per_sec(
                bounds.reservation.into(),
            )),
            ..Default::default()
        });
        self.apply(update);
    }
    pub fn estimates(&self) -> (u32, u32, f64, u64) {
        (
            self.target,
            self.core
                .bounded_target_rate()
                .bps_or(0)
                .clamp(0, self.maximum as i64) as u32,
            self.loss,
            self.generation,
        )
    }
    pub fn next_sequence(&mut self) -> i64 {
        let seq = self.sequence;
        self.sequence += 1;
        seq
    }
    pub fn sent(
        &mut self,
        sequence: i64,
        bytes: usize,
        pacing: PacedPacketInfo,
        burst: Option<u64>,
        audio: bool,
    ) {
        let now = self.now();
        if let Some(group) = burst {
            self.burst.sent(group, sequence, bytes, now.us());
        }
        let sent = SentPacket {
            audio,
            send_time: now,
            size: DataSize::from_bytes(bytes as i64),
            pacing_info: pacing,
            sequence_number: sequence,
            data_in_flight: DataSize::from_bytes(self.flight as i64),
            ..Default::default()
        };
        self.flight = self.flight.saturating_add(bytes);
        self.history.insert(
            sequence,
            Tracked {
                packet: sent,
                reported: None,
                burst,
            },
        );
        while self.history.first_key_value().is_some_and(|(_, p)| {
            self.history.len() > 32768 || (now - p.packet.send_time) > TimeDelta::from_seconds(10)
        }) {
            if let Some((_, old)) = self.history.pop_first() {
                if old.reported.is_none() {
                    self.flight = self.flight.saturating_sub(old.packet.size.bytes() as usize);
                }
            }
        }
        let update = self.core.on_sent_packet(sent);
        self.apply(update);
    }
    pub fn tick(&mut self, queued_bytes: usize) {
        self.burst.tick(self.now().us(), self.rtt.us().max(0));
        let update = self.core.on_process_interval(ProcessInterval {
            at_time: self.now(),
            pacer_queue: Some(DataSize::from_bytes(queued_bytes as i64)),
        });
        self.apply(update);
    }
    pub fn burst_bytes(&self) -> usize {
        self.burst.bytes
    }
    pub fn delay_overuse(&self) -> bool {
        self.core.delay_state() == BandwidthUsage::Overusing
    }
    pub fn probe_is_current(&self, cluster: &ProbeClusterConfig) -> bool {
        self.available
            && cluster.at_time >= self.route_at
            && self.now() - cluster.at_time <= TimeDelta::from_seconds(5)
    }
    pub fn link_pressure(&self) -> bool {
        self.pressure.state == 2
    }
    pub fn feedback(&mut self, feedback: &TransportLayerCc) {
        if feedback.packet_status_count == 0 {
            return;
        }
        let now = self.now();
        let mut statuses = Vec::with_capacity(feedback.packet_status_count as usize);
        for chunk in &feedback.packet_chunks {
            let remaining = feedback.packet_status_count as usize - statuses.len();
            match chunk {
                PacketStatusChunk::RunLengthChunk(c) => statuses.extend(std::iter::repeat_n(
                    c.packet_status_symbol,
                    remaining.min(c.run_length as usize),
                )),
                PacketStatusChunk::StatusVectorChunk(c) => {
                    statuses.extend(c.symbol_list.iter().copied().take(remaining))
                }
            }
            if statuses.len() == feedback.packet_status_count as usize {
                break;
            }
        }
        if statuses.len() != feedback.packet_status_count as usize {
            return;
        }
        let delta_count = statuses
            .iter()
            .filter(|s| {
                matches!(
                    s,
                    SymbolTypeTcc::PacketReceivedSmallDelta
                        | SymbolTypeTcc::PacketReceivedLargeDelta
                )
            })
            .count();
        if delta_count != feedback.recv_deltas.len() {
            return;
        }
        let reference = unwrap(
            feedback.reference_time as i64,
            24,
            self.reference.unwrap_or(feedback.reference_time as i64),
        );
        self.reference = Some(reference);
        let delta_sum: i64 = feedback.recv_deltas.iter().map(|d| d.delta).sum();
        let offset = *self
            .remote_offset
            .get_or_insert(now.us() - reference * 64_000 - delta_sum);
        let mut received_at = reference * 64_000 + offset;
        let base = unwrap(feedback.base_sequence_number.into(), 16, self.sequence - 1);
        let mut deltas = feedback.recv_deltas.iter();
        let mut packets = Vec::new();
        for (index, status) in statuses.into_iter().enumerate() {
            let received = status != SymbolTypeTcc::PacketNotReceived;
            if matches!(
                status,
                SymbolTypeTcc::PacketReceivedSmallDelta | SymbolTypeTcc::PacketReceivedLargeDelta
            ) {
                received_at += deltas.next().expect("validated TWCC delta count").delta;
            }
            let Some(stored) = self.history.get_mut(&(base + index as i64)) else {
                continue;
            };
            if stored.reported == Some(true) || stored.reported == Some(received) {
                continue;
            }
            if stored.reported.is_none() {
                self.flight = self
                    .flight
                    .saturating_sub(stored.packet.size.bytes() as usize);
            }
            stored.reported = Some(received);
            if let Some(group) = stored.burst {
                self.burst
                    .feedback(group, stored.packet.sequence_number, received, now.us());
            }
            if status == SymbolTypeTcc::PacketReceivedWithoutDelta {
                continue;
            }
            packets.push(PacketResult {
                sent_packet: stored.packet,
                receive_time: if received {
                    Timestamp::from_micros(received_at)
                } else {
                    Timestamp::plus_infinity()
                },
                ..Default::default()
            });
        }
        if packets.is_empty() {
            return;
        }
        self.feedback_count = self.feedback_count.saturating_add(1);
        packets.sort_by_key(|p| p.sent_packet.send_time);
        self.pressure.update(&packets);
        self.core
            .set_random_loss_no_backoff(self.pressure.random_loss);
        let update = self
            .core
            .on_transport_packets_feedback(TransportPacketsFeedback {
                feedback_time: now,
                data_in_flight: DataSize::from_bytes(self.flight as i64),
                packet_feedbacks: packets,
                sendless_arrival_times: Vec::new(),
            });
        self.apply(update);
    }
}
fn constraints(at_time: Timestamp, maximum: u32, start: Option<u32>) -> TargetRateConstraints {
    TargetRateConstraints {
        at_time,
        // UU product field trial PcFactoryDefaultBitrates/min:300 (kbps).
        min_data_rate: Some(DataRate::from_bits_per_sec(
            super::parameters::NETWORK_FLOOR.min(maximum).into(),
        )),
        max_data_rate: Some(DataRate::from_bits_per_sec(maximum.into())),
        starting_rate: start.map(|v| DataRate::from_bits_per_sec(v.into())),
    }
}
fn unwrap(value: i64, bits: u32, reference: i64) -> i64 {
    let modulus = 1i64 << bits;
    let mut extended = (reference & !(modulus - 1)) | value;
    if extended - reference > modulus / 2 {
        extended -= modulus;
    }
    if reference - extended > modulus / 2 {
        extended += modulus;
    }
    extended
}

#[derive(Default)]
struct Pressure {
    end: Option<i64>,
    groups: BTreeMap<i64, BTreeMap<i64, bool>>,
    state: u8,
    candidate: Option<i64>,
    confirmations: u32,
    last_pressure: Option<i64>,
    clear_at: Option<i64>,
    clear_count: u32,
    random_loss: bool,
}
impl Pressure {
    fn update(&mut self, packets: &[PacketResult]) {
        let previous = self.end;
        for packet in packets.iter().filter(|p| !p.sent_packet.audio) {
            let group = packet.sent_packet.send_time.us() / 100_000;
            if self.end.is_some_and(|end| group - end >= 100) {
                *self = Self::default();
            }
            self.end = Some(self.end.map_or(group, |end| end.max(group)));
            if self.end.is_some_and(|end| group < end - 20) {
                continue;
            }
            let received = self
                .groups
                .entry(group)
                .or_default()
                .entry(packet.sent_packet.sequence_number)
                .or_insert(false);
            *received |= packet.is_received();
        }
        let Some(end) = self.end else { return };
        while self
            .groups
            .first_key_value()
            .is_some_and(|(&group, _)| group < end - 20)
        {
            self.groups.pop_first();
        }
        if previous == Some(end) {
            return;
        }
        let now = end * 100_000;
        let valid: Vec<(u32, u32)> = self
            .groups
            .range(end - 20..end)
            .filter(|(_, packets)| packets.len() >= 10)
            .map(|(_, packets)| {
                (
                    packets.len() as u32,
                    packets.values().filter(|&&received| !received).count() as u32,
                )
            })
            .collect();
        let total: u32 = valid.iter().map(|(s, _)| *s).sum();
        let mut correlation = None;
        let mut loss = 0.0;
        if valid.len() >= 14 && total >= 200 {
            let n = valid.len() as f64;
            let sent_mean = total as f64 / n;
            loss = valid
                .iter()
                .map(|(s, l)| *l as f64 / *s as f64)
                .sum::<f64>()
                / n;
            let (mut xx, mut yy, mut xy) = (0.0, 0.0, 0.0);
            for (sent, lost) in &valid {
                let x = *sent as f64 - sent_mean;
                let y = *lost as f64 / *sent as f64 - loss;
                xx += x * x;
                yy += y * y;
                xy += x * y;
            }
            if xx / n / (sent_mean * sent_mean) >= 0.01
                && yy / n >= 0.000001
                && xx > 0.0
                && yy > 0.0
            {
                correlation = Some(xy / (xx * yy).sqrt());
            }
        }
        let low_loss = valid.len() >= 20 && loss < 0.03;
        let over = !low_loss && correlation.is_some_and(|r| r > 0.6);
        match self.state {
            0 if over => {
                self.state = 1;
                self.candidate = Some(now);
                self.confirmations = 1;
            }
            1 if over => {
                self.confirmations += 1;
                if self.confirmations >= 6 || self.candidate.is_some_and(|at| now - at >= 1_000_000)
                {
                    self.state = 2;
                    self.last_pressure = Some(now);
                    self.clear_at = None;
                    self.clear_count = 0;
                }
            }
            1 => {
                self.confirmations = 0;
                if low_loss || self.candidate.is_some_and(|at| now - at >= 1_000_000) {
                    self.state = 0;
                    self.candidate = None;
                }
            }
            2 => {
                if low_loss {
                    let at = *self.clear_at.get_or_insert(now);
                    self.clear_count += 1;
                    if self.clear_count >= 8 || now - at >= 800_000 {
                        self.state = 0;
                    }
                } else {
                    self.clear_at = None;
                    self.clear_count = 0;
                    if correlation.is_some_and(|r| r > 0.5) {
                        self.last_pressure = Some(now);
                    }
                }
                if self.last_pressure.is_some_and(|at| now - at >= 3_000_000) {
                    self.state = 0;
                }
            }
            _ => {}
        }
        if self.state == 2 {
            self.random_loss = false;
        } else if let Some(r) = correlation {
            self.random_loss = loss >= 0.03 && r < 0.6;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alr_thresholds_are_written_where_goog_cc_reads_them() {
        let trials = field_trials();
        // AlrDetector 是从 alr_detector_parameters 构造的
        // (vendor/goog_cc/src/goog_cc_network_control.rs:176)，这里必须和实际生效
        // 的库默认一致：抬高 start_budget_level_ratio 会让 ALR 难进，也就堵死了
        // app-limited 场景下唯一的上探通道。
        assert_eq!(trials.alr_detector_parameters.bandwidth_usage_ratio, 0.80);
        assert_eq!(
            trials.alr_detector_parameters.start_budget_level_ratio,
            0.40
        );
        assert_eq!(
            trials.alr_detector_parameters.stop_budget_level_ratio,
            -0.60
        );
        // 对照组：alr_experiment_settings 那一组在 vendor/goog_cc 里没有任何读取
        // 方，写它不会有任何效果，所以这里必须保持默认——真写了才是 bug。
        let inert = goog_cc::experiments::AlrExperimentSettings::default();
        assert_eq!(
            trials
                .alr_experiment_settings
                .alr_start_budget_level_percent,
            inert.alr_start_budget_level_percent
        );
        assert_eq!(
            trials.alr_experiment_settings.alr_bandwidth_usage_percent,
            inert.alr_bandwidth_usage_percent
        );
        assert_eq!(
            trials.alr_experiment_settings.pacing_factor,
            inert.pacing_factor
        );
    }
}
