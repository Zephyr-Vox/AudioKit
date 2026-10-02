//! Optional CPAL device discovery, error categories and exclusive native ports.
use crate::ports::{
    self, CaptureFlags, CaptureIngress, FrameReader, FrameWriter, PlaybackEgress, PortTelemetry,
};
use audiokit::backend::{CapturePort, CaptureRead, PlaybackPort};
use audiokit::{
    AudioError, AudioFormat, AudioResult, ChannelLayout, ClockDomain, ClockTimestamp, SampleFrames,
    TimestampQuality,
};
use cpal::traits::StreamTrait;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{FromSample, Sample, SampleFormat};
use std::{fmt, str::FromStr};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;

/// Direction of a native audio device operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceDirection {
    /// Microphone/capture direction.
    Input,
    /// Speaker/playback direction.
    Output,
}

impl fmt::Display for DeviceDirection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Input => "input",
            Self::Output => "output",
        })
    }
}

/// Errors returned by native device discovery and stream setup.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NativeAudioError {
    /// macOS or the audio backend denied device access.
    #[error("audio permission denied; grant {direction} access and retry")]
    PermissionDenied {
        /// Requested device direction.
        direction: DeviceDirection,
    },
    /// The requested device or host is not available.
    #[error("audio {direction} device is unavailable: {detail}")]
    DeviceUnavailable {
        /// Requested device direction.
        direction: DeviceDirection,
        /// Redacted backend detail.
        detail: String,
    },
    /// The requested sample format or channel layout is unsupported.
    #[error("audio configuration is unsupported: {0}")]
    UnsupportedConfig(String),
    /// The requested audio source is not available on this OS or OS build.
    #[error("audio source is unsupported on this platform: {0}")]
    UnsupportedPlatform(String),
    /// The platform audio backend failed in an unclassified way.
    #[error("audio backend failed: {0}")]
    Backend(String),
}

/// A stable, serializable description of one native device.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AudioDeviceInfo {
    /// Stable CPAL device identifier.
    pub id: String,
    /// Human-readable device name.
    pub name: String,
    /// Whether the device supports capture.
    pub input: bool,
    /// Whether the device supports playback.
    pub output: bool,
    /// Whether this is the current default input device.
    pub default_input: bool,
    /// Whether this is the current default output device.
    pub default_output: bool,
}

/// Lists devices from the default platform audio host.
pub fn list_devices() -> Result<Vec<AudioDeviceInfo>, NativeAudioError> {
    let host = cpal::default_host();
    let default_input = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let default_output = host
        .default_output_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let devices = host
        .devices()
        .map_err(|error| map_cpal_error(error, None))?;
    let mut result = Vec::new();
    for device in devices {
        let id = device
            .id()
            .map_err(|error| map_cpal_error(error, None))?
            .to_string();
        let description = device
            .description()
            .map_err(|error| map_cpal_error(error, None))?;
        result.push(AudioDeviceInfo {
            default_input: default_input.as_deref() == Some(id.as_str()),
            default_output: default_output.as_deref() == Some(id.as_str()),
            input: description.supports_input(),
            output: description.supports_output(),
            id,
            name: description.name().to_owned(),
        });
    }
    Ok(result)
}

/// Resolves a selected or default device for one direction.
pub fn resolve_device(
    direction: DeviceDirection,
    selected_id: Option<&str>,
) -> Result<cpal::Device, NativeAudioError> {
    let host = cpal::default_host();
    if let Some(selected_id) = selected_id {
        let requested = cpal::DeviceId::from_str(selected_id).map_err(|error| {
            NativeAudioError::DeviceUnavailable {
                direction,
                detail: error.to_string(),
            }
        })?;
        return host
            .device_by_id(&requested)
            .ok_or_else(|| NativeAudioError::DeviceUnavailable {
                direction,
                detail: format!("device id {selected_id:?} was not found"),
            });
    }
    match direction {
        DeviceDirection::Input => host.default_input_device(),
        DeviceDirection::Output => host.default_output_device(),
    }
    .ok_or_else(|| NativeAudioError::DeviceUnavailable {
        direction,
        detail: "no default device is configured".to_owned(),
    })
}

/// Maps a CPAL error into a stable host-facing category.
pub fn map_cpal_error(error: cpal::Error, direction: Option<DeviceDirection>) -> NativeAudioError {
    let direction = direction.unwrap_or(DeviceDirection::Output);
    match error.kind() {
        cpal::ErrorKind::PermissionDenied => NativeAudioError::PermissionDenied { direction },
        cpal::ErrorKind::UnsupportedConfig | cpal::ErrorKind::UnsupportedOperation => {
            NativeAudioError::UnsupportedConfig(error.to_string())
        }
        cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::HostUnavailable => {
            NativeAudioError::DeviceUnavailable {
                direction,
                detail: error.to_string(),
            }
        }
        _ => NativeAudioError::Backend(error.to_string()),
    }
}

fn choose_config(
    device: &cpal::Device,
    direction: DeviceDirection,
    preferred: AudioFormat,
) -> Result<cpal::SupportedStreamConfig, NativeAudioError> {
    let ranges: Vec<_> = match direction {
        DeviceDirection::Input => device
            .supported_input_configs()
            .map_err(|e| map_cpal_error(e, Some(direction)))?
            .collect(),
        DeviceDirection::Output => device
            .supported_output_configs()
            .map_err(|e| map_cpal_error(e, Some(direction)))?
            .collect(),
    };
    ranges
        .into_iter()
        .filter(|r| r.channels() > 0 && r.channels() <= 32)
        .filter_map(|r| {
            let precision = match r.sample_format() {
                SampleFormat::F32 => 0,
                SampleFormat::I16 => 1,
                SampleFormat::I32 => 2,
                SampleFormat::F64 => 3,
                SampleFormat::U16 => 4,
                SampleFormat::U8 => 5,
                SampleFormat::I8 => 6,
                _ => return None,
            };
            let rate = preferred
                .sample_rate_hz()
                .clamp(r.min_sample_rate(), r.max_sample_rate());
            let class = if r.channels() == u16::from(preferred.channels()) {
                0
            } else if r.channels() == 1 || preferred.channels() == 1 {
                1
            } else {
                2
            };
            let score = (
                precision,
                class,
                r.channels().abs_diff(u16::from(preferred.channels())),
                rate.abs_diff(preferred.sample_rate_hz()),
            );
            Some((score, r.try_with_sample_rate(rate)?))
        })
        .min_by_key(|(score, _)| *score)
        .map(|(_, c)| c)
        .ok_or_else(|| {
            NativeAudioError::UnsupportedConfig("no native PCM conversion configuration".into())
        })
}

fn native_format(config: &cpal::StreamConfig) -> Result<AudioFormat, NativeAudioError> {
    let layout = match config.channels {
        1 => ChannelLayout::Mono,
        2 => ChannelLayout::Stereo,
        n => ChannelLayout::Discrete(n as u8),
    };
    AudioFormat::new(config.sample_rate, layout)
        .map_err(|e| NativeAudioError::UnsupportedConfig(e.to_string()))
}

fn capacity(format: AudioFormat, queue_ms: u16) -> Result<usize, NativeAudioError> {
    if !(20..=500).contains(&queue_ms) {
        return Err(NativeAudioError::UnsupportedConfig(
            "queue_ms must be 20..=500".into(),
        ));
    }
    Ok((u64::from(format.sample_rate_hz()) * u64::from(queue_ms)).div_ceil(1000) as usize)
}

fn fail_callback(error: cpal::Error, telemetry: &PortTelemetry) {
    if error.kind() == cpal::ErrorKind::Xrun {
        telemetry.record_xrun();
        return;
    }
    let code = match error.kind() {
        cpal::ErrorKind::PermissionDenied => 1,
        cpal::ErrorKind::DeviceNotAvailable => 2,
        _ => 3,
    };
    telemetry.fail(code);
}

/// Converts native input using exactly the same preallocated endpoint as virtual tests.
/// No allocation, locking, packetization, channel mapping or processing occurs here.
pub fn capture_callback<T: Sample>(
    data: &[T],
    ingress: &mut CaptureIngress,
    timestamp_ns: Option<u64>,
) where
    f32: FromSample<T>,
{
    capture_callback_at(data, ingress, timestamp_ns, None);
}

/// Same callback conversion with an optional host-side handoff observation.
pub fn capture_callback_at<T: Sample>(
    data: &[T],
    ingress: &mut CaptureIngress,
    timestamp_ns: Option<u64>,
    handoff_ns: Option<u64>,
) where
    f32: FromSample<T>,
{
    ingress.begin_callback_at(handoff_ns);
    let format = ingress.format();
    let channels = usize::from(format.channels());
    let mut samples = [0.0; 32];
    for (index, frame) in data.chunks_exact(channels).enumerate() {
        for (destination, sample) in samples.iter_mut().zip(frame) {
            *destination = f32::from_sample(*sample);
        }
        let stamp = timestamp_ns.and_then(|ns| {
            ns.checked_add(index as u64 * 1_000_000_000 / u64::from(format.sample_rate_hz()))
        });
        ingress.push(&samples[..channels], stamp, CaptureFlags::default());
    }
}

/// Fills actual native output and publishes its post-conversion PCM reference.
/// Startup and underrun silence are included. No DSP or host calls run in this callback.
pub fn playback_callback<T: Sample + FromSample<f32>>(
    data: &mut [T],
    egress: &mut PlaybackEgress,
    timestamp_ns: Option<u64>,
) where
    f32: FromSample<T>,
{
    egress.begin_callback();
    let format = egress.format();
    let channels = usize::from(format.channels());
    let mut chunks = data.chunks_exact_mut(channels);
    for (index, output) in chunks.by_ref().enumerate() {
        let stamp = timestamp_ns.and_then(|ns| {
            ns.checked_add(index as u64 * 1_000_000_000 / u64::from(format.sample_rate_hz()))
        });
        let mut frame = egress.next_frame(stamp);
        for (index, sample) in output.iter_mut().enumerate() {
            *sample = T::from_sample(frame.samples[index].clamp(-1.0, 1.0));
            frame.samples[index] = f32::from_sample(*sample);
        }
        egress.publish_reference(frame);
    }
    chunks.into_remainder().fill(T::EQUILIBRIUM);
}

/// CPAL capture/endpoint port; callbacks retain their producer until stream teardown.
pub struct CpalCapture {
    stream: Option<cpal::Stream>,
    reader: FrameReader,
    telemetry: Arc<PortTelemetry>,
    pending: Option<ports::DeviceFrame>,
    next_position: Option<u64>,
    domain: ClockDomain,
    origin: Instant,
}
impl CpalCapture {
    /// Opens microphone or output-endpoint loopback with native format and bounded queue.
    /// The host owns endpoint feedback consent and permission UX; no implicit fallback is used.
    pub fn open(
        selected: Option<&str>,
        direction: DeviceDirection,
        preferred: AudioFormat,
        queue_ms: u16,
        domain: ClockDomain,
    ) -> Result<Self, NativeAudioError> {
        let device = resolve_device(direction, selected)?;
        let supported = choose_config(&device, direction, preferred)?;
        let sample_format = supported.sample_format();
        let config = supported.config();
        let format = native_format(&config)?;
        let (mut ingress, reader) = ports::capture_pair(format, capacity(format, queue_ms)?)
            .map_err(|e| NativeAudioError::UnsupportedConfig(e.to_string()))?;
        let telemetry = ingress.telemetry();
        let errors = Arc::clone(&telemetry);
        let error = move |e| fail_callback(e, &errors);
        let origin = Instant::now();
        macro_rules! build {
            ($ty:ty) => {
                device.build_input_stream(
                    config,
                    move |data: &[$ty], info| {
                        capture_callback_at(
                            data,
                            &mut ingress,
                            info.timestamp().capture.as_nanos().try_into().ok(),
                            origin.elapsed().as_nanos().try_into().ok(),
                        )
                    },
                    error,
                    Some(Duration::from_secs(5)),
                )
            };
        }
        let stream = match sample_format {
            SampleFormat::I8 => build!(i8),
            SampleFormat::I16 => build!(i16),
            SampleFormat::I32 => build!(i32),
            SampleFormat::U8 => build!(u8),
            SampleFormat::U16 => build!(u16),
            SampleFormat::F32 => build!(f32),
            SampleFormat::F64 => build!(f64),
            _ => {
                return Err(NativeAudioError::UnsupportedConfig(
                    "native sample format unavailable".into(),
                ));
            }
        }
        .map_err(|e| map_cpal_error(e, Some(direction)))?;
        stream
            .play()
            .map_err(|e| map_cpal_error(e, Some(direction)))?;
        Ok(Self {
            stream: Some(stream),
            reader,
            telemetry,
            pending: None,
            next_position: None,
            domain,
            origin,
        })
    }
    /// Returns PCM-free atomic health and native error categories.
    pub fn telemetry(&self) -> Arc<PortTelemetry> {
        Arc::clone(&self.telemetry)
    }
    /// Returns the native capture format without a packet-duration restriction.
    pub fn native_format(&self) -> AudioFormat {
        self.reader.format()
    }
    /// Pops one raw native frame including raw device timestamp/flags.
    pub fn read_device_frame(&mut self) -> Option<ports::DeviceFrame> {
        self.pending.take().or_else(|| self.reader.pop())
    }
    /// Returns queued native frames.
    pub fn queued_frames(&self) -> usize {
        self.reader.queued_frames() + usize::from(self.pending.is_some())
    }
    /// Returns nanoseconds in the same host origin as DeviceFrame.host_handoff_ns.
    pub fn host_elapsed_ns(&self) -> u64 {
        self.origin
            .elapsed()
            .as_nanos()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}
impl CapturePort for CpalCapture {
    fn format(&self) -> AudioFormat {
        self.native_format()
    }
    fn read_into(&mut self, output: &mut [f32]) -> AudioResult<Option<CaptureRead>> {
        let format = self.native_format();
        format.frames_in(output.len())?;
        if output.is_empty() {
            return Ok(None);
        }
        if self.telemetry.snapshot().error_code != 0 {
            return Err(AudioError::DeviceLost(
                "CPAL capture callback failed".into(),
            ));
        }
        let Some(first) = self.read_device_frame() else {
            return Ok(None);
        };
        let gap = self
            .next_position
            .map(|p| first.sample_position.saturating_sub(p))
            .unwrap_or(0);
        let channels = usize::from(format.channels());
        output[..channels].copy_from_slice(&first.samples[..channels]);
        let mut frames = 1_u64;
        let mut next = first.sample_position + 1;
        for destination in output[channels..].chunks_exact_mut(channels) {
            let Some(frame) = self.reader.pop() else {
                break;
            };
            if frame.sample_position != next || frame.flags.discontinuity {
                self.pending = Some(frame);
                break;
            }
            destination.copy_from_slice(&frame.samples[..channels]);
            frames += 1;
            next += 1;
        }
        self.next_position = Some(next);
        // CPAL raw clock is not mapped to the host's monotonic epoch. Do not mark it measured.
        Ok(Some(CaptureRead {
            frames: SampleFrames::new(frames),
            gap_before: SampleFrames::new(gap),
            timestamp: ClockTimestamp {
                domain: self.domain,
                sample_position: first.sample_position,
                monotonic_ns: None,
                quality: TimestampQuality::Unavailable,
                uncertainty_ns: None,
            },
        }))
    }
    fn stop(&mut self) -> AudioResult<()> {
        self.stream.take();
        self.telemetry.stop();
        Ok(())
    }
}
impl Drop for CpalCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Native playback with a separately owned actual-consumed reference reader.
pub struct CpalPlayback {
    stream: Option<cpal::Stream>,
    writer: FrameWriter,
    reference: Option<FrameReader>,
    telemetry: Arc<PortTelemetry>,
    sample_format: SampleFormat,
}
impl CpalPlayback {
    /// Opens a device using millisecond queue/prebuffer settings; neither is tied to ptime.
    pub fn open(
        selected: Option<&str>,
        preferred: AudioFormat,
        queue_ms: u16,
        prebuffer_ms: u16,
    ) -> Result<Self, NativeAudioError> {
        if prebuffer_ms > queue_ms {
            return Err(NativeAudioError::UnsupportedConfig(
                "prebuffer exceeds queue_ms".into(),
            ));
        }
        let device = resolve_device(DeviceDirection::Output, selected)?;
        let supported = choose_config(&device, DeviceDirection::Output, preferred)?;
        let sample_format = supported.sample_format();
        let config = supported.config();
        let format = native_format(&config)?;
        let prebuffer =
            (u64::from(format.sample_rate_hz()) * u64::from(prebuffer_ms)).div_ceil(1000) as usize;
        let (writer, mut egress, reference) =
            ports::playback_pair(format, capacity(format, queue_ms)?, prebuffer)
                .map_err(|e| NativeAudioError::UnsupportedConfig(e.to_string()))?;
        let telemetry = writer.telemetry();
        let errors = Arc::clone(&telemetry);
        let error = move |e| fail_callback(e, &errors);
        macro_rules! build {
            ($ty:ty) => {
                device.build_output_stream(
                    config,
                    move |data: &mut [$ty], info| {
                        playback_callback(
                            data,
                            &mut egress,
                            info.timestamp().playback.as_nanos().try_into().ok(),
                        )
                    },
                    error,
                    Some(Duration::from_secs(5)),
                )
            };
        }
        let stream = match sample_format {
            SampleFormat::I8 => build!(i8),
            SampleFormat::I16 => build!(i16),
            SampleFormat::I32 => build!(i32),
            SampleFormat::U8 => build!(u8),
            SampleFormat::U16 => build!(u16),
            SampleFormat::F32 => build!(f32),
            SampleFormat::F64 => build!(f64),
            _ => {
                return Err(NativeAudioError::UnsupportedConfig(
                    "native sample format unavailable".into(),
                ));
            }
        }
        .map_err(|e| map_cpal_error(e, Some(DeviceDirection::Output)))?;
        stream
            .play()
            .map_err(|e| map_cpal_error(e, Some(DeviceDirection::Output)))?;
        Ok(Self {
            stream: Some(stream),
            writer,
            reference: Some(reference),
            telemetry,
            sample_format,
        })
    }
    /// Takes the actual-presented reference once for an independent AEC worker.
    pub fn take_reference(&mut self) -> Option<FrameReader> {
        self.reference.take()
    }
    /// Returns PCM-free backend health and sample/drop accounting.
    pub fn telemetry(&self) -> Arc<PortTelemetry> {
        Arc::clone(&self.telemetry)
    }
    /// Returns native queue depth, in per-channel frames.
    pub fn queued_frames(&self) -> usize {
        self.writer.queued_frames()
    }
    /// Returns the chosen device sample representation.
    pub fn sample_format(&self) -> SampleFormat {
        self.sample_format
    }
}
impl PlaybackPort for CpalPlayback {
    fn format(&self) -> AudioFormat {
        self.writer.format()
    }
    fn write(&mut self, pcm: &[f32]) -> AudioResult<SampleFrames> {
        if self.telemetry.snapshot().error_code != 0 {
            return Err(AudioError::DeviceLost("CPAL output callback failed".into()));
        }
        self.writer.write(pcm)
    }
    fn presented_frames(&self) -> SampleFrames {
        SampleFrames::new(self.telemetry.snapshot().device_frames)
    }
    fn stop(&mut self) -> AudioResult<()> {
        self.stream.take();
        self.telemetry.stop();
        Ok(())
    }
}
impl Drop for CpalPlayback {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
