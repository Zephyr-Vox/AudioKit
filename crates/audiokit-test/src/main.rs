//! Headless frontend for the shared AudioKit runner; GUI and devices are not implemented yet.
use audiokit_testkit::{
    Cancellation, Error, RunConfig, Scenario, analyze, compare, plan_file, replay, run,
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
  describe-scenario file-processing|file-roundtrip [--json]
  validate [--config FILE] [--input WAV] [--scenario NAME] [--json]
  run --input WAV --out-dir NEW_DIR [--config FILE] [--scenario NAME] [--retain-input] [--quiet]
  analyze --bundle DIR [--json]
  replay --bundle DIR --out-dir NEW_DIR [--input ORIGINAL_WAV] [--quiet]
  compare --baseline DIR --candidate DIR [--json]
All results are JSON on stdout; progress is on stderr. Input retention is opt-in.
Only WAV PCM16/24/32 and float32 mono/stereo are supported. No devices/network/GUI.
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
            Some(path) => {
                let file = std::fs::File::open(path)?;
                let mut data = Vec::new();
                file.take(1_048_577).read_to_end(&mut data)?;
                if data.len() > 1_048_576 {
                    return Err(Error::Invalid("configuration exceeds 1 MiB".into()));
                }
                serde_json::from_slice(&data)?
            }
            None => RunConfig::default(),
        };
        if let Some(value) = self.values.get("--scenario") {
            config.scenario = serde_json::from_value(Value::String(
                value
                    .to_str()
                    .ok_or_else(|| Error::Invalid("scenario is not UTF-8".into()))?
                    .into(),
            ))?;
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
                "processed {}/{} input frames",
                event.input_frames, event.total_frames
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
            json!({"name":"audiokit-test", "version":env!("CARGO_PKG_VERSION"), "revision":audiokit_testkit::BUILD_REVISION, "source_digest":audiokit_testkit::BUILD_SOURCE_DIGEST, "gui":false}),
            0,
        )),
        "list-scenarios" => {
            Options::parse(args, &["--json"])?;
            Ok((
                json!({"schema_version":1,"scenarios":["file-processing","file-roundtrip"],"capabilities":{"codec_opus":cfg!(feature="codec-opus"),"processing_sonora":cfg!(feature="processing-sonora"),"devices":false,"gui":false,"server_e2e":false}}),
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
                json!({"schema_version":1,"scenario":scenario,"default_config":RunConfig { scenario, ..Default::default() },"input":"WAV mono/stereo","output":"float32 WAV + diagnostic bundle","coverage":if scenario == Scenario::FileProcessing {"capture-subchain"} else {"virtual-roundtrip"},"not_covered":["devices","server","AEC reference"],"selection":"scenario-defined production subchain; arbitrary endpoints not implemented"}),
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
        "--gui" | "devices" | "sweep" => Err(Error::Capability(format!(
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
