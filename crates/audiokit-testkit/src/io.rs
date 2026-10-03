//! Bounded WAV/JSON I/O; this initial runner is offline, never a device callback.
use crate::{Error, Result};
use audiokit::{AudioFormat, ChannelLayout};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Cursor, Read, Write},
    path::Path,
};

pub(crate) const JSON_LIMIT: u64 = 32 * 1024 * 1024;
pub(crate) fn bytes(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    if file.metadata()?.len() > max {
        return Err(Error::Invalid("file exceeds byte budget".into()));
    }
    let mut data = Vec::new();
    file.take(max + 1).read_to_end(&mut data)?;
    if data.len() as u64 > max {
        return Err(Error::Invalid("file grew beyond byte budget".into()));
    }
    Ok(data)
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&bytes(path, JSON_LIMIT)?)?)
}
pub(crate) fn write_new(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(data)?;
    file.flush()?;
    Ok(())
}
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let data = serde_json::to_vec_pretty(value)?;
    if data.len() as u64 > JSON_LIMIT {
        return Err(Error::Execution(
            "JSON artifact exceeds hard byte limit".into(),
        ));
    }
    write_new(path, &data)
}

pub(crate) fn wav(data: &[u8], max_samples: usize) -> Result<(AudioFormat, Vec<f32>)> {
    let mut reader = hound::WavReader::new(Cursor::new(data))?;
    let spec = reader.spec();
    let layout = match spec.channels {
        1 => ChannelLayout::Mono,
        2 => ChannelLayout::Stereo,
        _ => return Err(Error::Invalid("WAV must be mono or stereo".into())),
    };
    let format = AudioFormat::new(spec.sample_rate, layout)?;
    if reader.len() as usize > max_samples {
        return Err(Error::Invalid("decoded sample budget exceeded".into()));
    }
    let mut pcm = Vec::with_capacity(reader.len() as usize);
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => {
            for sample in reader.samples::<f32>() {
                pcm.push(sample?);
            }
        }
        (hound::SampleFormat::Int, bits @ (16 | 24 | 32)) => {
            let divisor = 2.0_f64.powi(i32::from(bits) - 1);
            for sample in reader.samples::<i32>() {
                pcm.push((f64::from(sample?) / divisor) as f32);
            }
        }
        _ => {
            return Err(Error::Invalid(
                "supported WAV formats: float32, PCM16/24/32".into(),
            ));
        }
    }
    format.frames_in(pcm.len())?;
    if pcm.len() > max_samples
        || pcm.is_empty()
        || !pcm.iter().all(|v| v.is_finite() && v.abs() <= 64.0)
    {
        return Err(Error::Invalid(
            "empty, oversized, non-finite or excessive WAV amplitude".into(),
        ));
    }
    Ok((format, pcm))
}
pub(crate) fn write_wav(path: &Path, format: AudioFormat, pcm: &[f32]) -> Result<()> {
    format.frames_in(pcm.len())?;
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut writer = hound::WavWriter::new(
        std::io::BufWriter::new(file),
        hound::WavSpec {
            channels: u16::from(format.channels()),
            sample_rate: format.sample_rate_hz(),
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )?;
    for &sample in pcm {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok(())
}
