//! CLI and optional Slint frontend for the same shared AudioKit runner.
#[cfg(feature = "gui")]
mod gui;
#[cfg(any(feature = "gui", test))]
mod workbench;
use audiokit_testkit::{
    Cancellation, Error, RunConfig, Scenario, SweepMatrix, analyze, compare, plan_file, replay,
    run, sweep,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{Read, Write},
    path::PathBuf,
    time::Instant,
};

const HELP: &str = "AudioKit debugging CLI (initial offline suite)
  list-scenarios [--json]
  describe-scenario file-processing|file-roundtrip|mix-stress|receive-simulation [--json]
  validate [--config FILE] [--input WAV] [--scenario NAME] [--json]
  run --input WAV_OR_PACKET_JSON --out-dir NEW_DIR [--config FILE] [--scenario NAME] [--retain-input] [--quiet]
  analyze --bundle DIR [--json]
  replay --bundle DIR --out-dir NEW_DIR [--input ORIGINAL_WAV] [--quiet]
  compare --baseline DIR --candidate DIR [--json]
  sweep --input WAV --matrix FILE --out-dir NEW_DIR [--config FILE] [--retain-input] [--quiet]
  --gui [--config FILE] [--input WAV_OR_PACKET_JSON] [--out-dir BASE_DIR] (feature gui)
All results are JSON on stdout; progress is on stderr. Input retention is opt-in.
WAV PCM16/24/32 or float32 mono/stereo; receive-simulation takes packet JSON v1.
CLI runs never open devices or network. GUI audition is explicit; no hardware/server E2E yet.
Packet payload retention is opt-in audio-sharing consent.
Exit: 0 checks pass, 1 checks fail, 2 invalid arguments, 3 unavailable capability,
      4 execution/I/O failure, 130 cancellation.
";

struct Options {
    values: BTreeMap<String, OsString>,
}
impl Options {
    fn parse(args: impl Iterator<Item = OsString>, allowed: &[&str]) -> Result<Self, Error> {
        let mut values = BTreeMap::new();
        let mut args = args;
        while let Some(key) = args.next() {
            let key = key
                .into_string()
                .map_err(|_| Error::Invalid("flag is not UTF-8".into()))?;
            if !allowed.contains(&key.as_str()) {
                return Err(Error::Invalid(format!("unknown option {key}")));
            }
            let value = if matches!(key.as_str(), "--json" | "--quiet" | "--retain-input") {
                OsString::new()
            } else {
                args.next()
                    .ok_or_else(|| Error::Invalid(format!("missing value for {key}")))?
            };
            if values.insert(key.clone(), value).is_some() {
                return Err(Error::Invalid(format!("duplicate option {key}")));
            }
        }
        Ok(Self { values })
    }
    fn path(&self, key: &str) -> Option<PathBuf> {
        self.values.get(key).map(PathBuf::from)
    }
    fn required(&self, key: &str) -> Result<PathBuf, Error> {
        self.path(key)
            .ok_or_else(|| Error::Invalid(format!("required option {key}")))
    }
    fn flag(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
    fn config(&self) -> Result<RunConfig, Error> {
        let mut config: RunConfig = match self.path("--config") {
            Some(path) => audiokit_testkit::read_config(&path)?,
            None => RunConfig::default(),
        };
        if let Some(value) = self.values.get("--scenario") {
            let scenario: Scenario = serde_json::from_value(Value::String(
                value
                    .to_str()
                    .ok_or_else(|| Error::Invalid("scenario is not UTF-8".into()))?
                    .into(),
            ))?;
            if self.path("--config").is_none() {
                config = RunConfig::for_scenario(scenario);
            } else {
                config.scenario = scenario;
            }
        }
        if self.flag("--retain-input") {
            config.retain_input = true;
        }
        Ok(config)
    }
}
fn emit(value: &Value) -> Result<(), Error> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    Ok(())
}
fn stop_token() -> Result<Cancellation, Error> {
    let stop = Cancellation::default();
    let handler = stop.clone();
    ctrlc::set_handler(move || handler.cancel())
        .map_err(|e| Error::Invalid(format!("Ctrl+C handler: {e}")))?;
    Ok(stop)
}
fn progress(quiet: bool) -> impl FnMut(audiokit_testkit::ProgressEvent) {
    let mut last = Instant::now();
    move |event| {
        if !quiet && last.elapsed().as_millis() >= 250 {
            eprintln!(
                "processed {}/{} {}",
                event.input_frames,
                event.total_frames,
                match event.unit {
                    audiokit_testkit::ProgressUnit::InputFrames => "input frames",
                    audiokit_testkit::ProgressUnit::PacketRecords => "packet records",
                }
            );
            last = Instant::now();
        }
    }
}
fn execute() -> Result<(Value, i32), Error> {
    let mut args = std::env::args_os().skip(1);
    let command = args.next().unwrap_or_else(|| "--help".into());
    let command = command
        .to_str()
        .ok_or_else(|| Error::Invalid("command is not UTF-8".into()))?;
    match command {
        "--help" | "help" | "-h" => {
            print!("{HELP}");
            Ok((Value::Null, 0))
        }
        "--version" => Ok((
            json!({"name":"audiokit-test", "version":env!("CARGO_PKG_VERSION"), "revision":audiokit_testkit::BUILD_REVISION, "source_digest":audiokit_testkit::BUILD_SOURCE_DIGEST, "gui":cfg!(feature="gui")}),
            0,
        )),
        "list-scenarios" => {
            Options::parse(args, &["--json"])?;
            Ok((
                json!({"schema_version":1,"scenarios":["file-processing","file-roundtrip","mix-stress","receive-simulation"],"capabilities":{"codec_opus":cfg!(feature="codec-opus"),"processing_sonora":cfg!(feature="processing-sonora"),"virtual_faults":cfg!(feature="codec-opus"),"independent_clocks":cfg!(feature="codec-opus"),"packet_replay":cfg!(feature="codec-opus"),"mix_stress":true,"sweep":true,"packet_sweep":false,"devices":false,"gui":cfg!(feature="gui"),"gui_wav_preview":cfg!(feature="gui"),"server_e2e":false}}),
                0,
            ))
        }
        "describe-scenario" => {
            let name = args
                .next()
                .ok_or_else(|| Error::Invalid("missing scenario".into()))?;
            let scenario: Scenario = serde_json::from_value(json!(
                name.to_str()
                    .ok_or_else(|| Error::Invalid("scenario is not UTF-8".into()))?
            ))?;
            Options::parse(args, &["--json"])?;
            Ok((
                json!({"schema_version":1,"scenario":scenario,"default_config":RunConfig::for_scenario(scenario),"input":if scenario == Scenario::ReceiveSimulation {"packet recording JSON v1, single source/epoch"} else {"WAV mono/stereo"},"output":"float32 WAV + diagnostic bundle","coverage":match scenario { Scenario::FileProcessing=>"capture-subchain", Scenario::FileRoundtrip=>"virtual-roundtrip", Scenario::MixStress=>"render-stress", Scenario::ReceiveSimulation=>"receiver-replay" },"not_covered":["devices","server","AEC reference"],"selection":"scenario-defined production subchain; arbitrary endpoints not implemented"}),
                0,
            ))
        }
        "validate" => {
            let opts = Options::parse(args, &["--config", "--input", "--scenario", "--json"])?;
            let config = opts.config()?;
            config.validate()?;
            let plan = match opts.path("--input") {
                Some(path) => Some(plan_file(&config, &path)?),
                None => None,
            };
            Ok((
                json!({"schema_version":1,"valid":true,"effective_config":config,"plan":plan,"plan_reason":if plan.is_none() {Some("supply --input to resolve boundary formats")} else {None}}),
                0,
            ))
        }
        "run" => {
            let opts = Options::parse(
                args,
                &[
                    "--config",
                    "--input",
                    "--out-dir",
                    "--scenario",
                    "--retain-input",
                    "--quiet",
                    "--json",
                ],
            )?;
            let result = run(
                &opts.config()?,
                &opts.required("--input")?,
                &opts.required("--out-dir")?,
                &stop_token()?,
                progress(opts.flag("--quiet")),
            )?;
            let code = if result.checks.iter().all(|c| c.passed) {
                0
            } else {
                1
            };
            Ok((serde_json::to_value(result)?, code))
        }
        "analyze" => {
            let opts = Options::parse(args, &["--bundle", "--json"])?;
            let result = analyze(&opts.required("--bundle")?)?;
            let code = if result.recorded_checks_passed { 0 } else { 1 };
            Ok((serde_json::to_value(result)?, code))
        }
        "replay" => {
            let opts = Options::parse(
                args,
                &["--bundle", "--out-dir", "--input", "--quiet", "--json"],
            )?;
            let result = replay(
                &opts.required("--bundle")?,
                opts.path("--input").as_deref(),
                &opts.required("--out-dir")?,
                &stop_token()?,
                progress(opts.flag("--quiet")),
            )?;
            let code = if result.checks.iter().all(|c| c.passed) {
                0
            } else {
                1
            };
            Ok((serde_json::to_value(result)?, code))
        }
        "compare" => {
            let opts = Options::parse(args, &["--baseline", "--candidate", "--json"])?;
            Ok((
                serde_json::to_value(compare(
                    &opts.required("--baseline")?,
                    &opts.required("--candidate")?,
                )?)?,
                0,
            ))
        }
        "sweep" => {
            let opts = Options::parse(
                args,
                &[
                    "--config",
                    "--matrix",
                    "--input",
                    "--out-dir",
                    "--retain-input",
                    "--quiet",
                    "--json",
                ],
            )?;
            let file = std::fs::File::open(opts.required("--matrix")?)?;
            let mut data = Vec::new();
            file.take(1_048_577).read_to_end(&mut data)?;
            if data.len() > 1_048_576 {
                return Err(Error::Invalid("matrix exceeds 1 MiB".into()));
            }
            let matrix: SweepMatrix = serde_json::from_slice(&data)?;
            let mut on_progress = progress(opts.flag("--quiet"));
            let result = sweep(
                &opts.config()?,
                &matrix,
                &opts.required("--input")?,
                &opts.required("--out-dir")?,
                &stop_token()?,
                |_, event| on_progress(event),
            )?;
            let code = result.exit_code;
            Ok((serde_json::to_value(result)?, code))
        }
        "--gui" => {
            let opts = Options::parse(args, &["--config", "--input", "--out-dir"])?;
            #[cfg(feature = "gui")]
            {
                gui::launch(opts.config()?, opts.path("--input"), opts.path("--out-dir"))?;
                Ok((Value::Null, 0))
            }
            #[cfg(not(feature = "gui"))]
            {
                let _ = opts;
                Err(Error::Capability("rebuild with --features gui".into()))
            }
        }
        "devices" => Err(Error::Capability(format!(
            "{command} is planned but not implemented"
        ))),
        _ => Err(Error::Invalid("unknown command; use --help".into())),
    }
}
fn main() {
    let code = match execute() {
        Ok((value, code)) => {
            if value.is_null() {
                code
            } else if let Err(error) = emit(&value) {
                eprintln!("{error}");
                4
            } else {
                code
            }
        }
        Err(error) => {
            let code = error.exit_code();
            let _ = emit(&json!({"schema_version":1,"error":error.to_string(),"exit_code":code}));
            code
        }
    };
    std::process::exit(code);
}
