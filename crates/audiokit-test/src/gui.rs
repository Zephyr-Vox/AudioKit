//! Optional Slint frontend. DSP, file validation and exports run on one owned worker.
use crate::workbench::Job;
use audiokit_platform::cpal::{AudioDeviceInfo, list_devices};
use audiokit_testkit::{
    Cancellation, Diagnostics, Error, ExecutionPlan, Manifest, MaterialCaptureOptions, NodeStatus,
    ProgressEvent, ProgressUnit, Result, RunConfig, analyze, export_bundle, export_wav, inspect,
    plan_file, read_config, record_microphone, run, write_config,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[allow(missing_docs)]
mod ui {
    slint::include_modules!();
}
#[path = "gui_forms.rs"]
mod forms;
#[path = "gui_preview.rs"]
mod preview;
use ui::{StageRow, Workbench};

fn status(ui: &Workbench, text: slint::SharedString) {
    ui.set_status_prefix("".into());
    ui.set_status_detail("".into());
    ui.set_status(text);
}
fn detail_status(ui: &Workbench, prefix: &str, detail: String) {
    status(ui, format!("{prefix}: {detail}").into());
    ui.set_status_prefix(prefix.into());
    ui.set_status_detail(detail.into());
}
fn language(ui: &Workbench, index: i32, devices: &[AudioDeviceInfo]) -> Result<()> {
    let locale = match index {
        0 => "zh_CN",
        1 => "en",
        _ => return Err(Error::Invalid("unknown UI language".into())),
    };
    slint::select_bundled_translation(locale).map_err(|e| Error::Capability(e.to_string()))?;
    let default = ui
        .global::<ui::Strings>()
        .invoke_translate("System default".into());
    let mut outputs = vec![default.clone()];
    outputs.extend(
        devices
            .iter()
            .filter(|d| d.output)
            .map(|d| d.name.clone().into()),
    );
    let mut inputs = vec![default];
    inputs.extend(
        devices
            .iter()
            .filter(|d| d.input)
            .map(|d| d.name.clone().into()),
    );
    ui.set_output_devices(ModelRc::new(VecModel::from(outputs)));
    ui.set_input_devices(ModelRc::new(VecModel::from(inputs)));
    Ok(())
}

enum Outcome {
    Config(Box<RunConfig>),
    Plan(ExecutionPlan),
    Report(Box<Report>),
    Saved(PathBuf),
    Devices(Vec<AudioDeviceInfo>),
    Message(String),
}
struct Report {
    path: PathBuf,
    manifest: Manifest,
    diagnostics: Diagnostics,
    checks: String,
    evidence: String,
    latency: String,
}

/// Bound display text independently of the full, lossless exported artifacts.
fn text_preview(value: &serde_json::Value) -> Result<String> {
    fn compact(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.remove("bins");
                for v in map.values_mut() {
                    compact(v);
                }
            }
            serde_json::Value::Array(values) => {
                for v in values {
                    compact(v);
                }
            }
            _ => {}
        }
    }
    let mut value = value.clone();
    compact(&mut value);
    let mut text = serde_json::to_string_pretty(&value)?;
    if text.len() > 65_536 {
        let mut end = 65_536;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[Display truncated; full data in exported bundle]");
    }
    Ok(text)
}
fn report(path: PathBuf) -> Result<Outcome> {
    let (manifest, diagnostics) = inspect(&path)?;
    let analysis = analyze(&path)?;
    if analysis.run_id != diagnostics.run_id {
        return Err(Error::Invalid("bundle changed during inspection".into()));
    }
    let checks = diagnostics
        .checks
        .iter()
        .map(|c| {
            format!(
                "{} {}: {}",
                if c.passed { "PASS" } else { "FAIL" },
                c.id,
                c.detail
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let evidence = text_preview(&serde_json::to_value(analysis)?)?;
    let latency = text_preview(&diagnostics.latency)?;
    Ok(Outcome::Report(Box::new(Report {
        path,
        manifest,
        diagnostics,
        checks,
        evidence,
        latency,
    })))
}
fn fresh(base: &Path, prefix: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    base.join(format!(
        "{prefix}-{time}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
fn show_error(ui: &Workbench, error: &Error) {
    ui.set_failed(true);
    detail_status(ui, "Operation failed", error.to_string());
}
fn show_plan(ui: &Workbench, plan: &ExecutionPlan, state: &str) {
    ui.set_coverage(format!("{state} / {}", plan.coverage).into());
    ui.set_stages(ModelRc::new(VecModel::from(
        plan.stages
            .iter()
            .map(|stage| StageRow {
                name: stage.id.clone().into(),
                state: match stage.status {
                    NodeStatus::Applied => "Applied",
                    NodeStatus::Bypassed => "Bypassed",
                    NodeStatus::NotCovered => "Not covered",
                }
                .into(),
                applied: stage.status == NodeStatus::Applied,
            })
            .collect::<Vec<_>>(),
    )));
}
fn metric(value: &serde_json::Value, key: &str) -> slint::SharedString {
    value[key]
        .as_u64()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "Not covered".into())
        .into()
}

struct State {
    config: RunConfig,
    job: Option<Job<Outcome>>,
    finish_capture: Option<Cancellation>,
    result: Option<PathBuf>,
    devices: Vec<AudioDeviceInfo>,
    closing: bool,
}
impl State {
    fn clear_result(&mut self, ui: &Workbench) {
        self.result = None;
        ui.set_has_result(false);
        ui.set_playable(false);
        ui.set_coverage("Preparing / input not yet validated".into());
        ui.set_stages(ModelRc::default());
        ui.set_peak("--".into());
        ui.set_clamps("--".into());
        ui.set_gaps("--".into());
        ui.set_cpu("--".into());
        ui.set_checks("".into());
        ui.set_evidence("".into());
        ui.set_latency("".into());
        ui.set_material_evidence("".into());
    }
    fn apply(&mut self, ui: &Workbench, mut config: RunConfig, imported: bool) -> Result<()> {
        // A preset is not fresh authorization to share microphone/audio content.
        if imported {
            config.retain_input = false;
        }
        forms::put(ui, &config)?;
        self.config = config;
        ui.set_failed(false);
        if self.result.is_none() {
            ui.set_coverage("Planned / input not validated".into());
            ui.set_stages(ModelRc::default());
        }
        Ok(())
    }
    fn start(
        &mut self,
        ui: &Workbench,
        work: impl FnOnce(Cancellation, Box<dyn FnMut(ProgressEvent) + Send>) -> Result<Outcome>
        + Send
        + 'static,
    ) -> Result<()> {
        if self.job.is_some() || self.closing {
            return Err(Error::Invalid("another operation is active".into()));
        }
        self.job = Some(Job::start(work)?);
        ui.set_busy(true);
        ui.set_failed(false);
        ui.set_progress(0.0);
        status(ui, "Working".into());
        Ok(())
    }
    fn poll(&mut self, ui: &Workbench) {
        if let Some(job) = &self.job
            && let Some(p) = job.progress()
        {
            if self.finish_capture.is_some() {
                let recording = p.unit == ProgressUnit::NativeFrames
                    && !self
                        .finish_capture
                        .as_ref()
                        .is_some_and(Cancellation::is_cancelled);
                ui.set_recording(recording);
                if !recording {
                    status(ui, "Working".into());
                }
            }
            ui.set_progress(if p.total_frames == 0 {
                0.0
            } else {
                (p.input_frames as f64 / p.total_frames as f64).clamp(0.0, 1.0) as f32
            });
        }
        if let Some(result) = self.job.as_mut().and_then(Job::finish) {
            self.job = None;
            self.finish_capture = None;
            ui.set_recording(false);
            ui.set_busy(false);
            match result.and_then(|outcome| self.accept(ui, outcome)) {
                Ok(()) => {}
                Err(e) => show_error(ui, &e),
            }
        }
        if self.closing && self.job.is_none() {
            let _ = ui.hide();
            let _ = slint::quit_event_loop();
        }
    }
    fn accept(&mut self, ui: &Workbench, outcome: Outcome) -> Result<()> {
        match outcome {
            Outcome::Config(c) => {
                self.apply(ui, *c, true)?;
                status(ui, "Preset loaded / source sharing off".into());
            }
            Outcome::Plan(p) => {
                if self.result.is_none() {
                    show_plan(ui, &p, "Validated");
                }
                detail_status(ui, "Validated", p.coverage);
            }
            Outcome::Saved(p) => detail_status(ui, "Saved", p.display().to_string()),
            Outcome::Message(message) => detail_status(ui, "Preview completed", message),
            Outcome::Devices(devices) => {
                ui.set_output_device(0);
                ui.set_input_device(0);
                self.devices = devices;
                language(ui, ui.get_language(), &self.devices)?;
                status(ui, "Device list refreshed".into());
            }
            Outcome::Report(r) => {
                let d = &r.diagnostics;
                show_plan(ui, &d.plan, "Recorded");
                let peak = d.output_signal["max_peak_q15"]
                    .as_f64()
                    .map(|v| {
                        if v == 0.0 {
                            "-inf dBFS".into()
                        } else {
                            format!("{:.2} dBFS", 20.0 * (v / 32768.0).log10())
                        }
                    })
                    .unwrap_or_else(|| "Unknown".into());
                ui.set_peak(peak.into());
                ui.set_clamps(metric(
                    &d.graph_statistics["steady_render"],
                    "safety_clamped_samples",
                ));
                ui.set_gaps(metric(
                    &d.graph_statistics["steady_render"],
                    "missing_frames",
                ));
                ui.set_cpu(metric(&d.latency["receive_execution"], "over_budget_calls"));
                ui.set_checks(r.checks.into());
                ui.set_evidence(r.evidence.into());
                ui.set_latency(r.latency.into());
                ui.set_material_evidence(match &d.material_capture {
                    Some(capture) => text_preview(&serde_json::to_value(capture)?)?.into(),
                    None => "".into(),
                });
                ui.set_result_run(d.run_id.clone().into());
                ui.set_result_state(d.status.clone().into());
                ui.set_result_rate(d.plan.output_format.sample_rate_hz().to_string().into());
                ui.set_result_channels(d.plan.output_format.channels().to_string().into());
                ui.set_result_retained(r.manifest.input_audio_authorized);
                ui.set_result_drops(d.trace_events_dropped.to_string().into());
                ui.set_result_path(r.path.to_string_lossy().as_ref().into());
                let pass = r.manifest.complete
                    && d.status == "completed"
                    && d.checks.iter().all(|c| c.passed);
                ui.set_failed(!pass);
                status(
                    ui,
                    if pass {
                        "Completed / scoped checks passed"
                    } else {
                        "Partial or failed result / inspect checks"
                    }
                    .into(),
                );
                ui.set_has_result(true);
                ui.set_playable(d.output_frames > 0);
                ui.set_progress(1.0);
                self.result = Some(r.path);
            }
        }
        Ok(())
    }
    fn close(&mut self, ui: &Workbench) -> slint::CloseRequestResponse {
        self.closing = true;
        if let Some(job) = &self.job {
            job.cancel();
            status(ui, "Closing / waiting for worker finalization".into());
            slint::CloseRequestResponse::KeepWindowShown
        } else {
            slint::CloseRequestResponse::HideWindow
        }
    }
}
fn path(value: slint::SharedString, label: &str) -> Result<PathBuf> {
    if value.trim().is_empty() {
        return Err(Error::Invalid(format!("{label} is required")));
    }
    Ok(PathBuf::from(value.as_str()))
}
fn bind(ui: &Workbench, state: Rc<RefCell<State>>) {
    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_language_changed(move || {
            if let Some(ui) = weak.upgrade()
                && let Err(e) = language(&ui, ui.get_language(), &state.borrow().devices)
            {
                show_error(&ui, &e);
            }
        });
    }
    macro_rules! action {
        ($name:ident, $body:expr) => {{
            let weak = ui.as_weak();
            let state = Rc::clone(&state);
            ui.$name(move || {
                if let Some(ui) = weak.upgrade() {
                    let mut state = state.borrow_mut();
                    if state.job.is_some() || state.closing {
                        return;
                    }
                    let result: Result<()> = ($body)(&ui, &mut state);
                    if let Err(e) = result {
                        show_error(&ui, &e);
                    }
                }
            });
        }};
    }
    action!(on_choose_input, |ui: &Workbench, _: &mut State| {
        let dialog = rfd::FileDialog::new();
        let dialog = if ui.get_scenario() == 3 {
            dialog.add_filter("Packet JSON", &["json"])
        } else {
            dialog.add_filter("WAV", &["wav"])
        };
        if let Some(p) = dialog.pick_file() {
            ui.set_input_path(p.to_string_lossy().as_ref().into());
        }
        Ok(())
    });
    action!(on_choose_output, |ui: &Workbench, _: &mut State| {
        if let Some(p) = rfd::FileDialog::new().pick_folder() {
            ui.set_output_base(p.to_string_lossy().as_ref().into());
        }
        Ok(())
    });
    action!(on_select_profile, |ui: &Workbench, s: &mut State| s.apply(
        ui,
        forms::defaults(ui.get_scenario(), ui.get_profile())?,
        false
    ));
    action!(on_reset, |ui: &Workbench, s: &mut State| s.apply(
        ui,
        forms::defaults(ui.get_scenario(), ui.get_profile())?,
        false
    ));
    action!(on_sync_json, |ui: &Workbench, s: &mut State| s.apply(
        ui,
        forms::get(ui, &s.config)?,
        false
    ));
    action!(on_apply_json, |ui: &Workbench, s: &mut State| {
        let text = ui.get_preset_json();
        if text.len() > 1_048_576 {
            return Err(Error::Invalid("preset exceeds 1 MiB".into()));
        }
        let c: RunConfig = serde_json::from_str(&text)?;
        c.validate()?;
        s.apply(ui, c, true)
    });
    action!(on_load_preset, |ui: &Workbench, s: &mut State| {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Preset JSON", &["json"])
            .pick_file()
        {
            s.start(ui, move |_, _| {
                let c = read_config(&p)?;
                c.validate()?;
                Ok(Outcome::Config(Box::new(c)))
            })?;
        }
        Ok(())
    });
    action!(on_save_preset, |ui: &Workbench, s: &mut State| {
        let c = forms::get(ui, &s.config)?;
        if let Some(p) = rfd::FileDialog::new()
            .set_file_name("preset.json")
            .save_file()
        {
            s.start(ui, move |_, _| {
                write_config(&p, &c)?;
                Ok(Outcome::Saved(p))
            })?;
        }
        Ok(())
    });
    action!(on_validate, |ui: &Workbench, s: &mut State| {
        let c = forms::get(ui, &s.config)?;
        let input = path(ui.get_input_path(), "input")?;
        s.apply(ui, c.clone(), false)?;
        s.start(ui, move |_, _| Ok(Outcome::Plan(plan_file(&c, &input)?)))
    });
    action!(on_execute, |ui: &Workbench, s: &mut State| {
        let c = forms::get(ui, &s.config)?;
        let input = path(ui.get_input_path(), "input")?;
        let base = path(ui.get_output_base(), "output folder")?;
        let output = fresh(&base, "run");
        s.apply(ui, c.clone(), false)?;
        s.start(ui, move |stop, progress| {
            plan_file(&c, &input)?;
            if stop.is_cancelled() {
                return Err(Error::Cancelled);
            }
            std::fs::create_dir_all(&base)?;
            match run(&c, &input, &output, &stop, progress) {
                Ok(_) => report(output),
                Err(e) if output.join("manifest.json").is_file() => report(output).map_err(|_| e),
                Err(e) => Err(e),
            }
        })?;
        s.clear_result(ui);
        Ok(())
    });
    action!(on_record, |ui: &Workbench, s: &mut State| {
        let c = forms::get(ui, &s.config)?;
        let seconds: u32 = ui
            .get_record_seconds()
            .parse()
            .map_err(|_| Error::Invalid("recording duration must be integer seconds".into()))?;
        let options = MaterialCaptureOptions {
            duration_ms: seconds
                .checked_mul(1000)
                .ok_or_else(|| Error::Invalid("recording duration overflow".into()))?,
            ..MaterialCaptureOptions::default()
        };
        options.validate()?;
        let index = ui.get_input_device();
        let device = if index == 0 {
            None
        } else {
            Some(
                s.devices
                    .iter()
                    .filter(|d| d.input)
                    .nth((index - 1) as usize)
                    .ok_or_else(|| Error::Invalid("input device selection expired".into()))?
                    .id
                    .clone(),
            )
        };
        let base = path(ui.get_output_base(), "output folder")?;
        let output = fresh(&base, "microphone");
        let finish = Cancellation::default();
        let worker_finish = finish.clone();
        s.apply(ui, c.clone(), false)?;
        s.start(ui, move |stop, progress| {
            if stop.is_cancelled() {
                return Err(Error::Cancelled);
            }
            std::fs::create_dir_all(&base)?;
            match record_microphone(
                &c,
                options,
                device.as_deref(),
                &output,
                &stop,
                &worker_finish,
                progress,
            ) {
                Ok(_) => report(output),
                Err(e) if output.join("manifest.json").is_file() => report(output).map_err(|_| e),
                Err(e) => Err(e),
            }
        })?;
        s.finish_capture = Some(finish);
        s.clear_result(ui);
        ui.set_recording(true);
        status(ui, "Recording microphone".into());
        Ok(())
    });
    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_finish_recording(move || {
            if let Some(ui) = weak.upgrade()
                && ui.get_recording()
                && let Some(finish) = &state.borrow().finish_capture
            {
                finish.cancel();
                ui.set_recording(false);
                status(&ui, "Working".into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = Rc::clone(&state);
        ui.on_cancel(move || {
            if let Some(ui) = weak.upgrade()
                && let Some(job) = &state.borrow().job
            {
                job.cancel();
                status(&ui, "Stopping / finalizing partial result".into());
            }
        });
    }
    action!(on_load_result, |ui: &Workbench, s: &mut State| {
        if let Some(p) = rfd::FileDialog::new().pick_folder() {
            s.start(ui, move |_, _| report(p))?;
        }
        Ok(())
    });
    action!(on_export_bundle, |ui: &Workbench, s: &mut State| {
        let root = s
            .result
            .clone()
            .ok_or_else(|| Error::Invalid("no result".into()))?;
        if let Some(parent) = rfd::FileDialog::new().pick_folder() {
            let p = fresh(&parent, "bundle");
            s.start(ui, move |stop, _| {
                export_bundle(&root, &p, &stop)?;
                Ok(Outcome::Saved(p))
            })?;
        }
        Ok(())
    });
    action!(on_export_wav, |ui: &Workbench, s: &mut State| {
        let root = s
            .result
            .clone()
            .ok_or_else(|| Error::Invalid("no result".into()))?;
        if let Some(p) = rfd::FileDialog::new()
            .set_file_name("processed.wav")
            .save_file()
        {
            s.start(ui, move |stop, _| {
                export_wav(&root, &p, &stop)?;
                Ok(Outcome::Saved(p))
            })?;
        }
        Ok(())
    });
    action!(on_refresh_outputs, |ui: &Workbench, s: &mut State| s.start(
        ui,
        |_, _| {
            Ok(Outcome::Devices(
                list_devices().map_err(|e| Error::Execution(e.to_string()))?,
            ))
        }
    ));
    action!(on_preview, |ui: &Workbench, s: &mut State| {
        let root = s
            .result
            .clone()
            .ok_or_else(|| Error::Invalid("no result".into()))?;
        let index = ui.get_output_device();
        let device = if index == 0 {
            None
        } else {
            Some(
                s.devices
                    .iter()
                    .filter(|d| d.output)
                    .nth((index - 1) as usize)
                    .ok_or_else(|| Error::Invalid("playback device selection expired".into()))?
                    .id
                    .clone(),
            )
        };
        let volume = ui.get_volume();
        s.start(ui, move |stop, progress| {
            Ok(Outcome::Message(preview::play(
                &root,
                device.as_deref(),
                volume,
                &stop,
                progress,
            )?))
        })
    });
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        weak.upgrade()
            .map(|ui| state.borrow_mut().close(&ui))
            .unwrap_or(slint::CloseRequestResponse::HideWindow)
    });
}

pub(crate) fn launch(
    config: RunConfig,
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    locale: &str,
) -> Result<()> {
    config.validate()?;
    let index = match locale {
        "zh" | "zh-CN" | "zh_CN" => 0,
        "en" => 1,
        _ => return Err(Error::Invalid("language must be zh-CN or en".into())),
    };
    let ui = Workbench::new().map_err(|e| Error::Capability(format!("GUI backend: {e}")))?;
    ui.set_language(index);
    language(&ui, index, &[])?;
    let state = Rc::new(RefCell::new(State {
        config: config.clone(),
        job: None,
        finish_capture: None,
        result: None,
        devices: vec![],
        closing: false,
    }));
    state.borrow_mut().apply(&ui, config, true)?;
    ui.set_volume(0.2);
    ui.set_record_seconds(
        (MaterialCaptureOptions::default().duration_ms / 1000)
            .to_string()
            .into(),
    );
    if let Some(p) = input {
        ui.set_input_path(p.to_string_lossy().as_ref().into());
    }
    let base = output.unwrap_or(std::env::current_dir()?.join("target/audiokit-gui"));
    ui.set_output_base(base.to_string_lossy().as_ref().into());
    bind(&ui, Rc::clone(&state));
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    let polling = Rc::clone(&state);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            if let Some(ui) = weak.upgrade() {
                polling.borrow_mut().poll(&ui);
            }
        },
    );
    let result = ui
        .run()
        .map_err(|e| Error::Execution(format!("GUI event loop: {e}")));
    timer.stop();
    if let Some(job) = &state.borrow().job {
        job.cancel();
    }
    result
}

#[cfg(test)]
#[path = "gui_tests.rs"]
mod tests;
