//! One encoded jitter scheduler driven by render demand, not packet-arrival mixer output.
use super::{
    GraphState,
    render::{RenderGraph, RenderGraphConfig, RenderMetrics, SourceRegistration},
};
use crate::backend::{AudioDecoder, DecodeRequest};
use crate::clock::SampleRateEstimator;
use crate::{AudioError, AudioResult, PacketDuration, SourceKey, StreamEpoch};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Shared encoded startup/FEC policy; no separate fixed mix jitter is introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JitterBufferConfig {
    /// Encoded startup target in milliseconds, 20..=200.
    pub target_ms: u16,
    /// Attempt previous-slot recovery from the immediately following packet.
    pub in_band_fec: bool,
}
impl Default for JitterBufferConfig {
    fn default() -> Self {
        Self {
            target_ms: 60,
            in_band_fec: true,
        }
    }
}
impl JitterBufferConfig {
    /// Validates the common startup target.
    pub fn validate(self) -> AudioResult<Self> {
        if !(20..=200).contains(&self.target_ms) {
            return Err(AudioError::InvalidConfig(
                "jitter target_ms must be 20..=200".into(),
            ));
        }
        Ok(self)
    }
}

/// Receive capacities and retirement policy, shared by production and simulations.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReceiveGraphConfig {
    /// Shared render DSP and source admission policy.
    pub render: RenderGraphConfig,
    /// The only encoded-packet startup window.
    pub jitter: JitterBufferConfig,
    /// Maximum admitted payload bytes, 1..=4000.
    pub max_payload_bytes: usize,
    /// Maximum sequence distance/queued packets, 1..=64, below half the wrapping sequence space.
    pub max_sequence_distance: u16,
    /// Retire source/decoder state after no accepted packets for this many milliseconds.
    pub source_expiry_ms: u16,
    /// Host-time clock inference window in milliseconds, 1000..=60000; arrival time is estimated.
    pub clock_window_ms: u16,
    /// Maximum source-correction motion in ppm per second, positive and <=1000.
    pub clock_slew_ppm_per_second: f64,
}
impl Default for ReceiveGraphConfig {
    fn default() -> Self {
        Self {
            render: RenderGraphConfig::default(),
            jitter: JitterBufferConfig::default(),
            max_payload_bytes: 4000,
            max_sequence_distance: 32,
            source_expiry_ms: 2000,
            clock_window_ms: 10_000,
            clock_slew_ppm_per_second: 100.0,
        }
    }
}
impl ReceiveGraphConfig {
    /// Validates all capacities and real-time work limits before graph construction.
    pub fn validate(self) -> AudioResult<Self> {
        self.render.validate()?;
        self.jitter.validate()?;
        if !(1..=4000).contains(&self.max_payload_bytes)
            || !(1..=64).contains(&self.max_sequence_distance)
            || !(200..=10_000).contains(&self.source_expiry_ms)
            || !(1000..=60_000).contains(&self.clock_window_ms)
            || !self.clock_slew_ppm_per_second.is_finite()
            || !(0.1..=1000.0).contains(&self.clock_slew_ppm_per_second)
        {
            return Err(AudioError::InvalidConfig(
                "invalid receive payload/sequence/expiry budget".into(),
            ));
        }
        Ok(self)
    }
}

/// A host-clock arrival record, independent of ZephyrVox wire packet types.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// Registered source/stream identity.
    pub source: SourceKey,
    /// Decoder/DSP generation. Stale epochs never enter current prediction history.
    pub epoch: StreamEpoch,
    /// Per-source wrapping 16-bit sequence; not a global transport sequence.
    pub sequence: u16,
    /// Fixed session duration. The decoder independently checks actual codec packet duration.
    pub duration: PacketDuration,
    /// Host-monotonic nanoseconds within this graph's explicitly assigned arrival clock.
    pub arrival_ns: u64,
    /// Complete encoded payload, bounded before admission. No raw audio timestamps are assumed.
    pub payload: Vec<u8>,
}

/// Bounded ingress decision; normal arrivals do not advance decoder or render state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PacketOutcome {
    /// Packet queued without advancing playback.
    Accepted,
    /// An already-buffered sequence was not inserted twice.
    Duplicate,
    /// Packet lies behind the current sequence cursor.
    Late,
    /// A forward jump cleared bounded queues and all source DSP history.
    Resynchronized,
    /// Packet generation is no longer current.
    StaleEpoch,
}

/// Cumulative receive outcomes; not a truncated per-block event list.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ReceiveStats {
    /// Accepted packets, including bounded resynchronization arrivals.
    pub accepted: u64,
    /// Duplicate arrivals rejected before decoding.
    pub duplicates: u64,
    /// Late arrivals rejected before decoding.
    pub late: u64,
    /// Stale generation arrivals rejected before decoding.
    pub stale_epochs: u64,
    /// Explicit forward sequence resynchronizations.
    pub resynchronizations: u64,
    /// Normal packet decoder advancements.
    pub decoded_packets: u64,
    /// FEC attempts. Success does not prove that encoded redundancy existed.
    pub fec_attempts: u64,
    /// PLC decoder advancements for known missing sequence slots.
    pub concealed_packets: u64,
    /// Invalid/backend-failed decodes replaced by an explicit zero slot and state reset.
    pub decode_errors: u64,
    /// Sources retired after their accepted-arrival timeout.
    pub expired_sources: u64,
    /// Render calls that intentionally started/restarted behind the prior host clock.
    pub clock_recoveries: u64,
    /// Decoder CPU time, including failed/FEC/PLC calls, excluding I/O and DSP.
    pub decoding_execution_ns: u64,
}

struct Track {
    registration: SourceRegistration,
    decoder: Box<dyn AudioDecoder>,
    duration: PacketDuration,
    pcm: Vec<f32>,
    next: Option<u16>,
    packets: BTreeMap<u16, EncodedPacket>,
    started: bool,
    first_arrival_ns: Option<u64>,
    last_arrival_ns: Option<u64>,
    rate: SampleRateEstimator,
    media_position: u64,
    last_clock_sequence: Option<u16>,
    applied_clock_ppm: f64,
    freeze_clock_until_ns: u64,
}

/// Inferred source/device clocks; unavailable values are never serialized as zero.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SourceClockMetrics {
    /// Source/stream owning this correction state.
    pub source: SourceKey,
    /// Sample rate inferred from sequence-derived media position and host arrivals, in ppm.
    pub inferred_source_drift_ppm: Option<f64>,
    /// Device rate inferred from render demand and host time, in ppm; backend presentation mapping may refine it later.
    pub inferred_device_drift_ppm: Option<f64>,
    /// Applied linked-channel pre-limiter ratio correction.
    pub applied_correction_ppm: i32,
    /// Whether inferred clock difference exceeded the configured cap.
    pub saturated: bool,
}

/// Device-demand receiver owning one decoder per source/stream/epoch and one render cursor.
/// Protocol registration and transport remain host responsibilities; no async runtime is needed.
pub struct ReceiveGraph {
    config: ReceiveGraphConfig,
    render: RenderGraph,
    tracks: BTreeMap<SourceKey, Track>,
    stats: ReceiveStats,
    state: GraphState,
    last_render_ns: Option<u64>,
    device_rate: SampleRateEstimator,
}
impl ReceiveGraph {
    /// Creates an empty graph with bounded admission and exactly one encoded startup policy.
    pub fn new(config: ReceiveGraphConfig) -> AudioResult<Self> {
        let config = config.validate()?;
        Ok(Self {
            config,
            render: RenderGraph::new(config.render)?,
            tracks: BTreeMap::new(),
            stats: ReceiveStats::default(),
            state: GraphState::Running,
            last_render_ns: None,
            device_rate: SampleRateEstimator::new(
                config.render.format.sample_rate_hz(),
                config.clock_window_ms,
            )?,
        })
    }
    /// Returns validated effective graph policy; backend controls remain with backend instances.
    pub fn config(&self) -> ReceiveGraphConfig {
        self.config
    }
    /// Returns lifecycle state; abort and drain are explicit and idempotent.
    pub fn state(&self) -> GraphState {
        self.state
    }
    /// Returns complete cumulative ingress/decoder/retirement counters.
    pub fn statistics(&self) -> ReceiveStats {
        self.stats
    }
    /// Returns registered source count, including sources still waiting for their first packet.
    pub fn source_count(&self) -> usize {
        self.tracks.len()
    }
    /// Registers an exclusive decoder; replacing an existing key requires a different epoch.
    /// A failed registration leaves existing source/decoder state intact.
    pub fn register(
        &mut self,
        registration: SourceRegistration,
        decoder: Box<dyn AudioDecoder>,
    ) -> AudioResult<()> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        if decoder.format() != registration.format {
            return Err(AudioError::InvalidConfig(
                "decoder/registration format mismatch".into(),
            ));
        }
        if self
            .tracks
            .get(&registration.key)
            .is_some_and(|t| t.registration.epoch == registration.epoch)
        {
            return Err(AudioError::InvalidConfig(
                "replace must use a new source epoch".into(),
            ));
        }
        let duration = decoder.packet_duration();
        if self.config.render.max_source_queue_ms < duration.milliseconds() + 30 {
            return Err(AudioError::InvalidConfig(
                "source FIFO needs ptime plus 30 ms filter/rate headroom".into(),
            ));
        }
        let pcm = vec![
            0.0;
            duration
                .frames(registration.format)?
                .interleaved_samples(registration.format)?
        ];
        self.render.replace_source(registration)?;
        self.tracks.insert(
            registration.key,
            Track {
                registration,
                decoder,
                duration,
                pcm,
                next: None,
                packets: BTreeMap::new(),
                started: false,
                first_arrival_ns: None,
                last_arrival_ns: None,
                rate: SampleRateEstimator::new(
                    registration.format.sample_rate_hz(),
                    self.config.clock_window_ms,
                )?,
                media_position: 0,
                last_clock_sequence: None,
                applied_clock_ppm: 0.0,
                freeze_clock_until_ns: 0,
            },
        );
        Ok(())
    }
    /// Submits a packet without decoding or rendering. Sequence work and allocations are bounded.
    pub fn push_packet(&mut self, packet: EncodedPacket) -> AudioResult<PacketOutcome> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        let track = self
            .tracks
            .get_mut(&packet.source)
            .ok_or_else(|| AudioError::InvalidConfig("unregistered packet source".into()))?;
        if packet.epoch != track.registration.epoch {
            self.stats.stale_epochs += 1;
            return Ok(PacketOutcome::StaleEpoch);
        }
        if packet.duration != track.duration
            || packet.payload.is_empty()
            || packet.payload.len() > self.config.max_payload_bytes
        {
            return Err(AudioError::MalformedPacket(
                "invalid duration/empty/oversized packet".into(),
            ));
        }
        if track
            .last_arrival_ns
            .is_some_and(|last| packet.arrival_ns < last)
        {
            return Err(AudioError::InvalidFrame(
                "arrival clock regressed; host must supply a new epoch".into(),
            ));
        }
        let next = *track.next.get_or_insert(packet.sequence);
        let backwards = next.wrapping_sub(packet.sequence);
        if !track.started && backwards > 0 && backwards <= self.config.max_sequence_distance {
            // Only a bounded startup reorder may move the left edge backwards.
            let farthest = track
                .packets
                .keys()
                .map(|seq| seq.wrapping_sub(packet.sequence))
                .max()
                .unwrap_or(0);
            if farthest <= self.config.max_sequence_distance {
                track.next = Some(packet.sequence);
            }
        }
        let distance = packet
            .sequence
            .wrapping_sub(track.next.expect("next sequence initialized"));
        if distance >= 0x8000 {
            self.stats.late += 1;
            return Ok(PacketOutcome::Late);
        }
        let mut outcome = PacketOutcome::Accepted;
        if distance > self.config.max_sequence_distance {
            track.decoder.reset()?;
            self.render.reset_source(track.registration)?;
            track.packets.clear();
            track.next = Some(packet.sequence);
            track.started = false;
            track.first_arrival_ns = Some(packet.arrival_ns);
            self.stats.resynchronizations += 1;
            outcome = PacketOutcome::Resynchronized;
            track.rate.reset();
            track.last_clock_sequence = None;
            track.media_position = 0;
            track.applied_clock_ppm = 0.0;
            track.freeze_clock_until_ns = packet.arrival_ns.saturating_add(500_000_000);
        }
        if track.packets.contains_key(&packet.sequence) {
            self.stats.duplicates += 1;
            return Ok(PacketOutcome::Duplicate);
        }
        if track.packets.len() > usize::from(self.config.max_sequence_distance) {
            return Err(AudioError::ResourceExhausted(
                "encoded source queue capacity".into(),
            ));
        }
        track.first_arrival_ns.get_or_insert(packet.arrival_ns);
        track.last_arrival_ns = Some(packet.arrival_ns);
        match track.last_clock_sequence {
            None => {
                track.last_clock_sequence = Some(packet.sequence);
                track.rate.observe(0, packet.arrival_ns);
            }
            Some(previous) => {
                let distance = packet.sequence.wrapping_sub(previous);
                if distance > 0 && distance < 0x8000 {
                    track.media_position = track.media_position.saturating_add(
                        u64::from(distance)
                            * track.duration.frames(track.registration.format)?.get(),
                    );
                    track.rate.observe(track.media_position, packet.arrival_ns);
                    track.last_clock_sequence = Some(packet.sequence);
                }
            }
        }
        track.packets.insert(packet.sequence, packet);
        self.stats.accepted += 1;
        Ok(outcome)
    }
    /// Applies a source gain at a worker block boundary, preserving the shared 50 ms fade.
    pub fn set_gain(&mut self, key: SourceKey, gain: f32) -> AudioResult<()> {
        self.render.set_gain(key, gain)
    }
    /// Explicitly removes a source's decoder, encoded queue, resampler and limiter histories.
    pub fn remove_source(&mut self, key: SourceKey) {
        self.tracks.remove(&key);
        self.render.remove_source(key);
    }
    /// Services bounded device-clock demand. Caller supplies host-monotonic now, not remote time.
    pub fn render_into(&mut self, output: &mut [f32], now_ns: u64) -> AudioResult<RenderMetrics> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        self.service(output, now_ns, false)
    }
    fn service(
        &mut self,
        output: &mut [f32],
        now_ns: u64,
        draining: bool,
    ) -> AudioResult<RenderMetrics> {
        let demand = self.config.render.format.frames_in(output.len())?.get();
        if demand == 0
            || demand * 1000
                > u64::from(self.config.render.format.sample_rate_hz())
                    * u64::from(self.config.render.max_render_ms)
        {
            return Err(AudioError::InvalidFrame(
                "invalid receive render demand".into(),
            ));
        }
        if self.last_render_ns.is_some_and(|last| now_ns < last) {
            return Err(AudioError::InvalidFrame(
                "render host clock regressed; explicit recovery required".into(),
            ));
        }
        let elapsed_seconds = self
            .last_render_ns
            .map(|last| now_ns.saturating_sub(last) as f64 / 1_000_000_000.0)
            .unwrap_or(0.0)
            .min(0.1);
        self.last_render_ns = Some(now_ns);
        self.device_rate
            .observe(self.render.sample_position(), now_ns);
        if !draining {
            let expired = self
                .tracks
                .iter()
                .filter_map(|(key, track)| {
                    track
                        .last_arrival_ns
                        .filter(|last| {
                            now_ns.saturating_sub(*last)
                                > u64::from(self.config.source_expiry_ms) * 1_000_000
                        })
                        .map(|_| *key)
                })
                .collect::<Vec<_>>();
            for key in expired {
                self.remove_source(key);
                self.stats.expired_sources += 1;
            }
        }
        for (key, track) in &mut self.tracks {
            let correction = if !draining && now_ns >= track.freeze_clock_until_ns {
                match (self.device_rate.estimate(), track.rate.estimate()) {
                    (Some(device), Some(source)) => {
                        ((1.0 + device.drift_ppm / 1_000_000.0)
                            / (1.0 + source.drift_ppm / 1_000_000.0)
                            - 1.0)
                            * 1_000_000.0
                    }
                    _ => 0.0,
                }
            } else {
                0.0
            };
            let cap = f64::from(self.config.render.max_source_clock_correction_ppm);
            let step = self.config.clock_slew_ppm_per_second * elapsed_seconds;
            track.applied_clock_ppm +=
                (correction.clamp(-cap, cap) - track.applied_clock_ppm).clamp(-step, step);
            self.render
                .set_clock_correction(*key, track.applied_clock_ppm.round() as i32)?;
            if !track.started {
                let waited = track.first_arrival_ns.is_some_and(|first| {
                    now_ns.saturating_sub(first)
                        >= u64::from(self.config.jitter.target_ms) * 1_000_000
                });
                let buffered = track.packets.len()
                    > usize::from(
                        self.config
                            .jitter
                            .target_ms
                            .div_ceil(track.duration.milliseconds()),
                    );
                track.started = waited || buffered || (draining && !track.packets.is_empty());
            }
            if !track.started {
                continue;
            }
            // One demand is <=60 ms; 10 ms packets plus bounded filter warmup require at
            // most eight decoder advancements. A large sequence hole never causes a loop.
            for _ in 0..8 {
                if self.render.queued_frames(*key).unwrap_or(0) as u64 >= demand {
                    break;
                }
                if draining && track.packets.is_empty() {
                    break;
                }
                let Some(sequence) = track.next else {
                    break;
                };
                let packet = track.packets.remove(&sequence);
                if packet.is_none() {
                    track.freeze_clock_until_ns = now_ns.saturating_add(500_000_000);
                }
                let following = if self.config.jitter.in_band_fec {
                    track.packets.get(&sequence.wrapping_add(1))
                } else {
                    None
                };
                let request = if let Some(packet) = &packet {
                    self.stats.decoded_packets += 1;
                    DecodeRequest::Packet(&packet.payload)
                } else if let Some(following) = following {
                    self.stats.fec_attempts += 1;
                    DecodeRequest::Fec(&following.payload)
                } else {
                    self.stats.concealed_packets += 1;
                    DecodeRequest::Loss
                };
                let expected_frames = track.duration.frames(track.registration.format)?;
                let started = std::time::Instant::now();
                let decoded = track.decoder.decode_into(request, &mut track.pcm);
                self.stats.decoding_execution_ns = self
                    .stats
                    .decoding_execution_ns
                    .saturating_add(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
                if !matches!(decoded, Ok(frames) if frames == expected_frames)
                    || !track
                        .pcm
                        .iter()
                        .all(|sample| sample.is_finite() && sample.abs() <= 64.0)
                {
                    self.stats.decode_errors += 1;
                    track.decoder.reset()?;
                    self.render.reset_source(track.registration)?;
                    track.freeze_clock_until_ns = now_ns.saturating_add(500_000_000);
                    track.pcm.fill(0.0);
                }
                self.render
                    .push_pcm(*key, track.registration.epoch, &track.pcm)?;
                track.next = Some(sequence.wrapping_add(1));
            }
        }
        self.render.render_demand(output, draining)
    }
    /// Returns clock inference quality and bounds for each source without inventing remote timestamps.
    pub fn clock_metrics(&self) -> Vec<SourceClockMetrics> {
        self.tracks
            .iter()
            .map(|(key, track)| {
                let device = self.device_rate.estimate().map(|e| e.drift_ppm);
                let source = track.rate.estimate().map(|e| e.drift_ppm);
                let requested = device
                    .zip(source)
                    .map(|(d, s)| ((1.0 + d / 1e6) / (1.0 + s / 1e6) - 1.0) * 1e6);
                SourceClockMetrics {
                    source: *key,
                    inferred_source_drift_ppm: source,
                    inferred_device_drift_ppm: device,
                    applied_correction_ppm: track.applied_clock_ppm.round() as i32,
                    saturated: requested.is_some_and(|v| {
                        v.abs() > f64::from(self.config.render.max_source_clock_correction_ppm)
                    }),
                }
            })
            .collect()
    }
    /// Clears stale playout buffers after a long host pause/clock reset without reusing histories.
    /// Registered source epochs remain visible; host may replace them on reconnect.
    pub fn recover_clock(&mut self) -> AudioResult<()> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        for track in self.tracks.values_mut() {
            track.decoder.reset()?;
            self.render.reset_source(track.registration)?;
            track.packets.clear();
            track.next = None;
            track.started = false;
            track.first_arrival_ns = None;
            track.last_arrival_ns = None;
            track.rate.reset();
            track.last_clock_sequence = None;
            track.media_position = 0;
            track.applied_clock_ppm = 0.0;
            track.freeze_clock_until_ns = 0;
        }
        self.render.reset_output_history()?;
        self.last_render_ns = None;
        self.device_rate.reset();
        self.stats.clock_recoveries += 1;
        Ok(())
    }
    /// Stops packet admission and drains buffered real packets, finite sequence holes and DSP tails.
    pub fn begin_drain(&mut self) {
        if self.state == GraphState::Running {
            self.state = GraphState::Draining;
        }
    }
    /// Returns a bounded drain prefix in per-channel frames; zero means completed.
    pub fn drain_into(&mut self, output: &mut [f32], now_ns: u64) -> AudioResult<u64> {
        if self.state == GraphState::Stopped {
            return Ok(0);
        }
        self.begin_drain();
        if self.tracks.values().any(|t| !t.packets.is_empty()) {
            return self.service(output, now_ns, true).map(|m| m.frames);
        }
        let frames = self.render.drain_into(output)?;
        if frames == 0 || self.render.state() == GraphState::Stopped {
            self.state = GraphState::Stopped;
            self.tracks.clear();
        }
        Ok(frames)
    }
    /// Immediately discards encoded/decoded/backend state and makes stop idempotent.
    pub fn abort(&mut self) {
        self.tracks.clear();
        self.render.abort();
        self.state = GraphState::Stopped;
    }
}
