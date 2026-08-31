use std::path::PathBuf;

use kikigaki_core::audio::StreamResampler;
use sherpa_onnx::Wave;

/// Reads a fixture WAV (first channel) at its native rate.
pub fn load_wav_raw(name: &str) -> (Vec<f32>, u32) {
    let path = fixture_path(name);
    let wave = Wave::read(&path.to_string_lossy())
        .unwrap_or_else(|| panic!("failed to read WAV {}", path.display()));
    let sample_rate = u32::try_from(wave.sample_rate()).expect("WAV sample rate is negative");
    (wave.samples().to_vec(), sample_rate)
}

fn fixture_path(name: &str) -> PathBuf {
    let models_dir = PathBuf::from(
        std::env::var_os("KIKIGAKI_MODELS_DIR")
            .expect("KIKIGAKI_MODELS_DIR must be set by the real-model test"),
    );
    std::env::var_os("KIKIGAKI_TEST_WAVS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            models_dir
                .join("reazonspeech-ja-en-2025-01-17")
                .join("test_wavs")
        })
        .join(name)
}

pub fn load_wav_16k(name: &str) -> Vec<f32> {
    let path = fixture_path(name);
    let wave = Wave::read(&path.to_string_lossy())
        .unwrap_or_else(|| panic!("failed to read WAV {}", path.display()));
    let sample_rate = u32::try_from(wave.sample_rate()).expect("WAV sample rate is negative");
    // The pinned upstream archive's stereo test_ja_2 fixture is 44.1 kHz; Wave exposes its
    // first channel. The other bundled fixtures used here are 48 kHz (or may be preconverted).
    assert!(
        matches!(sample_rate, 16_000 | 44_100 | 48_000),
        "unexpected sample rate {sample_rate} in {}",
        path.display()
    );
    let mut resampler = StreamResampler::new(sample_rate, 16_000);
    resampler.push(wave.samples())
}
