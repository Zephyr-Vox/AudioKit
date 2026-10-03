//! Demand-driven sample-cursor mixing. There is no second packet jitter/startup wait here.
use super::GraphState;
use crate::activity::{ActivityConfig, ActivityDetector};
use crate::diagnostics::{AudioDiagnosticsConfig, AudioFrameMetrics, AudioSignalAnalyzer};
use crate::limiter::{LimiterConfig, LimiterFrameMetrics, MasterLimiter};
use crate::mix::{MAX_SOURCE_VOLUME, MixConfig, SourceGainSmoother};
use crate::resample::PcmClockDriftCorrector;
use crate::resample::ResamplerConfig;
use crate::resample_format::ResampleBlockFormat;
use crate::stream_resample::ContinuousResampler;
use crate::{
    AudioError, AudioFormat, AudioResult, ChannelLayout, SourceKey, StreamEpoch, StreamKind,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// Render policy shared by production hosts and future test scenarios.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderGraphConfig {
    /// Device/render PCM, independent of encoded source channel counts.
    pub format: AudioFormat,
    /// Explicit simultaneous source/stream admission cap, 1..=128.
    pub max_sources: usize,
    /// Per-source decoded FIFO cap in milliseconds, 40..=500, including rate staging.
    pub max_source_queue_ms: u16,
    /// Maximum demand serviced by one call, 1..=60 milliseconds.
    pub max_render_ms: u16,
    /// Accepted source gain fade, default 50 ms.
    pub gain: MixConfig,
    /// Energy/hysteresis/hangover activity, not packet-presence normalization.
    pub activity: ActivityConfig,
    /// Shared resampler profile used by each isolated source.
    pub resampler: ResamplerConfig,
    /// Source-stage protection, with independent lookahead accounted in metrics.
    pub source_limiter: LimiterConfig,
    /// Final master protection after bus summation; does not quantize PCM.
    pub master_limiter: LimiterConfig,
    /// Power-normalization coefficient smoothing time in milliseconds, 0.1..=100.
    pub normalization_smoothing_ms: f32,
    /// Absolute per-source rate correction cap in ppm, 0..=1000; zero disables correction.
    pub max_source_clock_correction_ppm: u16,
}
impl Default for RenderGraphConfig {
    fn default() -> Self {
        Self {
            format: AudioFormat::new(48_000, ChannelLayout::Stereo).expect("valid default"),
            max_sources: 32,
            max_source_queue_ms: 160,
            max_render_ms: 20,
            gain: MixConfig::default(),
            activity: ActivityConfig::default(),
            resampler: ResamplerConfig::default(),
            source_limiter: LimiterConfig {
                ceiling_dbfs: -3.0,
                ..Default::default()
            },
            master_limiter: LimiterConfig::default(),
            normalization_smoothing_ms: 5.0,
            max_source_clock_correction_ppm: 500,
        }
    }
}
impl RenderGraphConfig {
    /// Validates all nodes before allocation or source admission.
    pub fn validate(self) -> AudioResult<Self> {
        if !(1..=128).contains(&self.max_sources)
            || !(40..=500).contains(&self.max_source_queue_ms)
            || !(1..=60).contains(&self.max_render_ms)
            || !self.normalization_smoothing_ms.is_finite()
            || !(0.1..=100.0).contains(&self.normalization_smoothing_ms)
            || self.max_source_clock_correction_ppm > 1000
        {
            return Err(AudioError::InvalidConfig(
                "invalid render graph capacity/demand/smoothing".into(),
            ));
        }
        self.gain
            .validate()
            .map_err(|e| AudioError::InvalidConfig(e.to_string()))?;
        self.activity.validate()?;
        self.resampler
            .validate()
            .map_err(|e| AudioError::InvalidConfig(e.to_string()))?;
        self.source_limiter
            .validate()
            .map_err(|e| AudioError::InvalidConfig(e.to_string()))?;
        self.master_limiter
            .validate()
            .map_err(|e| AudioError::InvalidConfig(e.to_string()))?;
        Ok(self)
    }
}

/// Explicit source registration. Format/epoch changes require replacement, never implicit reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRegistration {
    /// Opaque host-owned source and stream identity.
    pub key: SourceKey,
    /// Restart/reconnect generation owning codec and DSP state.
    pub epoch: StreamEpoch,
    /// Decoder PCM; voice remains mono until render channel mapping.
    pub format: AudioFormat,
    /// Separates speech and desktop normalization buses; no automatic ducking.
    pub kind: StreamKind,
}

/// One source's decision and protection metrics, without PCM content.
#[derive(Debug, Clone, Serialize)]
pub struct SourceRenderMetrics {
    /// Opaque source key for per-run anonymization by the diagnostic exporter.
    pub key: SourceKey,
    /// State-owning epoch.
    pub epoch: StreamEpoch,
    /// Energy-activity decision after gain, distinct from receiving a packet.
    pub active: bool,
    /// Filtered RMS dBFS; None denotes exact zero power.
    pub envelope_dbfs: Option<f64>,
    /// Applied final sample's source gain.
    pub gain: f32,
    /// Frames missing from an already-started source, not startup/EOF silence.
    pub missing_frames: u64,
    /// Queued decoded frames remaining after this demand.
    pub queue_frames: u64,
    /// Source limiter protection and delay metrics.
    pub limiter: LimiterFrameMetrics,
    /// Applied pre-limiter linked-channel source correction, in ppm.
    pub clock_correction_ppm: i32,
    /// Fixed-ratio converter group delay in the render clock, not worker CPU duration.
    pub resampler_delay_frames: u64,
    /// PCM awaiting a complete rate-correction quantum, in render-clock frames.
    pub rate_staging_frames: u64,
    /// Cumulative zeros added only to complete the final rate-correction quantum at EOF.
    pub rate_eof_padding_frames: u64,
}

/// Optional, non-overlapping worker substage timers. Decoder/admission timers are
/// supplied by ReceiveGraph and are outside render's execution_ns.
/// Timer overhead is included, so these observations are not a CPU deadline guarantee.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RenderExecutionProfile {
    /// FIFO extraction, gain/activity, source protection and channel expansion, summed across sources.
    pub source_processing_ns: u64,
    /// Bus accumulation and final normalization/summation.
    pub mix_ns: u64,
    /// Master protection and copying its result.
    pub master_limiter_ns: u64,
    /// Pre/post-master signal analysis.
    pub signal_analysis_ns: u64,
    /// Receiver decoder calls, including FEC/PLC; None for render-only demand.
    pub decode_ns: Option<u64>,
    /// Receiver PCM admission/resampling/rate correction; None for render-only demand.
    pub pcm_admission_ns: Option<u64>,
}
fn elapsed_ns(start: std::time::Instant) -> u64 {
    start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

/// One render call's common cursor and stage measurements.
#[derive(Debug, Clone, Serialize)]
pub struct RenderMetrics {
    /// First rendered frame's device-clock cursor.
    pub sample_position: u64,
    /// Complete device-clock frames returned.
    pub frames: u64,
    /// Energy-active speech source count; silence is excluded.
    pub active_voice: usize,
    /// Energy-active desktop/media source count, independent of speech count.
    pub active_desktop: usize,
    /// Pre-master floating mix measurement, retaining overdrive.
    pub pre_master: AudioFrameMetrics,
    /// Post-master final floating measurement.
    pub post_master: AudioFrameMetrics,
    /// Master protection, including lookahead and clamp counts.
    pub master_limiter: LimiterFrameMetrics,
    /// Measured worker execution only; excludes device buffers, transport and artifact I/O.
    pub execution_ns: u64,
    /// None unless the worker explicitly enables detailed timing; no PCM/state changes.
    pub execution_profile: Option<RenderExecutionProfile>,
    /// Per-source gain/activity/queue/protection trace.
    pub sources: Vec<SourceRenderMetrics>,
}

struct Source {
    registration: SourceRegistration,
    converter: ContinuousResampler,
    fifo: VecDeque<f32>,
    activity: ActivityDetector,
    gain: SourceGainSmoother,
    target_gain: f32,
    limiter: MasterLimiter,
    started: bool,
    finished: bool,
    rate_fifo: VecDeque<f32>,
    corrector: PcmClockDriftCorrector,
    quantum_samples: usize,
    clock_ppm: i32,
    rate_eof_padding_frames: u64,
}

/// One worker-owned render graph. Arrival submits PCM; only render demand advances the cursor.
pub struct RenderGraph {
    config: RenderGraphConfig,
    sources: BTreeMap<SourceKey, Source>,
    cursor: u64,
    voice_normalization: f32,
    desktop_normalization: f32,
    master: MasterLimiter,
    pre: AudioSignalAnalyzer,
    post: AudioSignalAnalyzer,
    state: GraphState,
    drain_remaining: u64,
    execution_profiling: bool,
}
fn processing(error: impl std::fmt::Display) -> AudioError {
    AudioError::Processing(error.to_string())
}
impl RenderGraph {
    /// Constructs the shared render owner with no sources or startup buffering.
    pub fn new(config: RenderGraphConfig) -> AudioResult<Self> {
        let config = config.validate()?;
        let master = MasterLimiter::new(
            config.format.sample_rate_hz(),
            config.format.channels(),
            config.master_limiter,
        )
        .map_err(processing)?;
        Ok(Self {
            config,
            sources: BTreeMap::new(),
            cursor: 0,
            voice_normalization: 1.0,
            desktop_normalization: 1.0,
            master,
            pre: AudioSignalAnalyzer::new(AudioDiagnosticsConfig::default()),
            post: AudioSignalAnalyzer::new(AudioDiagnosticsConfig::default()),
            state: GraphState::Running,
            drain_remaining: 0,
            execution_profiling: false,
        })
    }
    /// Returns the validated shared policy, including both limiter delays.
    pub fn config(&self) -> RenderGraphConfig {
        self.config
    }
    /// Enables worker-side substage timers. Disabled by default to avoid per-source clock reads.
    /// Changing this observation policy does not reset histories or alter PCM.
    pub fn set_execution_profiling(&mut self, enabled: bool) {
        self.execution_profiling = enabled;
    }
    /// Returns the cursor advanced exclusively by actual render demand.
    pub fn sample_position(&self) -> u64 {
        self.cursor
    }
    /// Returns graph lifecycle state.
    pub fn state(&self) -> GraphState {
        self.state
    }
    /// Returns currently admitted source/stream count.
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }
    pub(super) fn reset_output_history(&mut self) -> AudioResult<()> {
        self.master = MasterLimiter::new(
            self.config.format.sample_rate_hz(),
            self.config.format.channels(),
            self.config.master_limiter,
        )
        .map_err(processing)?;
        self.pre = AudioSignalAnalyzer::new(AudioDiagnosticsConfig::default());
        self.post = AudioSignalAnalyzer::new(AudioDiagnosticsConfig::default());
        self.voice_normalization = 1.0;
        self.desktop_normalization = 1.0;
        Ok(())
    }
    fn build_source(&self, registration: SourceRegistration) -> AudioResult<Source> {
        if (registration.kind == StreamKind::Voice && registration.format.channels() != 1)
            || (registration.kind == StreamKind::Desktop && registration.format.channels() != 2)
        {
            return Err(AudioError::InvalidConfig(
                "voice source must be mono; desktop stereo".into(),
            ));
        }
        let resampled = AudioFormat::new(
            self.config.format.sample_rate_hz(),
            registration.format.layout(),
        )?;
        let shape = ResampleBlockFormat::new(resampled.sample_rate_hz(), resampled.channels(), 10)?;
        Ok(Source {
            registration,
            converter: ContinuousResampler::new(
                registration.format,
                resampled,
                10,
                self.config.resampler,
            )?,
            fifo: VecDeque::new(),
            activity: ActivityDetector::new(self.config.activity)?,
            gain: SourceGainSmoother::default(),
            target_gain: 1.0,
            limiter: MasterLimiter::new(
                self.config.format.sample_rate_hz(),
                registration.format.channels(),
                self.config.source_limiter,
            )
            .map_err(processing)?,
            started: false,
            finished: false,
            rate_fifo: VecDeque::new(),
            corrector: PcmClockDriftCorrector::new(
                shape,
                self.config.max_source_clock_correction_ppm,
            )?,
            quantum_samples: shape.interleaved_samples(),
            clock_ppm: 0,
            rate_eof_padding_frames: 0,
        })
    }
    /// Registers an isolated source; an existing key requires an explicit epoch replacement.
    pub fn register(&mut self, registration: SourceRegistration) -> AudioResult<()> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        if self.sources.contains_key(&registration.key) {
            return Err(AudioError::InvalidConfig(
                "source already registered".into(),
            ));
        }
        if self.sources.len() >= self.config.max_sources {
            return Err(AudioError::ResourceExhausted(
                "render source admission cap".into(),
            ));
        }
        let source = self.build_source(registration)?;
        self.sources.insert(registration.key, source);
        Ok(())
    }
    /// Replaces DSP state for a new epoch while preserving the host's target gain/mute policy.
    pub fn replace_source(&mut self, registration: SourceRegistration) -> AudioResult<()> {
        if self
            .sources
            .get(&registration.key)
            .is_some_and(|s| s.registration.epoch == registration.epoch)
        {
            return Err(AudioError::InvalidConfig(
                "replacement must use a new epoch".into(),
            ));
        }
        self.reset_source(registration)
    }
    pub(super) fn reset_source(&mut self, registration: SourceRegistration) -> AudioResult<()> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        let Some(previous) = self.sources.get(&registration.key) else {
            return self.register(registration);
        };
        let target_gain = previous.target_gain;
        let mut source = self.build_source(registration)?;
        source.target_gain = target_gain;
        self.sources.insert(registration.key, source);
        Ok(())
    }
    /// Removes all state for an expired/aborted source; the scheduler owns retirement policy.
    pub fn remove_source(&mut self, key: SourceKey) {
        self.sources.remove(&key);
    }
    /// Applies a validated target at a worker block boundary. Gain retargeting remains continuous.
    pub fn set_gain(&mut self, key: SourceKey, gain: f32) -> AudioResult<()> {
        if !gain.is_finite() || !(0.0..=MAX_SOURCE_VOLUME).contains(&gain) {
            return Err(AudioError::InvalidConfig("invalid source gain".into()));
        }
        let source = self
            .sources
            .get_mut(&key)
            .ok_or_else(|| AudioError::InvalidConfig("unregistered gain source".into()))?;
        source.target_gain = gain;
        Ok(())
    }
    /// Returns a source's render-clock FIFO watermark, never an interleaved sample count.
    pub fn queued_frames(&self, key: SourceKey) -> Option<usize> {
        self.sources
            .get(&key)
            .map(|s| s.fifo.len() / usize::from(s.registration.format.channels()))
    }
    /// Applies a bounded source-clock ratio before source/master protection, never after it.
    /// The receiver owns inferred-clock quality and disturbance gating.
    pub fn set_clock_correction(&mut self, key: SourceKey, ppm: i32) -> AudioResult<()> {
        let source = self
            .sources
            .get_mut(&key)
            .ok_or_else(|| AudioError::InvalidConfig("unregistered clock source".into()))?;
        let cap = i32::from(self.config.max_source_clock_correction_ppm);
        if !(-cap..=cap).contains(&ppm) {
            return Err(AudioError::InvalidConfig(
                "source clock correction exceeds cap".into(),
            ));
        }
        source.clock_ppm = ppm;
        Ok(())
    }
    /// Submits at most 120 ms finite PCM to one registered source/epoch. No render occurs here.
    /// Magnitudes above 64 full-scale are rejected as invalid numeric input before DSP state changes.
    pub fn push_pcm(&mut self, key: SourceKey, epoch: StreamEpoch, pcm: &[f32]) -> AudioResult<()> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        let source = self
            .sources
            .get_mut(&key)
            .ok_or_else(|| AudioError::InvalidConfig("unregistered render source".into()))?;
        if source.registration.epoch != epoch || source.finished {
            return Err(AudioError::InvalidFrame(
                "stale/finished render epoch".into(),
            ));
        }
        let input_frames = source.registration.format.frames_in(pcm.len())?.get();
        if input_frames * 1000 > u64::from(source.registration.format.sample_rate_hz()) * 120
            || !pcm.iter().all(|v| v.is_finite() && v.abs() <= 64.0)
        {
            return Err(AudioError::InvalidFrame(
                "invalid/budget-exceeding source PCM".into(),
            ));
        }
        let predicted = (input_frames * u64::from(self.config.format.sample_rate_hz()))
            .div_ceil(u64::from(source.registration.format.sample_rate_hz()));
        let queued = source.fifo.len() as u64 / u64::from(source.registration.format.channels())
            + source.rate_fifo.len() as u64 / u64::from(source.registration.format.channels());
        let cap = u64::from(self.config.format.sample_rate_hz())
            * u64::from(self.config.max_source_queue_ms)
            / 1000;
        // Include one filter block of previously withheld samples in the preflight bound.
        let withheld = source
            .converter
            .pending_input_frames()
            .saturating_mul(u64::from(self.config.format.sample_rate_hz()))
            .div_ceil(u64::from(source.registration.format.sample_rate_hz()));
        if queued + predicted + withheld + u64::from(self.config.format.sample_rate_hz()) / 100 + 2
            > cap
        {
            return Err(AudioError::ResourceExhausted(
                "source PCM FIFO capacity".into(),
            ));
        }
        let converted = source.converter.push(pcm)?;
        source.rate_fifo.extend(converted);
        Self::release_rate_quanta(source)?;
        Ok(())
    }
    fn release_rate_quanta(source: &mut Source) -> AudioResult<()> {
        while source.rate_fifo.len() >= source.quantum_samples {
            let quantum = source
                .rate_fifo
                .drain(..source.quantum_samples)
                .collect::<Vec<_>>();
            let corrected = source.corrector.process_f32(&quantum, source.clock_ppm)?;
            source.fifo.extend(corrected);
        }
        Ok(())
    }
    /// Services a bounded exact device demand on the common sample cursor; missing source PCM is zero.
    /// Speech/media power normalization is separate, and correlated peaks remain limiter-protected.
    pub fn render_into(&mut self, output: &mut [f32]) -> AudioResult<RenderMetrics> {
        self.render_demand(output, false)
    }
    pub(super) fn render_demand(
        &mut self,
        output: &mut [f32],
        eof: bool,
    ) -> AudioResult<RenderMetrics> {
        let started = std::time::Instant::now();
        if self.state == GraphState::Stopped {
            return Err(AudioError::Cancelled);
        }
        let frames = self.config.format.frames_in(output.len())?.get();
        if frames == 0
            || frames * 1000
                > u64::from(self.config.format.sample_rate_hz())
                    * u64::from(self.config.max_render_ms)
        {
            return Err(AudioError::InvalidFrame(
                "render demand exceeds configured block budget or is empty".into(),
            ));
        }
        let end = self
            .cursor
            .checked_add(frames)
            .ok_or_else(|| AudioError::ResourceExhausted("render cursor overflow".into()))?;
        let mut voice = vec![0.0; output.len()];
        let mut desktop = vec![0.0; output.len()];
        let mut other = vec![0.0; output.len()];
        let mut active_voice = 0;
        let mut active_desktop = 0;
        let mut metrics = Vec::with_capacity(self.sources.len());
        let mut profile = self
            .execution_profiling
            .then(RenderExecutionProfile::default);
        for source in self.sources.values_mut() {
            let source_started = self.execution_profiling.then(std::time::Instant::now);
            let source_channels = source.registration.format.channels();
            let source_samples = frames as usize * usize::from(source_channels);
            let available = source.fifo.len().min(source_samples);
            let mut pcm = vec![0.0; source_samples];
            for sample in &mut pcm[..available] {
                *sample = source.fifo.pop_front().expect("available source PCM");
            }
            source
                .gain
                .apply_interleaved(
                    &mut pcm,
                    source_channels,
                    self.config.format.sample_rate_hz(),
                    source.target_gain,
                    self.config.gain,
                )
                .map_err(processing)?;
            let active = source.activity.observe(
                &pcm,
                source_channels,
                self.config.format.sample_rate_hz(),
            )? && (source.target_gain > 0.0 || source.gain.current_gain() > 0.0);
            let (limited, limiter) = source
                .limiter
                .process_interleaved(&pcm)
                .map_err(processing)?;
            // Mono speech stays mono through gain/activity/protection. Expand only at the bus.
            let source_format = AudioFormat::new(
                self.config.format.sample_rate_hz(),
                source.registration.format.layout(),
            )?;
            let limited =
                crate::channel::map_channels(source_format, self.config.format, &limited)?;
            if let (Some(p), Some(start)) = (&mut profile, source_started) {
                p.source_processing_ns = p.source_processing_ns.saturating_add(elapsed_ns(start));
            }
            let mix_started = self.execution_profiling.then(std::time::Instant::now);
            let bus = match source.registration.kind {
                StreamKind::Voice => {
                    active_voice += usize::from(active);
                    &mut voice
                }
                StreamKind::Desktop => {
                    active_desktop += usize::from(active);
                    &mut desktop
                }
                StreamKind::Other => &mut other,
            };
            for (mixed, sample) in bus.iter_mut().zip(limited) {
                *mixed += sample;
            }
            if let (Some(p), Some(start)) = (&mut profile, mix_started) {
                p.mix_ns = p.mix_ns.saturating_add(elapsed_ns(start));
            }
            let db = source.activity.envelope_dbfs();
            metrics.push(SourceRenderMetrics {
                key: source.registration.key,
                epoch: source.registration.epoch,
                active,
                envelope_dbfs: db.is_finite().then_some(db),
                gain: source.gain.current_gain(),
                missing_frames: if source.started && self.state == GraphState::Running && !eof {
                    (source_samples - available) as u64 / u64::from(source_channels)
                } else {
                    0
                },
                queue_frames: source.fifo.len() as u64 / u64::from(source_channels),
                limiter,
                clock_correction_ppm: source.clock_ppm,
                resampler_delay_frames: source.converter.accounting().delay_frames as u64,
                rate_staging_frames: source.rate_fifo.len() as u64 / u64::from(source_channels),
                rate_eof_padding_frames: source.rate_eof_padding_frames,
            });
            source.started |= available > 0;
        }
        let mix_started = self.execution_profiling.then(std::time::Instant::now);
        let alpha = 1.0
            - (-1000.0
                / self.config.format.sample_rate_hz() as f32
                / self.config.normalization_smoothing_ms)
                .exp();
        let voice_target = 1.0 / (active_voice.max(1) as f32).sqrt();
        let desktop_target = 1.0 / (active_desktop.max(1) as f32).sqrt();
        let channels = usize::from(self.config.format.channels());
        for start in (0..output.len()).step_by(channels) {
            self.voice_normalization += (voice_target - self.voice_normalization) * alpha;
            self.desktop_normalization += (desktop_target - self.desktop_normalization) * alpha;
            for index in start..start + channels {
                output[index] = voice[index] * self.voice_normalization
                    + desktop[index] * self.desktop_normalization
                    + other[index];
            }
        }
        if let (Some(p), Some(start)) = (&mut profile, mix_started) {
            p.mix_ns = p.mix_ns.saturating_add(elapsed_ns(start));
        }
        let analysis_started = self.execution_profiling.then(std::time::Instant::now);
        let pre_master = self
            .pre
            .observe_f32_frame(
                output,
                self.config.format.sample_rate_hz(),
                self.config.format.channels(),
            )
            .map_err(processing)?;
        if let (Some(p), Some(start)) = (&mut profile, analysis_started) {
            p.signal_analysis_ns = elapsed_ns(start);
        }
        let master_started = self.execution_profiling.then(std::time::Instant::now);
        let (limited, master_limiter) = self
            .master
            .process_interleaved(output)
            .map_err(processing)?;
        output.copy_from_slice(&limited);
        if let (Some(p), Some(start)) = (&mut profile, master_started) {
            p.master_limiter_ns = elapsed_ns(start);
        }
        let analysis_started = self.execution_profiling.then(std::time::Instant::now);
        let post_master = self
            .post
            .observe_f32_frame(
                output,
                self.config.format.sample_rate_hz(),
                self.config.format.channels(),
            )
            .map_err(processing)?;
        if let (Some(p), Some(start)) = (&mut profile, analysis_started) {
            p.signal_analysis_ns = p.signal_analysis_ns.saturating_add(elapsed_ns(start));
        }
        let result = RenderMetrics {
            sample_position: self.cursor,
            frames,
            active_voice,
            active_desktop,
            pre_master,
            post_master,
            master_limiter,
            execution_ns: started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            execution_profile: profile,
            sources: metrics,
        };
        self.cursor = end;
        Ok(result)
    }
    /// Begins an explicit finite drain: resampler tails plus source and master lookahead are retained.
    pub fn begin_drain(&mut self) -> AudioResult<()> {
        if self.state != GraphState::Running {
            return Ok(());
        }
        for source in self.sources.values_mut() {
            let tail = source.converter.finish()?;
            source.rate_fifo.extend(tail);
            if !source.rate_fifo.is_empty() {
                let remainder = source.rate_fifo.len() % source.quantum_samples;
                if remainder != 0 {
                    source.rate_eof_padding_frames += (source.quantum_samples - remainder) as u64
                        / u64::from(source.registration.format.channels());
                    source
                        .rate_fifo
                        .extend(std::iter::repeat_n(0.0, source.quantum_samples - remainder));
                }
            }
            Self::release_rate_quanta(source)?;
            source.finished = true;
        }
        let queued = self
            .sources
            .values()
            .map(|s| s.fifo.len() as u64 / u64::from(s.registration.format.channels()))
            .max()
            .unwrap_or(0);
        let source_delay = self
            .sources
            .values()
            .map(|s| s.limiter.lookahead_frames() as u64)
            .max()
            .unwrap_or(0);
        let master_delay = self.master.lookahead_frames() as u64;
        self.drain_remaining = if self.cursor == 0 && queued == 0 {
            0
        } else {
            queued
                + source_delay
                + master_delay
                + crate::diagnostics::TRUE_PEAK_GROUP_DELAY_FRAMES as u64
        };
        self.state = if self.drain_remaining == 0 {
            GraphState::Stopped
        } else {
            GraphState::Draining
        };
        Ok(())
    }
    /// Copies a bounded drain prefix, returning per-channel frames; zero means completed.
    pub fn drain_into(&mut self, output: &mut [f32]) -> AudioResult<u64> {
        self.begin_drain()?;
        let capacity = self.config.format.frames_in(output.len())?.get();
        let budget = u64::from(self.config.format.sample_rate_hz())
            * u64::from(self.config.max_render_ms)
            / 1000;
        let frames = capacity.min(self.drain_remaining).min(budget);
        if frames == 0 {
            return Ok(0);
        }
        let samples = usize::try_from(frames)
            .map_err(|_| AudioError::ResourceExhausted("drain overflow".into()))?
            * usize::from(self.config.format.channels());
        self.render_into(&mut output[..samples])?;
        self.drain_remaining -= frames;
        if self.drain_remaining == 0 {
            self.state = GraphState::Stopped;
        }
        Ok(frames)
    }
    /// Discards all source queues/DSP immediately and makes stop idempotent.
    pub fn abort(&mut self) {
        self.sources.clear();
        self.drain_remaining = 0;
        self.state = GraphState::Stopped;
    }
}
