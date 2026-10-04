//! Native material acquisition, separate from offline production-graph execution.
use crate::{Error, Result, RunConfig};
use audiokit::AudioFormat;
use serde::{Deserialize, Serialize};

/// Bounded microphone acquisition controls, separate from portable DSP settings.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaterialCaptureOptions {
    /// Requested native sample duration, 100..=60000 ms; default 10000 ms.
    pub duration_ms: u32,
    /// Native queue capacity, 20..=1000 ms before power-of-two rounding; default 200.
    pub queue_ms: u16,
    /// No valid frames for this long ends capture, 250..=10000 ms; default 2000.
    pub stall_timeout_ms: u16,
}
impl Default for MaterialCaptureOptions {
    fn default() -> Self {
        Self {
            duration_ms: 10_000,
            queue_ms: 200,
            stall_timeout_ms: 2000,
        }
    }
}
impl MaterialCaptureOptions {
    /// Checks bounds without device access, allocation or filesystem effects.
    pub fn validate(&self) -> Result<()> {
        if !(100..=60_000).contains(&self.duration_ms)
            || !(20..=1000).contains(&self.queue_ms)
            || !(250..=10_000).contains(&self.stall_timeout_ms)
        {
            return Err(Error::Invalid(
                "microphone capture limits out of range".into(),
            ));
        }
        Ok(())
    }
}

/// Device evidence for the original material, not a claim of realtime DSP or duplex E2E.
/// Device IDs/names and absolute paths are deliberately excluded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialCaptureReport {
    /// Native evidence schema version, currently 1.
    pub schema_version: u32,
    /// Actual negotiated mono/stereo native format.
    pub format: AudioFormat,
    /// Requested acquisition controls.
    pub options: MaterialCaptureOptions,
    /// duration_reached, finished_early, cancelled, stalled or device_error.
    pub stop_reason: String,
    /// Per-channel material frames, including explicit missing-frame zeros.
    pub material_frames: u64,
    /// Accepted native frames included in the material.
    pub captured_frames: u64,
    /// Missing cursor positions preserved as zeros, never compressed out of time.
    pub inserted_gap_frames: u64,
    /// Repeated/backward native cursors discarded; frame counter wrap is unsupported here.
    pub rejected_cursor_frames: u64,
    /// Discontinuity flag boundaries, not proof of a specific audible defect.
    pub discontinuity_boundaries: u64,
    /// Invalid native timestamp flag boundaries.
    pub timestamp_error_boundaries: u64,
    /// First accepted raw device timestamp; no host epoch mapping is implied.
    pub first_device_timestamp_ns: Option<u64>,
    /// Last accepted raw device timestamp; no presentation/capture latency inference.
    pub last_device_timestamp_ns: Option<u64>,
    /// Maximum callback-handoff to worker-read age, measured in the port's host origin.
    /// Not acoustic capture delay or processing latency.
    pub max_handoff_age_ns: Option<u64>,
    /// Callback count; the final atomic snapshot can span adjacent callbacks.
    pub callbacks: u64,
    /// Whole-port rejected frames, potentially including cropped EOF activity.
    pub port_dropped_frames: u64,
    /// Maximum native queue depth in frames.
    pub queue_high_water_frames: u64,
    /// Native xrun notifications.
    pub xruns: u64,
    /// Stable backend failure code; zero means no reported terminal error.
    pub error_code: u8,
    /// Queued frames intentionally excluded when capture stopped, not a playback gap.
    pub excluded_queued_frames: u64,
}
impl MaterialCaptureReport {
    pub(crate) fn validate(
        &self,
        config: &RunConfig,
        format: AudioFormat,
        frames: u64,
    ) -> Result<()> {
        self.options.validate()?;
        let maximum =
            u64::from(self.format.sample_rate_hz()) * u64::from(self.options.duration_ms) / 1000;
        if self.schema_version != 1
            || self.format != format
            || ![1, 2].contains(&self.format.channels())
            || self.material_frames != frames
            || self.material_frames > maximum
            || self.captured_frames.checked_add(self.inserted_gap_frames) != Some(frames)
            || self.discontinuity_boundaries > self.captured_frames
            || self.timestamp_error_boundaries > self.captured_frames
            || self.first_device_timestamp_ns.is_some() != self.last_device_timestamp_ns.is_some()
            || frames == 0
            || frames > config.pcm_sample_limit() as u64 / u64::from(format.channels())
            || ![
                "duration_reached",
                "finished_early",
                "cancelled",
                "stalled",
                "device_error",
            ]
            .contains(&self.stop_reason.as_str())
            || (self.stop_reason == "duration_reached" && frames != maximum)
        {
            return Err(Error::Invalid(
                "inconsistent microphone material evidence".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn healthy(&self) -> bool {
        matches!(
            self.stop_reason.as_str(),
            "duration_reached" | "finished_early"
        ) && self.inserted_gap_frames == 0
            && self.rejected_cursor_frames == 0
            && self.discontinuity_boundaries == 0
            && self.port_dropped_frames == 0
            && self.error_code == 0
            && self.xruns == 0
    }
}

#[cfg(feature = "native-cpal")]
#[path = "microphone.rs"]
mod native;
#[cfg(feature = "native-cpal")]
pub use native::record_microphone;
