//! Fixed-size frame handoff. Endpoints are exclusive, preallocated and never cloned.
use audiokit::spsc::{self, Consumer, Producer};
use audiokit::{AudioError, AudioFormat, AudioResult, SampleFrames};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};

/// Raw backend flags; silence is not a missing/discontinuous block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureFlags {
    /// Backend explicitly reported silent samples.
    pub silent: bool,
    /// Sample continuity was lost at this frame boundary.
    pub discontinuity: bool,
    /// Backend timestamp was invalid; its numeric value is not trusted.
    pub timestamp_error: bool,
}

/// One preallocated native sample frame. Unused channels remain zero.
#[derive(Debug, Clone, Copy)]
pub struct DeviceFrame {
    /// Native, post-conversion samples; only format.channels() are meaningful.
    pub samples: [f32; 32],
    /// Device sample cursor, not an encoded-packet sequence number.
    pub sample_position: u64,
    /// Raw backend clock nanoseconds. This is NOT a mapped host monotonic timestamp.
    pub device_timestamp_ns: Option<u64>,
    /// Host monotonic handoff time relative to this port's private origin, not presentation time.
    pub host_handoff_ns: Option<u64>,
    /// Native flags retained with the first frame of a block.
    pub flags: CaptureFlags,
}
impl Default for DeviceFrame {
    fn default() -> Self {
        Self {
            samples: [0.0; 32],
            sample_position: 0,
            device_timestamp_ns: None,
            host_handoff_ns: None,
            flags: CaptureFlags::default(),
        }
    }
}

/// Immutable counters without PCM or device identifiers.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct PortSnapshot {
    /// Total native callback invocations.
    pub callbacks: u64,
    /// Received/requested device frames, including silence and drops.
    pub device_frames: u64,
    /// Frames rejected because a bounded ring was full.
    pub dropped_frames: u64,
    /// Playback frames replaced by silence after startup.
    pub underrun_frames: u64,
    /// Explicit startup silence, separate from underrun.
    pub startup_frames: u64,
    /// Actual-playback reference frames lost to slow reference processing.
    pub reference_dropped_frames: u64,
    /// Maximum queued native frames.
    pub queue_high_water_frames: u64,
    /// Native xrun notifications, which do not automatically terminate the stream.
    pub xruns: u64,
    /// Nonzero error category indicates a terminal native backend failure.
    pub error_code: u8,
    /// Whether device/reference activity has been stopped.
    pub stopped: bool,
}

/// Atomic callback counters and stop/error signals; contains no locks or owned errors.
#[derive(Default)]
pub struct PortTelemetry {
    callbacks: AtomicU64,
    frames: AtomicU64,
    dropped: AtomicU64,
    underrun: AtomicU64,
    startup: AtomicU64,
    reference_dropped: AtomicU64,
    high_water: AtomicU64,
    xruns: AtomicU64,
    error: AtomicU8,
    stopped: AtomicBool,
}
impl PortTelemetry {
    /// Reads a best-effort atomic snapshot; fields may span adjacent callbacks.
    pub fn snapshot(&self) -> PortSnapshot {
        PortSnapshot {
            callbacks: self.callbacks.load(Ordering::Relaxed),
            device_frames: self.frames.load(Ordering::Relaxed),
            dropped_frames: self.dropped.load(Ordering::Relaxed),
            underrun_frames: self.underrun.load(Ordering::Relaxed),
            startup_frames: self.startup.load(Ordering::Relaxed),
            reference_dropped_frames: self.reference_dropped.load(Ordering::Relaxed),
            queue_high_water_frames: self.high_water.load(Ordering::Relaxed),
            xruns: self.xruns.load(Ordering::Relaxed),
            error_code: self.error.load(Ordering::Acquire),
            stopped: self.stopped.load(Ordering::Acquire),
        }
    }
    /// Signals stop without dropping heap ownership from a callback.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    /// Records an xrun notification without constructing an error string.
    pub fn record_xrun(&self) {
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }
    /// Publishes a stable numeric failure code; worker constructs the detailed host error.
    pub fn fail(&self, code: u8) {
        self.error.store(code.max(1), Ordering::Release);
    }
}

/// Worker-side capture/reference reader, preserving per-frame native metadata.
pub struct FrameReader {
    format: AudioFormat,
    consumer: Consumer<DeviceFrame>,
    telemetry: Arc<PortTelemetry>,
}
impl FrameReader {
    /// Returns the native format; channel mapping belongs to a worker graph.
    pub fn format(&self) -> AudioFormat {
        self.format
    }
    /// Pops one published native frame; no allocation or waiting.
    pub fn pop(&mut self) -> Option<DeviceFrame> {
        self.consumer.pop()
    }
    /// Returns a queue-depth snapshot in per-channel frames.
    pub fn queued_frames(&self) -> usize {
        self.consumer.len()
    }
    /// Returns shared PCM-free port telemetry.
    pub fn telemetry(&self) -> Arc<PortTelemetry> {
        Arc::clone(&self.telemetry)
    }
}

/// Native capture callback producer. A single backend callback owns it.
pub struct CaptureIngress {
    format: AudioFormat,
    producer: Producer<DeviceFrame>,
    telemetry: Arc<PortTelemetry>,
    cursor: u64,
    handoff_ns: Option<u64>,
}
impl CaptureIngress {
    /// Starts one callback's metadata accounting without allocating.
    pub fn begin_callback(&mut self) {
        self.begin_callback_at(None);
    }
    /// Supplies a host-monotonic handoff observation without pretending it is device capture time.
    pub fn begin_callback_at(&mut self, handoff_ns: Option<u64>) {
        self.handoff_ns = handoff_ns;
        self.telemetry.callbacks.fetch_add(1, Ordering::Relaxed);
    }
    /// Copies a complete native frame. Missing timestamps remain unavailable.
    /// On overflow the new frame is rejected; the sample cursor still advances.
    pub fn push(
        &mut self,
        samples: &[f32],
        timestamp_ns: Option<u64>,
        flags: CaptureFlags,
    ) -> bool {
        self.telemetry.frames.fetch_add(1, Ordering::Relaxed);
        if samples.len() != usize::from(self.format.channels())
            || !samples.iter().all(|s| s.is_finite())
        {
            self.telemetry.dropped.fetch_add(1, Ordering::Relaxed);
            self.cursor = self.cursor.wrapping_add(1);
            return false;
        }
        let mut frame = DeviceFrame {
            sample_position: self.cursor,
            device_timestamp_ns: timestamp_ns,
            host_handoff_ns: self.handoff_ns,
            flags,
            ..Default::default()
        };
        frame.samples[..samples.len()].copy_from_slice(samples);
        self.cursor = self.cursor.wrapping_add(1);
        let accepted = self.producer.push(frame);
        if !accepted {
            self.telemetry.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.telemetry
            .high_water
            .fetch_max(self.producer.len() as u64, Ordering::Relaxed);
        accepted
    }
    /// Applies an actual backend sample position (e.g. WASAPI device position).
    pub fn set_device_position(&mut self, position: u64) {
        self.cursor = position;
    }
    /// Returns the native format used for callback conversion.
    pub fn format(&self) -> AudioFormat {
        self.format
    }
    /// Returns a worker/error callback telemetry handle; clone outside sample callback.
    pub fn telemetry(&self) -> Arc<PortTelemetry> {
        Arc::clone(&self.telemetry)
    }
}

/// Creates fixed-capacity capture endpoints, rounding frame capacity up to a power of two.
pub fn capture_pair(
    format: AudioFormat,
    capacity_frames: usize,
) -> AudioResult<(CaptureIngress, FrameReader)> {
    if capacity_frames == 0 {
        return Err(AudioError::InvalidConfig(
            "capture capacity must be positive".into(),
        ));
    }
    let capacity = capacity_frames
        .checked_next_power_of_two()
        .ok_or_else(|| AudioError::ResourceExhausted("capture capacity overflow".into()))?;
    let (producer, consumer) = spsc::bounded(capacity, DeviceFrame::default())?;
    let telemetry = Arc::new(PortTelemetry::default());
    Ok((
        CaptureIngress {
            format,
            producer,
            telemetry: Arc::clone(&telemetry),
            cursor: 0,
            handoff_ns: None,
        },
        FrameReader {
            format,
            consumer,
            telemetry,
        },
    ))
}

/// Worker-side playback ingress. Writes are bounded; an unaccepted suffix is not silently lost.
pub struct FrameWriter {
    format: AudioFormat,
    producer: Producer<DeviceFrame>,
    telemetry: Arc<PortTelemetry>,
}
impl FrameWriter {
    /// Writes complete finite native PCM frames, accepting only a contiguous prefix.
    pub fn write(&mut self, pcm: &[f32]) -> AudioResult<SampleFrames> {
        if self.telemetry.snapshot().stopped {
            return Err(AudioError::Cancelled);
        }
        let frames = self.format.frames_in(pcm.len())?;
        if !pcm.iter().all(|s| s.is_finite()) {
            return Err(AudioError::InvalidFrame("non-finite playback PCM".into()));
        }
        let channels = usize::from(self.format.channels());
        let mut accepted = 0;
        for samples in pcm.chunks_exact(channels) {
            let mut frame = DeviceFrame::default();
            frame.samples[..channels].copy_from_slice(samples);
            if !self.producer.push(frame) {
                break;
            }
            accepted += 1;
        }
        self.telemetry
            .dropped
            .fetch_add(frames.get() - accepted, Ordering::Relaxed);
        self.telemetry
            .high_water
            .fetch_max(self.producer.len() as u64, Ordering::Relaxed);
        Ok(SampleFrames::new(accepted))
    }
    /// Returns queue depth in per-channel native frames.
    pub fn queued_frames(&self) -> usize {
        self.producer.len()
    }
    /// Returns shared PCM-free telemetry.
    pub fn telemetry(&self) -> Arc<PortTelemetry> {
        Arc::clone(&self.telemetry)
    }
    /// Returns the native playback format.
    pub fn format(&self) -> AudioFormat {
        self.format
    }
}

/// Callback-owned playback consumer. Reference is published after native conversion.
pub struct PlaybackEgress {
    format: AudioFormat,
    consumer: Consumer<DeviceFrame>,
    reference: Producer<DeviceFrame>,
    telemetry: Arc<PortTelemetry>,
    prebuffer_frames: usize,
    started: bool,
    cursor: u64,
}
impl PlaybackEgress {
    /// Accounts one callback and evaluates startup once, not once per output frame.
    pub fn begin_callback(&mut self) {
        self.telemetry.callbacks.fetch_add(1, Ordering::Relaxed);
        if !self.started && self.consumer.len() >= self.prebuffer_frames {
            self.started = true;
        }
    }
    /// Consumes one frame or produces explicit startup/underrun silence.
    pub fn next_frame(&mut self, timestamp_ns: Option<u64>) -> DeviceFrame {
        let mut frame = if self.started {
            self.consumer.pop().unwrap_or_else(|| {
                self.telemetry.underrun.fetch_add(1, Ordering::Relaxed);
                DeviceFrame {
                    flags: CaptureFlags {
                        silent: true,
                        ..Default::default()
                    },
                    ..Default::default()
                }
            })
        } else {
            self.telemetry.startup.fetch_add(1, Ordering::Relaxed);
            DeviceFrame {
                flags: CaptureFlags {
                    silent: true,
                    ..Default::default()
                },
                ..Default::default()
            }
        };
        frame.sample_position = self.cursor;
        frame.device_timestamp_ns = timestamp_ns;
        self.cursor = self.cursor.wrapping_add(1);
        self.telemetry.frames.store(self.cursor, Ordering::Relaxed);
        frame
    }
    /// Publishes exactly what reached the device buffer, including quantization and silence.
    /// Caller converts native samples back to f32 before this copy-only operation.
    pub fn publish_reference(&mut self, frame: DeviceFrame) {
        if !self.reference.push(frame) {
            self.telemetry
                .reference_dropped
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    /// Returns the native format used by the actual-device and virtual callback.
    pub fn format(&self) -> AudioFormat {
        self.format
    }
}

/// Creates playback plus actual-consumed-reference ports. No AEC work runs in callbacks.
pub fn playback_pair(
    format: AudioFormat,
    capacity_frames: usize,
    prebuffer_frames: usize,
) -> AudioResult<(FrameWriter, PlaybackEgress, FrameReader)> {
    if capacity_frames == 0 || prebuffer_frames > capacity_frames {
        return Err(AudioError::InvalidConfig(
            "invalid playback capacity/prebuffer".into(),
        ));
    }
    let capacity = capacity_frames
        .checked_next_power_of_two()
        .ok_or_else(|| AudioError::ResourceExhausted("playback capacity overflow".into()))?;
    let (producer, consumer) = spsc::bounded(capacity, DeviceFrame::default())?;
    let (reference, reference_reader) = spsc::bounded(capacity, DeviceFrame::default())?;
    let telemetry = Arc::new(PortTelemetry::default());
    Ok((
        FrameWriter {
            format,
            producer,
            telemetry: Arc::clone(&telemetry),
        },
        PlaybackEgress {
            format,
            consumer,
            reference,
            telemetry: Arc::clone(&telemetry),
            prebuffer_frames,
            started: false,
            cursor: 0,
        },
        FrameReader {
            format,
            consumer: reference_reader,
            telemetry,
        },
    ))
}
