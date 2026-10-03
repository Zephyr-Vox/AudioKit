//! Real compiled Slint + production runner tests, without native devices or network.
use super::*;
use slint::platform::{
    Platform, WindowAdapter,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use std::thread;

struct TestPlatform(Rc<MinimalSoftwareWindow>);
impl Platform for TestPlatform {
    fn create_window_adapter(
        &self,
    ) -> std::result::Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}
fn wait(ui: &Workbench, state: &Rc<RefCell<State>>) {
    let until = std::time::Instant::now() + Duration::from_secs(30);
    while state.borrow().job.is_some() {
        state.borrow_mut().poll(ui);
        assert!(std::time::Instant::now() < until, "worker timed out");
        thread::sleep(Duration::from_millis(1));
    }
}
fn screenshot(ui: &Workbench, output: &Path, w: u32, h: u32) {
    ui.window().set_size(slint::PhysicalSize::new(w, h));
    slint::platform::update_timers_and_animations();
    let pixels = ui.window().take_snapshot().unwrap();
    assert_eq!((pixels.width(), pixels.height()), (w, h));
    let data = pixels.as_bytes();
    assert!(
        data.as_chunks::<4>()
            .0
            .iter()
            .any(|p| p[0] < 80 && p[1] < 100 && p[2] < 110),
        "blank frame"
    );
    image::save_buffer(output, data, w, h, image::ColorType::Rgba8).unwrap();
}

#[test]
fn workbench_forms_worker_exports_and_software_layout() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(window))).unwrap();
    let ui = Workbench::new().unwrap();
    let config = RunConfig::for_scenario(audiokit_testkit::Scenario::FileProcessing);
    let state = Rc::new(RefCell::new(State {
        config: config.clone(),
        job: None,
        result: None,
        devices: vec![],
        closing: false,
    }));
    bind(&ui, Rc::clone(&state));
    state.borrow_mut().apply(&ui, config, false).unwrap();
    ui.set_volume(0.2);
    ui.set_output_devices(ModelRc::new(VecModel::from(vec!["System default".into()])));
    ui.show().unwrap();
    let root = fresh(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/validation/gui"),
        "test",
    );
    std::fs::create_dir_all(&root).unwrap();

    // Projection preserves invisible advanced settings, not just the widget values.
    for scenario in 0..4 {
        for profile in 0..2 {
            let mut c = forms::defaults(scenario, profile).unwrap();
            c.max_trace_events = 17;
            c.receive.render.source_limiter.release_ms = 173.0;
            state.borrow_mut().apply(&ui, c.clone(), false).unwrap();
            let projected = forms::get(&ui, &state.borrow().config).unwrap();
            assert_eq!(
                serde_json::to_value(&c).unwrap(),
                serde_json::to_value(&projected).unwrap()
            );
        }
    }
    let mut config = RunConfig::default();
    config.processing.enabled = false;
    config.retain_input = true;
    ui.set_preset_json(serde_json::to_string(&config).unwrap().into());
    ui.invoke_apply_json();
    assert!(!ui.get_retain_input());
    assert!(!state.borrow().config.retain_input);
    ui.set_input_path("".into());
    ui.invoke_execute();
    assert!(ui.get_failed());
    assert!(!ui.get_busy());
    assert!(state.borrow().job.is_none());

    let input = root.join("input.wav");
    let mut writer = hound::WavWriter::create(
        &input,
        hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    for i in 0..1001 {
        writer
            .write_sample(((i as f32 * 0.13).sin() * 10000.0) as i16)
            .unwrap();
    }
    writer.finalize().unwrap();
    ui.set_input_path(input.to_string_lossy().as_ref().into());
    ui.set_output_base(root.to_string_lossy().as_ref().into());
    ui.invoke_validate();
    wait(&ui, &state);
    assert!(!ui.get_failed(), "{}", ui.get_status());
    assert!(ui.get_coverage().starts_with("Validated"));
    ui.invoke_execute();
    assert!(ui.get_busy());
    let snapshot = serde_json::to_value(&state.borrow().config).unwrap();
    ui.invoke_reset(); // Programmatic invocation is guarded too, not only disabled buttons.
    assert_eq!(
        serde_json::to_value(&state.borrow().config).unwrap(),
        snapshot
    );
    wait(&ui, &state);
    assert!(ui.get_has_result());
    assert!(ui.get_playable());
    assert!(!ui.get_failed(), "{}", ui.get_status());
    assert_eq!(ui.get_clamps(), "Not covered");
    assert_eq!(ui.get_gaps(), "Not covered");
    assert_eq!(ui.get_cpu(), "Not covered");
    let bundle = state.borrow().result.clone().unwrap();
    let config = state.borrow().config.clone();
    write_config(&root.join("preset.json"), &config).unwrap();
    let baseline = root.join("api-baseline");
    run(&config, &input, &baseline, &Cancellation::default(), |_| {}).unwrap();
    let comparison = audiokit_testkit::compare(&bundle, &baseline).unwrap();
    assert!(
        comparison.same_output_bytes
            && comparison.same_plan
            && comparison.same_checks
            && comparison.same_config
    );
    export_bundle(&bundle, &root.join("exported"), &Cancellation::default()).unwrap();
    export_wav(
        &bundle,
        &root.join("exported.wav"),
        &Cancellation::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(root.join("exported.wav")).unwrap(),
        std::fs::read(bundle.join("processed.wav")).unwrap()
    );
    for (name, w, h) in [
        ("desktop.png", 1120, 900),
        ("narrow.png", 720, 900),
        ("compact.png", 720, 700),
    ] {
        screenshot(&ui, &root.join(name), w, h);
    }
    ui.set_scroll_position(-660.0);
    screenshot(&ui, &root.join("narrow-results.png"), 720, 900);
    ui.set_result_index(2);
    screenshot(&ui, &root.join("narrow-latency.png"), 720, 900);
    ui.set_scroll_position(0.0);
    for (panel, name) in [
        (2, "render.png"),
        (3, "protection.png"),
        (4, "advanced.png"),
    ] {
        ui.set_panel_index(panel);
        screenshot(&ui, &root.join(name), 720, 700);
    }
    // Invalid node values must fail before an output directory or a job exists.
    let c = forms::defaults(2, 1).unwrap();
    state.borrow_mut().apply(&ui, c, false).unwrap();
    let mut controls = ui.get_controls();
    controls.source_gain = "NaN".into();
    ui.set_controls(controls);
    ui.invoke_execute();
    assert!(ui.get_failed());
    assert!(state.borrow().job.is_none());
    for scenario in 0..3 {
        for profile in 0..2 {
            let mut c = forms::defaults(scenario, profile).unwrap();
            c.processing.enabled = profile == 0 && scenario < 2;
            if c.processing.enabled {
                c.processing.noise_suppression = audiokit_testkit::NoiseLevel::High;
                c.processing.adaptive_gain = true;
            }
            c.mix_stress.sources = 2;
            state.borrow_mut().apply(&ui, c.clone(), false).unwrap();
            ui.invoke_execute();
            wait(&ui, &state);
            assert!(!ui.get_failed(), "{}", ui.get_status());
            let gui_bundle = state.borrow().result.clone().unwrap();
            let api_bundle = root.join(format!("api-{scenario}-{profile}"));
            run(&c, &input, &api_bundle, &Cancellation::default(), |_| {}).unwrap();
            let comparison = audiokit_testkit::compare(&gui_bundle, &api_bundle).unwrap();
            assert!(
                comparison.same_output_bytes
                    && comparison.same_config
                    && comparison.same_plan
                    && comparison.same_checks
            );
        }
    }
    let mut c = RunConfig::default();
    c.processing.enabled = false;
    state.borrow_mut().apply(&ui, c, false).unwrap();
    ui.set_retain_input(true);
    ui.invoke_execute();
    wait(&ui, &state);
    let (manifest, _) = inspect(state.borrow().result.as_ref().unwrap()).unwrap();
    assert!(manifest.input_audio_authorized);
    // Cancel an actual graph after its first progress boundary; the partial WAV/manifest
    // must already be finalized when the GUI receives completion.
    let mut c = RunConfig::default();
    c.processing.enabled = false;
    let partial = root.join("cancelled");
    let partial_run = partial.clone();
    let partial_input = input.clone();
    state
        .borrow_mut()
        .start(&ui, move |stop, mut progress| {
            let cancel = stop.clone();
            let result = run(&c, &partial_input, &partial_run, &stop, move |p| {
                progress(p);
                cancel.cancel();
            });
            assert!(matches!(result, Err(Error::Cancelled)));
            report(partial_run)
        })
        .unwrap();
    wait(&ui, &state);
    assert!(ui.get_failed());
    assert!(ui.get_has_result());
    assert_eq!(inspect(&partial).unwrap().1.status, "cancelled");
    let stop = Cancellation::default();
    assert!(matches!(
        preview::play(&partial, None, f32::NAN, &stop, |_| {}),
        Err(Error::Invalid(_))
    ));
    stop.cancel();
    assert!(matches!(
        preview::play(&partial, None, 0.2, &stop, |_| {}),
        Err(Error::Cancelled)
    ));
    // Unknown observations remain unknown. Large histograms only collapse in the view.
    let value = serde_json::json!({"p99_ns":null,"bins":[1,2,3],"child":{"bins":[4],"calls":1}});
    let text = text_preview(&value).unwrap();
    assert!(!text.contains("bins"));
    assert!(text.contains("null"));
    assert!(value.get("bins").is_some());

    let finalized = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = finalized.clone();
    state
        .borrow_mut()
        .start(&ui, move |stop, _| {
            while !stop.is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            flag.store(true, Ordering::Release);
            Err(Error::Cancelled)
        })
        .unwrap();
    assert_eq!(
        state.borrow_mut().close(&ui),
        slint::CloseRequestResponse::KeepWindowShown
    );
    wait(&ui, &state);
    assert!(finalized.load(Ordering::Acquire));
    assert!(!ui.get_busy());
    println!("GUI artifacts: {}", root.display());
}
