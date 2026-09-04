use std::path::Path;

use anyhow::{anyhow, ensure, Context};
use kikigaki_core::config::{AsrConfig, DecodingMethod, VadConfig};
use kikigaki_core::models::{ASR_MODEL_ID, VAD_MODEL_ID};
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, SileroVadModelConfig, VadModelConfig,
    VoiceActivityDetector,
};

use crate::hotwords::materialize;
use crate::local::{Recognizer, Vad, VadSegment};

/// A loaded sherpa-onnx voice activity detector.
pub struct SherpaVad {
    detector: VoiceActivityDetector,
}

/// A loaded sherpa-onnx offline speech recognizer.
pub struct SherpaAsr {
    recognizer: OfflineRecognizer,
}

/// Loads the configured Silero voice activity detector from `models_dir`.
pub fn build_vad(models_dir: &Path, config: &VadConfig) -> anyhow::Result<SherpaVad> {
    let path = models_dir.join(VAD_MODEL_ID).join("silero_vad.onnx");
    let sherpa_config = VadModelConfig {
        silero_vad: SileroVadModelConfig {
            model: Some(path.to_string_lossy().into_owned()),
            threshold: config.threshold,
            min_silence_duration: config.min_silence_ms as f32 / 1_000.0,
            min_speech_duration: config.min_speech_ms as f32 / 1_000.0,
            window_size: 512,
            max_speech_duration: config.max_speech_s,
        },
        ten_vad: Default::default(),
        sample_rate: 16_000,
        num_threads: 1,
        provider: Some("cpu".into()),
        debug: false,
    };
    let detector = VoiceActivityDetector::create(&sherpa_config, 30.0)
        .ok_or_else(|| anyhow!("failed to load {VAD_MODEL_ID} from {}", path.display()))?;
    Ok(SherpaVad { detector })
}

/// Loads the configured ReazonSpeech transducer from `models_dir`.
pub fn build_recognizer(models_dir: &Path, config: &AsrConfig) -> anyhow::Result<SherpaAsr> {
    let sherpa_config = recognizer_config(models_dir, config, None)?;
    let path = models_dir.join(ASR_MODEL_ID);
    let recognizer = OfflineRecognizer::create(&sherpa_config)
        .ok_or_else(|| anyhow!("failed to load {ASR_MODEL_ID} from {}", path.display()))?;
    Ok(SherpaAsr { recognizer })
}

/// Loads the configured recognizer, optionally enabling embedded hotword biasing.
///
/// Hotword materialization and hotword-specific recognizer failures degrade to the baseline
/// recognizer. Invalid scores remain configuration errors and are rejected before the filesystem
/// is accessed.
pub fn build_recognizer_auto(
    models_dir: &Path,
    config: &AsrConfig,
    hotwords: Option<f32>,
) -> anyhow::Result<SherpaAsr> {
    crate::local::with_hotword_fallback(
        hotwords,
        || materialize(models_dir),
        || build_recognizer(models_dir, config),
        |setup, score| {
            build_recognizer_with_hotwords(
                models_dir,
                config,
                &setup.hotwords_file,
                score,
                &setup.bpe_vocab,
            )
        },
    )
}

/// Loads the configured ReazonSpeech transducer with hotword biasing enabled.
///
/// `bpe_vocab` supplies the sentencepiece-style vocabulary required to encode
/// hotwords for the model's byte-level BPE tokens.
pub fn build_recognizer_with_hotwords(
    models_dir: &Path,
    config: &AsrConfig,
    hotwords_file: &Path,
    score: f32,
    bpe_vocab: &Path,
) -> anyhow::Result<SherpaAsr> {
    ensure!(
        config.decoding_method == DecodingMethod::ModifiedBeamSearch,
        "hotwords require the modified_beam_search decoding method"
    );
    ensure!(
        score.is_finite() && score > 0.0,
        "hotwords score must be finite and greater than zero, got {score}"
    );
    ensure!(
        hotwords_file.exists(),
        "hotwords file does not exist: {}",
        hotwords_file.display()
    );
    ensure!(
        bpe_vocab.exists(),
        "BPE vocabulary file does not exist: {}",
        bpe_vocab.display()
    );

    let sherpa_config =
        recognizer_config(models_dir, config, Some((hotwords_file, score, bpe_vocab)))?;
    let path = models_dir.join(ASR_MODEL_ID);
    let recognizer = OfflineRecognizer::create(&sherpa_config)
        .ok_or_else(|| anyhow!("failed to load {ASR_MODEL_ID} from {}", path.display()))?;
    Ok(SherpaAsr { recognizer })
}

pub(crate) fn recognizer_config(
    models_dir: &Path,
    config: &AsrConfig,
    hotwords: Option<(&Path, f32, &Path)>,
) -> anyhow::Result<OfflineRecognizerConfig> {
    let mut sherpa_config = OfflineRecognizerConfig::default();
    let path = models_dir.join(ASR_MODEL_ID);
    sherpa_config.feat_config.sample_rate = 16_000;
    sherpa_config.feat_config.feature_dim = 80;
    sherpa_config.model_config.transducer.encoder = Some(
        path.join("encoder-epoch-35-avg-1.int8.onnx")
            .to_string_lossy()
            .into_owned(),
    );
    sherpa_config.model_config.transducer.decoder = Some(
        path.join("decoder-epoch-35-avg-1.int8.onnx")
            .to_string_lossy()
            .into_owned(),
    );
    sherpa_config.model_config.transducer.joiner = Some(
        path.join("joiner-epoch-35-avg-1.int8.onnx")
            .to_string_lossy()
            .into_owned(),
    );
    sherpa_config.model_config.tokens =
        Some(path.join("tokens.txt").to_string_lossy().into_owned());
    sherpa_config.model_config.num_threads =
        i32::try_from(config.num_threads).context("ASR thread count exceeds sherpa-onnx range")?;
    sherpa_config.model_config.provider = Some("cpu".into());
    sherpa_config.model_config.model_type = Some("zipformer".into());
    sherpa_config.model_config.modeling_unit = Some("cjkchar".into());
    sherpa_config.decoding_method = Some(config.decoding_method.as_sherpa_str().into());
    sherpa_config.max_active_paths = 4;
    if let Some((hotwords_file, score, bpe_vocab)) = hotwords {
        sherpa_config.model_config.modeling_unit = Some("bbpe".into());
        sherpa_config.model_config.bpe_vocab = Some(bpe_vocab.to_string_lossy().into_owned());
        sherpa_config.hotwords_file = Some(hotwords_file.to_string_lossy().into_owned());
        sherpa_config.hotwords_score = score;
    }

    Ok(sherpa_config)
}

/// Decodes one 16 kHz mono segment with a loaded recognizer.
fn decode(recognizer: &OfflineRecognizer, samples: &[f32]) -> Option<String> {
    let stream = recognizer.create_stream();
    stream.accept_waveform(16_000, samples);
    recognizer.decode(&stream);
    stream.get_result().map(|result| result.text)
}

impl Vad for SherpaVad {
    fn accept(&mut self, frame: &[f32]) {
        self.detector.accept_waveform(frame);
    }

    fn flush(&mut self) {
        self.detector.flush();
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        self.detector.reset();
        Ok(())
    }

    fn drain(&mut self) -> anyhow::Result<Vec<VadSegment>> {
        let mut segments = Vec::new();
        while !self.detector.is_empty() {
            let segment = self.detector.front().context("vad front")?;
            let start = usize::try_from(segment.start()).context("negative VAD segment start")?;
            let samples = segment.samples().to_vec();
            drop(segment);
            self.detector.pop();
            segments.push(VadSegment { start, samples });
        }
        Ok(segments)
    }
}

impl Recognizer for SherpaAsr {
    fn transcribe(&mut self, samples: &[f32]) -> Option<String> {
        decode(&self.recognizer, samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_baseline_configs_match(
        actual: &OfflineRecognizerConfig,
        expected: &OfflineRecognizerConfig,
    ) {
        assert_eq!(format!("{actual:#?}"), format!("{expected:#?}"));
    }

    #[test]
    fn recognizer_auto_without_hotwords_preserves_baseline_config() {
        let models_dir = Path::new("/models");
        let config = AsrConfig::default();
        let expected = recognizer_config(models_dir, &config, None).unwrap();

        let actual = crate::local::with_hotword_fallback(
            None,
            || panic!("disabled hotwords must not be materialized"),
            || recognizer_config(models_dir, &config, None),
            |setup, score| {
                recognizer_config(
                    models_dir,
                    &config,
                    Some((&setup.hotwords_file, score, &setup.bpe_vocab)),
                )
            },
        )
        .unwrap();

        assert_baseline_configs_match(&actual, &expected);
    }

    #[test]
    fn recognizer_auto_rejects_invalid_scores_before_materializing() {
        for score in [f32::NEG_INFINITY, -1.0, 0.0, f32::INFINITY, f32::NAN] {
            let error = build_recognizer_auto(
                Path::new("/models-that-must-not-be-read"),
                &AsrConfig::default(),
                Some(score),
            )
            .err()
            .expect("invalid score must fail");

            assert!(
                error.to_string().contains("finite and greater than zero"),
                "unexpected error for {score:?}: {error:#}"
            );
        }
    }

    #[test]
    fn recognizer_config_preserves_existing_defaults() {
        let models_dir = Path::new("/models");
        let config = AsrConfig::default();

        let actual = recognizer_config(models_dir, &config, None).unwrap();
        let model_dir = models_dir.join(ASR_MODEL_ID);

        assert_eq!(actual.feat_config.sample_rate, 16_000);
        assert_eq!(actual.feat_config.feature_dim, 80);
        assert_eq!(
            actual.model_config.transducer.encoder.as_deref(),
            Some(
                model_dir
                    .join("encoder-epoch-35-avg-1.int8.onnx")
                    .to_str()
                    .unwrap()
            )
        );
        assert_eq!(
            actual.model_config.transducer.decoder.as_deref(),
            Some(
                model_dir
                    .join("decoder-epoch-35-avg-1.int8.onnx")
                    .to_str()
                    .unwrap()
            )
        );
        assert_eq!(
            actual.model_config.transducer.joiner.as_deref(),
            Some(
                model_dir
                    .join("joiner-epoch-35-avg-1.int8.onnx")
                    .to_str()
                    .unwrap()
            )
        );
        assert_eq!(
            actual.model_config.tokens.as_deref(),
            Some(model_dir.join("tokens.txt").to_str().unwrap())
        );
        assert_eq!(actual.model_config.num_threads, 4);
        assert_eq!(actual.model_config.provider.as_deref(), Some("cpu"));
        assert_eq!(actual.model_config.model_type.as_deref(), Some("zipformer"));
        assert_eq!(
            actual.model_config.modeling_unit.as_deref(),
            Some("cjkchar")
        );
        assert_eq!(actual.model_config.bpe_vocab, None);
        assert_eq!(
            actual.decoding_method.as_deref(),
            Some("modified_beam_search")
        );
        assert_eq!(actual.max_active_paths, 4);
        assert_eq!(actual.hotwords_file, None);
        assert_eq!(actual.hotwords_score, 0.0);
    }

    #[test]
    fn recognizer_config_sets_hotwords() {
        let hotwords_file = Path::new("/hotwords.txt");
        let bpe_vocab = Path::new("/bpe.vocab");

        let actual = recognizer_config(
            Path::new("/models"),
            &AsrConfig::default(),
            Some((hotwords_file, 2.0, bpe_vocab)),
        )
        .unwrap();

        assert_eq!(actual.model_config.modeling_unit.as_deref(), Some("bbpe"));
        assert_eq!(
            actual.model_config.bpe_vocab.as_deref(),
            Some(bpe_vocab.to_str().unwrap())
        );
        assert_eq!(
            actual.hotwords_file.as_deref(),
            Some(hotwords_file.to_str().unwrap())
        );
        assert_eq!(actual.hotwords_score, 2.0);
    }

    #[test]
    fn recognizer_config_rejects_thread_count_outside_sherpa_range() {
        let config = AsrConfig {
            num_threads: u32::MAX,
            ..AsrConfig::default()
        };

        let error = recognizer_config(Path::new("/models"), &config, None).unwrap_err();

        assert_eq!(
            error.to_string(),
            "ASR thread count exceeds sherpa-onnx range"
        );
    }
}
