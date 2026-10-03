//! Bounded external packet recordings, distinct from diagnostic snapshots or WAV.
use crate::{Error, Result, RunConfig};
use audiokit::{AudioFormat, PacketDuration, StreamKind};
use serde::{Deserialize, Serialize};

/// Resource envelope for a single-source, single-epoch receiver recording.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReceiveSimulationConfig {
    /// Maximum supplied packet records, including missing payloads, 1..=65536.
    pub max_packets: usize,
    /// Maximum fixed-10-ms render demands, 1..=60000.
    pub max_render_ticks: usize,
    /// Maximum relative host timestamp, 1..=600000 milliseconds.
    pub max_duration_ms: u32,
}
impl Default for ReceiveSimulationConfig {
    fn default() -> Self {
        Self {
            max_packets: 8192,
            max_render_ticks: 6000,
            max_duration_ms: 60_000,
        }
    }
}
impl ReceiveSimulationConfig {
    /// Rejects unbounded packet count, demand count or media-time work.
    pub fn validate(&self) -> Result<()> {
        if !(1..=65_536).contains(&self.max_packets)
            || !(1..=60_000).contains(&self.max_render_ticks)
            || !(1..=600_000).contains(&self.max_duration_ms)
        {
            return Err(Error::Invalid("invalid receive simulation budgets".into()));
        }
        Ok(())
    }
}

/// Negotiated source identity/format; exporters must anonymize host account IDs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacketSource {
    /// Per-recording opaque nonzero source ID, never an account name.
    pub source_id: u64,
    /// Nonzero logical stream ID.
    pub stream_id: u16,
    /// One fixed stream generation; switching epochs needs another recording.
    pub epoch: u64,
    /// Voice mono or desktop stereo.
    pub kind: StreamKind,
    /// Decoder output format, currently 48 kHz with the negotiated channel layout.
    pub format: AudioFormat,
    /// Fixed negotiated packet duration; per-packet values must match.
    pub ptime: PacketDuration,
}
impl PacketSource {
    pub(crate) fn registration(&self) -> Result<audiokit::graph::render::SourceRegistration> {
        use audiokit::{SourceId, SourceKey, StreamEpoch, StreamId};
        Ok(audiokit::graph::render::SourceRegistration {
            key: SourceKey {
                source: SourceId::new(self.source_id)?,
                stream: StreamId::new(self.stream_id)?,
            },
            epoch: StreamEpoch(self.epoch),
            format: self.format,
            kind: self.kind,
        })
    }
}

/// One recorded arrival; duplicate/reordered sequences are intentional observations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedPacket {
    /// Wrapping negotiated sequence; never renumbered or sorted by sequence.
    pub sequence: u16,
    /// Arrival in relative host nanoseconds, not remote capture or CPU time.
    pub arrival_ns: u64,
    /// Explicit packet duration; mid-session renegotiation is not yet supported.
    pub duration: PacketDuration,
    /// Optional source-media position; null means unknown, not frame zero.
    pub media_frame: Option<u64>,
    /// Original Opus bytes; null is unavailable recording material, not network loss.
    pub payload: Option<Vec<u8>>,
}

/// Versioned input material for production receive/render, not an event-only trace.
///
/// Payloads can reconstruct audio. Import never opens devices/network. The
/// recording starts from empty decoder/jitter state; a mid-stream excerpt cannot
/// reconstruct unknown preceding state. Completeness is a producer declaration,
/// not authenticated proof that a recording captured every real callback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacketTrace {
    /// Version 1; unknown fields/versions are rejected.
    pub schema_version: u32,
    /// Exactly one anonymous source/epoch in this first external replay slice.
    pub source: PacketSource,
    /// True only when initial codec/jitter state was empty at the recording origin.
    pub starts_at_stream_start: bool,
    /// Omitted arrival records; null means unknown, zero means declared complete.
    pub omitted_packets: Option<u64>,
    /// Omitted render demands; null means unknown, zero means declared complete.
    pub omitted_render_ticks: Option<u64>,
    /// Arrival-ordered records; equal-time records retain their supplied order.
    pub packets: Vec<RecordedPacket>,
    /// Strictly increasing relative host times, each consuming exactly 10 ms PCM.
    /// Irregular demand is retained, not repaired to a periodic schedule.
    pub render_ticks_ns: Vec<u64>,
}
impl PacketTrace {
    /// Validates coverage, formats, temporal order and hard/configured work bounds.
    /// Corrupt nonempty payloads are left to the actual decoder and reported there.
    pub fn validate(&self, config: &RunConfig) -> Result<()> {
        config.validate_parameters()?;
        if config.scenario != crate::Scenario::ReceiveSimulation {
            return Err(Error::Invalid(
                "packet input requires receive-simulation".into(),
            ));
        }
        let limits = config.receive_simulation;
        let last_tick = self.render_ticks_ns.last().copied().unwrap_or(0);
        let channels = if config.stream == StreamKind::Voice {
            1
        } else {
            2
        };
        if self.schema_version != 1
            || self.source.kind != config.stream
            || self.source.ptime != config.ptime
            || self.source.format.sample_rate_hz() != 48_000
            || self.source.format.channels() != channels
            || self.packets.len() > limits.max_packets
            || self.render_ticks_ns.is_empty()
            || self.render_ticks_ns.len() > limits.max_render_ticks
            || self.render_ticks_ns[0] == 0
            || last_tick > u64::from(limits.max_duration_ms) * 1_000_000
            || !self.render_ticks_ns.windows(2).all(|w| w[0] < w[1])
            || !self
                .packets
                .windows(2)
                .all(|w| w[0].arrival_ns <= w[1].arrival_ns)
        {
            return Err(Error::Invalid(
                "invalid packet source, order, schedule or budgets".into(),
            ));
        }
        self.source.registration()?;
        let frames = u64::from(self.source.format.sample_rate_hz())
            * u64::from(config.ptime.milliseconds())
            / 1000;
        if self.packets.iter().any(|packet| {
            packet.duration != self.source.ptime
                || packet.arrival_ns > last_tick
                || packet
                    .media_frame
                    .is_some_and(|first| first.checked_add(frames).is_none())
                || packet.payload.as_ref().is_some_and(|payload| {
                    payload.is_empty()
                        || payload.len()
                            > config
                                .max_payload_bytes
                                .min(config.receive.max_payload_bytes)
                })
        }) {
            return Err(Error::Invalid(
                "invalid packet duration, range or payload size".into(),
            ));
        }
        let samples = self.render_ticks_ns.len() as u64
            * u64::from(config.receive.render.format.sample_rate_hz() / 100)
            * u64::from(config.receive.render.format.channels());
        if samples > config.max_pcm_samples as u64 {
            return Err(Error::Invalid(
                "recorded render demand exceeds output sample budget".into(),
            ));
        }
        Ok(())
    }
    /// True only for declared cold-start recordings with no material omissions.
    pub fn is_complete(&self) -> bool {
        self.starts_at_stream_start
            && self.omitted_packets == Some(0)
            && self.omitted_render_ticks == Some(0)
            && self.packets.iter().all(|packet| packet.payload.is_some())
    }
    pub(crate) fn summary(&self) -> serde_json::Value {
        serde_json::json!({"source":self.source,"packets":self.packets.len(),
            "render_ticks":self.render_ticks_ns.len(),"starts_at_stream_start":self.starts_at_stream_start,
            "omitted_packets":self.omitted_packets,"omitted_render_ticks":self.omitted_render_ticks,
            "missing_payloads":self.packets.iter().filter(|p| p.payload.is_none()).count(),
            "complete_material":self.is_complete(),"last_host_time_ns":self.render_ticks_ns.last()})
    }
}
pub(crate) fn parse(raw: &[u8], config: &RunConfig) -> Result<PacketTrace> {
    if raw.len() as u64 > crate::io::JSON_LIMIT.min(config.max_input_bytes) {
        return Err(Error::Invalid(
            "packet recording exceeds JSON/input byte budget".into(),
        ));
    }
    let trace: PacketTrace = serde_json::from_slice(raw)?;
    trace.validate(config)?;
    Ok(trace)
}
