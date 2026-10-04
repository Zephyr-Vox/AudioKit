//! Worker-only bounded native PCM acquisition. Never performs I/O in a callback.
use super::{MaterialCaptureOptions, MaterialCaptureReport};
use crate::{Cancellation, Diagnostics, Error, ProgressEvent, ProgressUnit, Result, RunConfig};
use audiokit::backend::CapturePort;
use audiokit::{AudioFormat, ChannelLayout, ClockDomain};
use audiokit_platform::cpal::{CpalCapture, DeviceDirection};
use audiokit_platform::ports::DeviceFrame;
use std::{
    io::Cursor,
    path::Path,
    thread,
    time::{Duration, Instant},
};

struct Material {
    pcm: Vec<f32>,
    report: MaterialCaptureReport,
    target: usize,
    next: u64,
}
impl Material {
    fn new(
        config: &RunConfig,
        format: AudioFormat,
        options: MaterialCaptureOptions,
    ) -> Result<Self> {
        options.validate()?;
        if !matches!(format.layout(), ChannelLayout::Mono | ChannelLayout::Stereo) {
            return Err(Error::Capability(
                "microphone material requires mono/stereo".into(),
            ));
        }
        let target = format.sample_rate_hz() as usize * options.duration_ms as usize / 1000;
        let samples = target
            .checked_mul(usize::from(format.channels()))
            .ok_or_else(|| Error::Invalid("capture sample budget overflow".into()))?;
        if samples > config.max_pcm_samples || samples as u64 * 4 + 128 > config.max_input_bytes {
            return Err(Error::Invalid(
                "requested recording exceeds input resource budget".into(),
            ));
        }
        let report = MaterialCaptureReport {
            schema_version: 1,
            format,
            options,
            stop_reason: "duration_reached".into(),
            material_frames: 0,
            captured_frames: 0,
            inserted_gap_frames: 0,
            rejected_cursor_frames: 0,
            discontinuity_boundaries: 0,
            timestamp_error_boundaries: 0,
            first_device_timestamp_ns: None,
            last_device_timestamp_ns: None,
            max_handoff_age_ns: None,
            callbacks: 0,
            port_dropped_frames: 0,
            queue_high_water_frames: 0,
            xruns: 0,
            error_code: 0,
            excluded_queued_frames: 0,
        };
        Ok(Self {
            pcm: Vec::with_capacity(samples),
            report,
            target,
            next: 0,
        })
    }
    fn complete(&self) -> bool {
        self.report.material_frames as usize >= self.target
    }
    fn push(&mut self, frame: DeviceFrame, host_now: u64) -> Result<()> {
        if self.complete() {
            return Ok(());
        }
        if frame.sample_position < self.next || frame.sample_position == u64::MAX {
            self.report.rejected_cursor_frames += 1;
            return Ok(());
        }
        let channels = usize::from(self.report.format.channels());
        if !frame.samples[..channels]
            .iter()
            .all(|s| s.is_finite() && s.abs() <= 64.0)
        {
            return Err(Error::Execution("invalid native microphone PCM".into()));
        }
        let gap = (frame.sample_position - self.next)
            .min((self.target as u64) - self.report.material_frames);
        self.pcm
            .resize(self.pcm.len() + gap as usize * channels, 0.0);
        self.report.material_frames += gap;
        self.report.inserted_gap_frames += gap;
        self.next = frame.sample_position;
        if self.complete() {
            return Ok(());
        }
        self.pcm.extend_from_slice(&frame.samples[..channels]);
        self.next += 1;
        self.report.captured_frames += 1;
        self.report.material_frames += 1;
        self.report.discontinuity_boundaries += u64::from(frame.flags.discontinuity);
        self.report.timestamp_error_boundaries += u64::from(frame.flags.timestamp_error);
        if !frame.flags.timestamp_error
            && let Some(ns) = frame.device_timestamp_ns
        {
            self.report.first_device_timestamp_ns.get_or_insert(ns);
            self.report.last_device_timestamp_ns = Some(ns);
        }
        if let Some(age) = frame.host_handoff_ns.and_then(|t| host_now.checked_sub(t)) {
            self.report.max_handoff_age_ns = Some(
                self.report
                    .max_handoff_age_ns
                    .map_or(age, |old| old.max(age)),
            );
        }
        Ok(())
    }
    fn wav(&self) -> Result<Vec<u8>> {
        let mut bytes = Cursor::new(Vec::new());
        let mut writer = hound::WavWriter::new(
            &mut bytes,
            hound::WavSpec {
                channels: u16::from(self.report.format.channels()),
                sample_rate: self.report.format.sample_rate_hz(),
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )?;
        for &sample in &self.pcm {
            writer.write_sample(sample)?;
        }
        writer.finalize()?;
        Ok(bytes.into_inner())
    }
}

/// Explicitly opens one microphone, collects bounded native material, then closes it
/// before invoking the SAME offline runner. No realtime NS, monitoring or AEC is claimed.
/// `finish` ends acquisition early and processes it; `stop` cancels both phases and
/// finalizes a partial bundle when material exists. Retained raw WAV needs RunConfig consent.
/// `selected` is a local stable device ID, never exported. Output directory must be new.
/// No file/artifact work runs inside native callbacks. Acquisition does not open output.
pub fn record_microphone(
    config: &RunConfig,
    options: MaterialCaptureOptions,
    selected: Option<&str>,
    output: &Path,
    stop: &Cancellation,
    finish: &Cancellation,
    mut progress: impl FnMut(ProgressEvent),
) -> Result<Diagnostics> {
    options.validate()?;
    config.validate()?;
    if config.scenario == crate::Scenario::ReceiveSimulation {
        return Err(Error::Invalid(
            "microphone cannot supply encoded packet JSON".into(),
        ));
    }
    if output.exists() {
        return Err(Error::Invalid(
            "recording output directory must be new".into(),
        ));
    }
    if stop.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let preferred = AudioFormat::new(48_000, ChannelLayout::Mono)?;
    let mut port = CpalCapture::open(
        selected,
        DeviceDirection::Input,
        preferred,
        options.queue_ms,
        ClockDomain::new(1)?,
    )
    .map_err(|e| Error::Execution(e.to_string()))?;
    let format = port.native_format();
    let mut material = Material::new(config, format, options)?;
    crate::runner::validate_plan(config, &config.plan(format)?)?;
    let origin = Instant::now();
    let mut last_frame = Instant::now();
    let mut last_progress = Instant::now();
    while !material.complete() {
        if stop.is_cancelled() {
            material.report.stop_reason = "cancelled".into();
            break;
        }
        if finish.is_cancelled() {
            material.report.stop_reason = "finished_early".into();
            break;
        }
        if port.telemetry().snapshot().error_code != 0 {
            material.report.stop_reason = "device_error".into();
            break;
        }
        if let Some(frame) = port.read_device_frame() {
            let before = material.report.material_frames;
            if material.push(frame, port.host_elapsed_ns()).is_err() {
                material.report.stop_reason = "device_error".into();
                break;
            }
            if material.report.material_frames > before {
                last_frame = Instant::now();
            }
        } else {
            thread::sleep(Duration::from_millis(1));
        }
        if last_frame.elapsed() > Duration::from_millis(u64::from(options.stall_timeout_ms))
            || origin.elapsed()
                > Duration::from_millis(
                    u64::from(options.duration_ms) + u64::from(options.stall_timeout_ms),
                )
        {
            material.report.stop_reason = "stalled".into();
            break;
        }
        if last_progress.elapsed() >= Duration::from_millis(50) {
            progress(ProgressEvent {
                input_frames: material.report.material_frames,
                total_frames: material.target as u64,
                unit: ProgressUnit::NativeFrames,
            });
            last_progress = Instant::now();
        }
    }
    port.stop()?;
    let stats = port.telemetry().snapshot();
    material.report.callbacks = stats.callbacks;
    material.report.port_dropped_frames = stats.dropped_frames;
    material.report.queue_high_water_frames = stats.queue_high_water_frames;
    material.report.xruns = stats.xruns;
    material.report.error_code = stats.error_code;
    material.report.excluded_queued_frames = port.queued_frames() as u64;
    drop(port);
    if material.pcm.is_empty() {
        return Err(if stop.is_cancelled() {
            Error::Cancelled
        } else {
            Error::Execution("microphone produced no material".into())
        });
    }
    let bytes = material.wav()?;
    crate::runner::run_material(config, bytes, output, stop, progress, Some(material.report))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> RunConfig {
        let mut c = RunConfig::default();
        c.processing.enabled = false;
        c
    }
    fn replace_diagnostics(root: &Path, manifest: &crate::Manifest, diagnostics: &Diagnostics) {
        let mut manifest = manifest.clone();
        let bytes = serde_json::to_vec_pretty(diagnostics).unwrap();
        std::fs::write(root.join("diagnostics.json"), &bytes).unwrap();
        let artifact = manifest
            .artifacts
            .iter_mut()
            .find(|a| a.path == "diagnostics.json")
            .unwrap();
        artifact.sha256 = crate::io::hash(&bytes);
        artifact.bytes = bytes.len() as u64;
        std::fs::write(
            root.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }
    #[test]
    fn native_cursor_gaps_remain_explicit_and_bounded() {
        let c = config();
        let format = AudioFormat::new(48_000, ChannelLayout::Stereo).unwrap();
        let mut m = Material::new(
            &c,
            format,
            MaterialCaptureOptions {
                duration_ms: 100,
                ..Default::default()
            },
        )
        .unwrap();
        let mut f = DeviceFrame::default();
        f.samples[..2].copy_from_slice(&[0.2, -0.1]);
        f.host_handoff_ns = Some(10);
        f.device_timestamp_ns = Some(5);
        m.push(f, 30).unwrap();
        f.sample_position = 3;
        m.push(f, 40).unwrap();
        assert_eq!(&m.pcm, &[0.2, -0.1, 0.0, 0.0, 0.0, 0.0, 0.2, -0.1]);
        assert_eq!(m.report.inserted_gap_frames, 2);
        assert_eq!(m.report.max_handoff_age_ns, Some(30));
        m.push(f, 50).unwrap();
        assert_eq!(m.report.rejected_cursor_frames, 1);
        f.sample_position = u64::MAX - 1;
        m.push(f, 60).unwrap();
        assert!(m.complete());
        assert_eq!(m.pcm.len(), 9600);
        assert!(!m.report.healthy());
        m.report.validate(&c, format, 4800).unwrap();
        assert_eq!(
            crate::io::wav(&m.wav().unwrap(), c.max_pcm_samples)
                .unwrap()
                .1,
            m.pcm
        );
    }
    #[test]
    fn recording_preflight_and_metadata_do_not_open_devices() {
        let mut c = config();
        c.max_input_bytes = 1;
        assert!(
            Material::new(
                &c,
                AudioFormat::new(48_000, ChannelLayout::Mono).unwrap(),
                Default::default()
            )
            .is_err()
        );
        let stop = Cancellation::default();
        stop.cancel();
        assert!(matches!(
            record_microphone(
                &config(),
                Default::default(),
                None,
                Path::new("never-created"),
                &stop,
                &Cancellation::default(),
                |_| {}
            ),
            Err(Error::Cancelled)
        ));
    }
    #[test]
    fn native_material_bundle_replays_dsp_not_hardware_and_validates_evidence() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "audiokit-material-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let mut c = config();
        c.retain_input = true;
        let format = AudioFormat::new(48_000, ChannelLayout::Mono).unwrap();
        let mut m = Material::new(
            &c,
            format,
            MaterialCaptureOptions {
                duration_ms: 100,
                ..Default::default()
            },
        )
        .unwrap();
        for i in 0..1001 {
            let mut f = DeviceFrame {
                sample_position: i,
                ..Default::default()
            };
            f.samples[0] = (i as f32 * 0.13).sin() * 0.2;
            m.push(f, 0).unwrap();
        }
        m.report.stop_reason = "finished_early".into();
        m.report.callbacks = 5;
        let stop = Cancellation::default();
        let output = root.join("recorded");
        crate::runner::run_material(
            &c,
            m.wav().unwrap(),
            &output,
            &stop,
            |_| {},
            Some(m.report.clone()),
        )
        .unwrap();
        let (manifest, diagnostics) = crate::inspect(&output).unwrap();
        assert!(diagnostics.material_capture.as_ref().unwrap().healthy());
        assert_eq!(
            crate::analyze(&output)
                .unwrap()
                .material_capture
                .unwrap()
                .captured_frames,
            1001
        );
        let exported = root.join("exported");
        crate::export_bundle(&output, &exported, &stop).unwrap();
        assert_eq!(
            crate::inspect(&exported)
                .unwrap()
                .1
                .material_capture
                .unwrap()
                .captured_frames,
            1001
        );
        let replayed = root.join("replayed");
        let replay = crate::replay(&exported, None, &replayed, &stop, |_| {}).unwrap();
        assert!(replay.material_capture.is_none());
        assert!(
            !replay
                .checks
                .iter()
                .any(|c| c.id == "material_capture_health")
        );
        let comparison = crate::compare(&output, &replayed).unwrap();
        assert!(comparison.same_output_bytes && comparison.same_config && comparison.same_plan);
        assert!(!comparison.same_checks);
        // Even with a recomputed hash, impossible material accounting is rejected.
        let mut inconsistent = diagnostics.clone();
        inconsistent
            .material_capture
            .as_mut()
            .unwrap()
            .captured_frames += 1;
        replace_diagnostics(&output, &manifest, &inconsistent);
        assert!(crate::inspect(&output).is_err());
        inconsistent = diagnostics.clone();
        inconsistent.material_capture = None;
        replace_diagnostics(&output, &manifest, &inconsistent);
        assert!(crate::inspect(&output).is_err());
        inconsistent = diagnostics;
        let check = inconsistent
            .checks
            .iter()
            .find(|c| c.id == "material_capture_health")
            .unwrap()
            .clone();
        inconsistent.checks.push(check);
        replace_diagnostics(&output, &manifest, &inconsistent);
        assert!(crate::inspect(&output).is_err());
        // Capture gaps remain a failed acquisition check, even if offline DSP succeeds.
        m.report.captured_frames -= 1;
        m.report.inserted_gap_frames += 1;
        let gap = crate::runner::run_material(
            &c,
            m.wav().unwrap(),
            &root.join("gap"),
            &stop,
            |_| {},
            Some(m.report),
        )
        .unwrap();
        assert_eq!(gap.status, "completed");
        assert!(
            gap.checks
                .iter()
                .any(|c| c.id == "material_capture_health" && !c.passed)
        );
        crate::inspect(&root.join("gap")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
