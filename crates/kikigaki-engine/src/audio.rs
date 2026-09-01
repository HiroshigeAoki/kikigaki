//! WAV loading helpers shared by integration tests and evaluation tools.

use std::path::Path;

use anyhow::{ensure, Context};
use kikigaki_core::audio::StreamResampler;
use sherpa_onnx::Wave;

/// Loads the first channel of a WAV file and resamples it to 16 kHz.
///
/// The loader accepts the sample rates used by the pinned model fixtures and
/// rejects empty audio or any decoded sample that is not finite.
pub fn load_wav_16k(path: &Path) -> anyhow::Result<Vec<f32>> {
    let wave = Wave::read(&path.to_string_lossy())
        .with_context(|| format!("read WAV {}", path.display()))?;
    let sample_rate = u32::try_from(wave.sample_rate())
        .with_context(|| format!("negative WAV sample rate in {}", path.display()))?;
    ensure!(
        matches!(sample_rate, 16_000 | 44_100 | 48_000),
        "unexpected sample rate {sample_rate} in {}",
        path.display()
    );
    validate_samples(wave.samples())
        .with_context(|| format!("validate decoded WAV samples in {}", path.display()))?;

    let mut resampler = StreamResampler::new(sample_rate, 16_000);
    let samples = resampler.push(wave.samples());
    validate_samples(&samples)
        .with_context(|| format!("validate resampled WAV samples in {}", path.display()))?;
    Ok(samples)
}

fn validate_samples(samples: &[f32]) -> anyhow::Result<()> {
    ensure!(!samples.is_empty(), "decoded samples are empty");
    ensure!(
        samples.iter().all(|sample| sample.is_finite()),
        "decoded samples contain a non-finite value"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_samples;

    #[test]
    fn rejects_empty_samples() {
        let error = validate_samples(&[]).unwrap_err();
        assert!(error.to_string().contains("empty"));
    }

    #[test]
    fn rejects_non_finite_samples() {
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let error = validate_samples(&[0.0, sample]).unwrap_err();
            assert!(error.to_string().contains("non-finite"));
        }
    }

    #[test]
    fn accepts_finite_samples() {
        validate_samples(&[-1.0, 0.0, 1.0]).unwrap();
    }
}
