//! Form values are projections of RunConfig, never another set of DSP defaults.
use super::ui::{Controls, Workbench};
use audiokit::resample::ResamplerQuality;
use audiokit::{AudioFormat, PacketDuration, StreamKind};
use audiokit_testkit::{Error, NoiseLevel, Result, RunConfig, Scenario};

pub(super) const SCENARIOS: [Scenario; 4] = [
    Scenario::FileProcessing,
    Scenario::FileRoundtrip,
    Scenario::MixStress,
    Scenario::ReceiveSimulation,
];

pub(super) fn put(ui: &Workbench, config: &RunConfig) -> Result<()> {
    let r = &config.receive.render;
    let m = r.master_limiter;
    ui.set_scenario(
        SCENARIOS
            .iter()
            .position(|s| *s == config.scenario)
            .unwrap() as i32,
    );
    ui.set_profile(i32::from(config.stream == StreamKind::Desktop));
    ui.set_profiling(config.execution_profiling);
    ui.set_retain_input(config.retain_input);
    ui.set_controls(Controls {
        processing: config.processing.enabled,
        noise: match config.processing.noise_suppression {
            NoiseLevel::Off => 0,
            NoiseLevel::Low => 1,
            NoiseLevel::Moderate => 2,
            NoiseLevel::High => 3,
            NoiseLevel::VeryHigh => 4,
        },
        high_pass: config.processing.high_pass_filter,
        agc: config.processing.gain_controller2,
        adaptive: config.processing.adaptive_gain,
        bitrate: config.bitrate_bps.to_string().into(),
        ptime: match config.ptime {
            PacketDuration::Ms10 => 0,
            PacketDuration::Ms20 => 1,
            PacketDuration::Ms40 => 2,
            PacketDuration::Ms60 => 3,
        },
        capture_quality: i32::from(config.resampler.quality == ResamplerQuality::HighQuality),
        render_quality: i32::from(r.resampler.quality == ResamplerQuality::HighQuality),
        output_rate: r.format.sample_rate_hz().to_string().into(),
        jitter: config.receive.jitter.target_ms.to_string().into(),
        sources: config.mix_stress.sources.to_string().into(),
        silent: config.mix_stress.silent_sources.to_string().into(),
        source_gain: config.mix_stress.source_gain.to_string().into(),
        admission: r.max_sources.to_string().into(),
        fade: r.gain.fade_ms.to_string().into(),
        source_ceiling: r.source_limiter.ceiling_dbfs.to_string().into(),
        master_ceiling: m.ceiling_dbfs.to_string().into(),
        lookahead: m.lookahead_ms.to_string().into(),
        attack: m.attack_ms.to_string().into(),
        release: m.release_ms.to_string().into(),
        headroom: m.reconstruction_headroom_db.to_string().into(),
    });
    ui.set_preset_json(serde_json::to_string_pretty(config)?.into());
    Ok(())
}
fn number<T: std::str::FromStr>(s: &str, label: &str) -> Result<T> {
    s.trim()
        .parse()
        .map_err(|_| Error::Invalid(format!("{label}: invalid numeric value")))
}
fn quality(index: i32) -> Result<ResamplerQuality> {
    match index {
        0 => Ok(ResamplerQuality::Balanced),
        1 => Ok(ResamplerQuality::HighQuality),
        _ => Err(Error::Invalid("unknown resampler quality".into())),
    }
}
/// Preserve advanced fields not represented by the form and edit only covered nodes.
pub(super) fn get(ui: &Workbench, base: &RunConfig) -> Result<RunConfig> {
    let c = ui.get_controls();
    let mut config = base.clone();
    if Some(&config.scenario) != SCENARIOS.get(ui.get_scenario() as usize)
        || config.stream
            != if ui.get_profile() == 0 {
                StreamKind::Voice
            } else {
                StreamKind::Desktop
            }
    {
        return Err(Error::Invalid("profile selection is not applied".into()));
    }
    if config.scenario != Scenario::ReceiveSimulation {
        config.resampler.quality = quality(c.capture_quality)?;
    }
    if matches!(
        config.scenario,
        Scenario::FileProcessing | Scenario::FileRoundtrip
    ) && config.stream == StreamKind::Voice
    {
        config.processing.enabled = c.processing;
        config.processing.noise_suppression = match c.noise {
            0 => NoiseLevel::Off,
            1 => NoiseLevel::Low,
            2 => NoiseLevel::Moderate,
            3 => NoiseLevel::High,
            4 => NoiseLevel::VeryHigh,
            _ => return Err(Error::Invalid("unknown suppression level".into())),
        };
        config.processing.high_pass_filter = c.high_pass;
        config.processing.gain_controller2 = c.agc;
        config.processing.adaptive_gain = c.adaptive && c.agc;
    }
    if config.scenario == Scenario::FileRoundtrip {
        config.bitrate_bps = number(&c.bitrate, "bitrate")?;
        config.ptime = match c.ptime {
            0 => PacketDuration::Ms10,
            1 => PacketDuration::Ms20,
            2 => PacketDuration::Ms40,
            3 => PacketDuration::Ms60,
            _ => return Err(Error::Invalid("unknown ptime".into())),
        };
    }
    if matches!(
        config.scenario,
        Scenario::FileRoundtrip | Scenario::ReceiveSimulation
    ) {
        config.receive.jitter.target_ms = number(&c.jitter, "jitter startup")?;
    }
    if config.scenario != Scenario::FileProcessing {
        let r = &mut config.receive.render;
        r.format = AudioFormat::new(number(&c.output_rate, "output rate")?, r.format.layout())?;
        r.resampler.quality = quality(c.render_quality)?;
        r.max_sources = number(&c.admission, "source admission")?;
        r.gain.fade_ms = number(&c.fade, "gain fade")?;
        r.source_limiter.ceiling_dbfs = number(&c.source_ceiling, "source ceiling")?;
        r.master_limiter.ceiling_dbfs = number(&c.master_ceiling, "master ceiling")?;
        r.master_limiter.lookahead_ms = number(&c.lookahead, "master lookahead")?;
        r.master_limiter.attack_ms = number(&c.attack, "master attack")?;
        r.master_limiter.release_ms = number(&c.release, "master release")?;
        r.master_limiter.reconstruction_headroom_db = number(&c.headroom, "master headroom")?;
    }
    if config.scenario == Scenario::MixStress {
        config.mix_stress.sources = number(&c.sources, "sources")?;
        config.mix_stress.silent_sources = number(&c.silent, "silent sources")?;
        config.mix_stress.source_gain = number(&c.source_gain, "source gain")?;
    }
    config.execution_profiling = ui.get_profiling();
    config.retain_input = ui.get_retain_input();
    config.validate()?;
    Ok(config)
}

pub(super) fn defaults(scenario: i32, profile: i32) -> Result<RunConfig> {
    let scenario = SCENARIOS
        .get(scenario as usize)
        .ok_or_else(|| Error::Invalid("unknown scenario".into()))?;
    let stream = match profile {
        0 => StreamKind::Voice,
        1 => StreamKind::Desktop,
        _ => return Err(Error::Invalid("unknown profile".into())),
    };
    Ok(RunConfig::for_profile(*scenario, stream))
}
