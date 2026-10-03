//! CLI protocol and reproduction smoke tests, without devices or network.
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "audiokit-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn input(&self) -> PathBuf {
        let path = self.path("input.wav");
        let mut writer = hound::WavWriter::create(
            &path,
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
        path
    }
    fn config(&self) -> PathBuf {
        let path = self.path("config.json");
        fs::write(
            &path,
            br#"{"processing":{"enabled":false},"retain_input":true}"#,
        )
        .unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn execute(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_audiokit-test"))
        .args(args)
        .output()
        .unwrap()
}
fn value(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn command_inventory_is_machine_readable_and_honest_about_gui_devices() {
    let output = execute(&["list-scenarios", "--json"]);
    assert!(output.status.success());
    assert_eq!(value(&output)["scenarios"].as_array().unwrap().len(), 2);
    assert_eq!(value(&output)["capabilities"]["gui"], false);
    assert_eq!(value(&output)["capabilities"]["devices"], false);
    assert_eq!(value(&output)["capabilities"]["sweep"], true);
    for cmd in ["--gui", "devices"] {
        let output = execute(&[cmd]);
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(value(&output)["exit_code"], 3);
    }
}

#[test]
fn headless_sweep_produces_json_summary_and_individual_replayable_bundles() {
    let fixture = Fixture::new();
    let source = fixture.input();
    let config = fixture.config();
    let matrix = fixture.path("matrix.json");
    fs::write(&matrix, b"{}").unwrap();
    let directory = fixture.path("sweep");
    let output = execute(&[
        "sweep",
        "--input",
        source.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--matrix",
        matrix.to_str().unwrap(),
        "--out-dir",
        directory.to_str().unwrap(),
        "--quiet",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(value(&output)["planned_cases"], 1);
    assert!(directory.join("sweep.json").is_file());
    let output = execute(&[
        "analyze",
        "--bundle",
        directory.join("case-000").to_str().unwrap(),
    ]);
    assert!(output.status.success());
    fs::write(&matrix, br#"{"max_cases":0}"#).unwrap();
    let output = execute(&[
        "sweep",
        "--input",
        source.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--matrix",
        matrix.to_str().unwrap(),
        "--out-dir",
        fixture.path("bad-sweep").to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(!fixture.path("bad-sweep").exists());
}
#[test]
fn malformed_options_and_unknown_configs_use_exit_two() {
    for args in [
        &["run", "--wat"][..],
        &["analyze"][..],
        &["sweep"][..],
        &["list-scenarios", "--json", "--json"][..],
        &["describe-scenario", "bad"][..],
    ] {
        let output = execute(args);
        assert_eq!(output.status.code(), Some(2));
        value(&output);
    }
    let fixture = Fixture::new();
    let path = fixture.path("bad.json");
    fs::write(&path, br#"{"unknown":true}"#).unwrap();
    let output = execute(&["validate", "--config", path.to_str().unwrap(), "--json"]);
    assert_eq!(output.status.code(), Some(2));
}
#[test]
fn cli_run_analyze_replay_compare_is_a_complete_headless_loop() {
    let fixture = Fixture::new();
    let input = fixture.input();
    let config = fixture.config();
    let first = fixture.path("first");
    let second = fixture.path("second");
    let output = execute(&[
        "validate",
        "--config",
        config.to_str().unwrap(),
        "--input",
        input.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(value(&output)["plan"]["coverage"], "capture-subchain");
    let output = execute(&[
        "run",
        "--config",
        config.to_str().unwrap(),
        "--input",
        input.to_str().unwrap(),
        "--out-dir",
        first.to_str().unwrap(),
        "--quiet",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(value(&output)["output_frames"], 1001);
    assert!(
        execute(&["analyze", "--bundle", first.to_str().unwrap(), "--json"])
            .status
            .success()
    );
    let output = execute(&[
        "replay",
        "--bundle",
        first.to_str().unwrap(),
        "--out-dir",
        second.to_str().unwrap(),
        "--quiet",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let output = execute(&[
        "compare",
        "--baseline",
        first.to_str().unwrap(),
        "--candidate",
        second.to_str().unwrap(),
        "--json",
    ]);
    assert!(output.status.success());
    assert_eq!(value(&output)["same_output_bytes"], true);
    let original = fs::read(first.join("processed.wav")).unwrap();
    assert_eq!(
        execute(&[
            "replay",
            "--bundle",
            first.to_str().unwrap(),
            "--out-dir",
            first.to_str().unwrap(),
            "--quiet"
        ])
        .status
        .code(),
        Some(4)
    );
    assert_eq!(original, fs::read(first.join("processed.wav")).unwrap());
}
#[test]
fn no_arguments_provides_help_without_opening_any_surface() {
    let output = execute(&[]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("replay --bundle"));
}
