use std::path::Path;

use anyhow::{anyhow, Context};
use kikigaki_core::config::{AsrConfig, VadConfig};
use kikigaki_core::models::{ASR_MODEL_ID, VAD_MODEL_ID};
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, SileroVadModelConfig, VadModelConfig,
    VoiceActivityDetector,
};

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
    let path = models_dir.join(ASR_MODEL_ID);
    let mut sherpa_config = OfflineRecognizerConfig::default();
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

    let recognizer = OfflineRecognizer::create(&sherpa_config)
        .ok_or_else(|| anyhow!("failed to load {ASR_MODEL_ID} from {}", path.display()))?;
    Ok(SherpaAsr { recognizer })
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
