use std::path::PathBuf;

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
    kikigaki_engine::audio::load_wav_16k(&path)
        .unwrap_or_else(|error| panic!("failed to load WAV {}: {error:#}", path.display()))
}

/// Returns the installed Haas directory, with the same visible skip as other model fixtures.
pub fn haas_models_dir() -> Option<PathBuf> {
    let Some(root) = std::env::var_os("KIKIGAKI_MODELS_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED: models missing (KIKIGAKI_MODELS_DIR unset)");
        return None;
    };
    let model = root.join(kikigaki_core::models::HAAS_MODEL_ID);
    for name in [
        "encoder.quantized.onnx",
        "encoder.data",
        "decoder.onnx",
        "joiner.onnx",
        "tokens.txt",
    ] {
        if !model.join(name).is_file() {
            eprintln!(
                "SKIPPED: Haas model file missing ({})",
                model.join(name).display()
            );
            return None;
        }
    }
    Some(root)
}
