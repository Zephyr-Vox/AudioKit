//! Deterministic host policy around production receive/render and the production SPSC ring.
use crate::{Error, Result};
use audiokit::AudioFormat;
use serde::{Deserialize, Serialize};

/// One half-open virtual-host pause interval. Duration zero disables this fault.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PauseConfig {
    /// Start relative to virtual host origin, 0..=600000 ms.
    pub start_ms: u32,
    /// Duration 0..=1000 ms; no real sleeping or CPU load injection.
    pub duration_ms: u16,
}
impl PauseConfig {
    #[cfg(any(feature = "codec-opus", test))]
    pub(crate) fn active(self, now: u64) -> bool {
        let start = u64::from(self.start_ms) * 1_000_000;
        now >= start && now < start + u64::from(self.duration_ms) * 1_000_000
    }
    fn validate(self) -> Result<()> {
        if self.start_ms > 600_000 || self.duration_ms > 1000 {
            return Err(Error::Invalid(
                "scheduler pause outside bounded envelope".into(),
            ));
        }
        Ok(())
    }
}
/// Optional virtual host: arrivals continue during worker/output pauses independently.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerConfig {
    /// Adds a bounded output queue and independently gated 10 ms worker/consumer ticks.
    pub enabled: bool,
    /// Skips DSP demand, not packet arrival, during this interval; no catch-up bursts.
    pub worker_pause: PauseConfig,
    /// Skips output consumption only; DSP continues and full rings reject whole blocks.
    pub output_pause: PauseConfig,
    /// Interleaved SPSC values, power of two in 1024..=65536 and at least one quantum.
    pub output_capacity_samples: usize,
    /// Explicit receive clock recovery and queue discard on worker resume.
    pub recover_on_worker_resume: bool,
    /// Discard queued stale output on consumer resume, without resetting DSP state.
    pub discard_on_output_resume: bool,
}
impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            worker_pause: Default::default(),
            output_pause: Default::default(),
            output_capacity_samples: 4096,
            recover_on_worker_resume: true,
            discard_on_output_resume: true,
        }
    }
}
impl SchedulerConfig {
    /// Validates bounded storage, whole-frame alignment and disabled-fault consistency.
    pub fn validate(self, format: AudioFormat) -> Result<()> {
        self.worker_pause.validate()?;
        self.output_pause.validate()?;
        if !(1024..=65536).contains(&self.output_capacity_samples)
            || !self.output_capacity_samples.is_power_of_two()
            || !self
                .output_capacity_samples
                .is_multiple_of(usize::from(format.channels()))
            || self.output_capacity_samples
                < format.sample_rate_hz() as usize / 100 * usize::from(format.channels())
            || !self.enabled
                && (self.worker_pause.duration_ms != 0 || self.output_pause.duration_ms != 0)
        {
            return Err(Error::Invalid(
                "invalid scheduler queue/fault configuration".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(feature = "codec-opus")]
#[derive(Default, Clone, Copy, Serialize)]
pub(crate) struct SchedulerStats {
    pub worker_paused_ticks: u64,
    pub output_paused_ticks: u64,
    pub worker_resume_events: u64,
    pub output_resume_events: u64,
    pub output_underrun_frames: u64,
    pub output_overflow_frames: u64,
    pub recovery_discarded_frames: u64,
    pub produced_frames: u64,
    pub enqueued_frames: u64,
    pub consumed_frames: u64,
    pub demanded_frames: u64,
    pub synthetic_eof_flushed_frames: u64,
    pub max_queue_frames: u64,
    pub max_queued_age_ns: u64,
    pub last_underrun_ns: Option<u64>,
    pub first_healthy_consumption_after_worker_resume_ns: Option<u64>,
}
#[cfg(feature = "codec-opus")]
pub(crate) struct OutputScheduler {
    config: SchedulerConfig,
    producer: audiokit::spsc::Producer<(f32, u64)>,
    consumer: audiokit::spsc::Consumer<(f32, u64)>,
    channels: usize,
    worker_was_paused: bool,
    output_was_paused: bool,
    pub stats: SchedulerStats,
}
#[cfg(feature = "codec-opus")]
impl OutputScheduler {
    pub fn new(config: SchedulerConfig, format: AudioFormat) -> Result<Self> {
        config.validate(format)?;
        let (producer, consumer) =
            audiokit::spsc::bounded(config.output_capacity_samples, (0.0, 0))?;
        Ok(Self {
            config,
            producer,
            consumer,
            channels: usize::from(format.channels()),
            worker_was_paused: false,
            output_was_paused: false,
            stats: Default::default(),
        })
    }
    /// Returns worker pause, output pause, and whether explicit receiver recovery is required.
    pub fn begin_tick(&mut self, now: u64) -> (bool, bool, bool) {
        let worker = self.config.worker_pause.active(now);
        let output = self.config.output_pause.active(now);
        let worker_resume = self.worker_was_paused && !worker;
        let output_resume = self.output_was_paused && !output;
        self.stats.worker_paused_ticks += u64::from(worker);
        self.stats.output_paused_ticks += u64::from(output);
        self.stats.worker_resume_events += u64::from(worker_resume);
        self.stats.output_resume_events += u64::from(output_resume);
        if worker_resume {
            self.stats.first_healthy_consumption_after_worker_resume_ns = None;
        }
        let recover = worker_resume && self.config.recover_on_worker_resume;
        if recover || output_resume && self.config.discard_on_output_resume {
            self.stats.recovery_discarded_frames += self.clear() as u64;
        }
        self.worker_was_paused = worker;
        self.output_was_paused = output;
        (worker, output, recover)
    }
    pub fn push(&mut self, samples: &[f32], now: u64) {
        let frames = (samples.len() / self.channels) as u64;
        self.stats.produced_frames += frames;
        // Admission is all-or-nothing: no partial interleaved frame or torn 10 ms block.
        if samples.len() > self.producer.capacity() - self.producer.len() {
            self.stats.output_overflow_frames += frames;
            return;
        }
        for sample in samples {
            assert!(self.producer.push((*sample, now)));
        }
        self.stats.enqueued_frames += frames;
        self.stats.max_queue_frames = self.stats.max_queue_frames.max(self.queued_frames());
    }
    pub fn consume(&mut self, output: &mut [f32], now: u64) {
        self.stats.demanded_frames += (output.len() / self.channels) as u64;
        let mut supplied = 0;
        for sample in output.iter_mut() {
            if let Some((value, written_ns)) = self.consumer.pop() {
                *sample = value;
                supplied += 1;
                self.stats.max_queued_age_ns = self
                    .stats
                    .max_queued_age_ns
                    .max(now.saturating_sub(written_ns));
            } else {
                *sample = 0.0;
            }
        }
        self.stats.consumed_frames += (supplied / self.channels) as u64;
        let missing = ((output.len() - supplied) / self.channels) as u64;
        self.stats.output_underrun_frames += missing;
        if missing > 0 {
            self.stats.last_underrun_ns = Some(now);
        } else if self.stats.worker_resume_events > 0
            && self
                .stats
                .first_healthy_consumption_after_worker_resume_ns
                .is_none()
        {
            self.stats.first_healthy_consumption_after_worker_resume_ns = Some(now);
        }
    }
    pub fn queued_frames(&self) -> u64 {
        (self.consumer.len() / self.channels) as u64
    }
    fn clear(&mut self) -> usize {
        let samples = self.consumer.len();
        while self.consumer.pop().is_some() {}
        samples / self.channels
    }
    /// Synthetic EOF flush is separate from ordinary consumption, preserving queue conservation.
    pub fn flush(&mut self) -> Vec<f32> {
        let mut samples = Vec::with_capacity(self.consumer.len());
        while let Some((sample, _)) = self.consumer.pop() {
            samples.push(sample);
        }
        self.stats.synthetic_eof_flushed_frames += (samples.len() / self.channels) as u64;
        samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pause_boundaries_and_disabled_faults_are_strict() {
        let pause = PauseConfig {
            start_ms: 100,
            duration_ms: 50,
        };
        assert!(!pause.active(99_999_999));
        assert!(pause.active(100_000_000));
        assert!(!pause.active(150_000_000));
        let c = SchedulerConfig {
            worker_pause: pause,
            ..Default::default()
        };
        assert!(
            c.validate(AudioFormat::new(48_000, audiokit::ChannelLayout::Stereo).unwrap())
                .is_err()
        );
    }
    #[cfg(feature = "codec-opus")]
    #[test]
    fn whole_block_overflow_and_zero_fill_preserve_accounting() {
        let mut s = OutputScheduler::new(
            SchedulerConfig {
                enabled: true,
                output_capacity_samples: 1024,
                ..Default::default()
            },
            AudioFormat::new(48_000, audiokit::ChannelLayout::Stereo).unwrap(),
        )
        .unwrap();
        s.push(&[0.25; 960], 10);
        s.push(&[0.5; 960], 20);
        let mut out = [0.0; 960];
        s.consume(&mut out, 30);
        assert_eq!(out, [0.25; 960]);
        s.consume(&mut out, 40);
        assert_eq!(out, [0.0; 960]);
        assert_eq!(s.stats.output_overflow_frames, 480);
        assert_eq!(s.stats.output_underrun_frames, 480);
        assert_eq!(s.stats.max_queued_age_ns, 20);
        assert_eq!(
            s.stats.produced_frames,
            s.stats.enqueued_frames + s.stats.output_overflow_frames
        );
    }
}
