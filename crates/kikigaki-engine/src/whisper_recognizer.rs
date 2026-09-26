//! whisper-rs-backed [`Recognizer`] implementation.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use crate::local::Recognizer;
use crate::whisper_engine::DecodeCancel;
use crate::whisper_guard::audio_ctx_for;

/// Loads the pinned Whisper model from its managed model directory.
pub fn build_whisper_recognizer(
    models_dir: &Path,
    use_gpu: bool,
    shutdown: Arc<AtomicBool>,
    decode_cancel: DecodeCancel,
) -> anyhow::Result<WhisperRecognizer> {
    let path =
        kikigaki_core::models::model_dir(models_dir, kikigaki_core::models::WHISPER_MODEL_ID)
            .join("ggml-large-v3-turbo-q8_0.bin");
    WhisperRecognizer::load(&path, use_gpu, shutdown, decode_cancel)
}

/// A reusable whisper.cpp context and decode state.
pub struct WhisperRecognizer {
    context: WhisperContext,
    state: Option<WhisperState>,
    shutdown: Arc<AtomicBool>,
    decode_cancel: DecodeCancel,
}

impl WhisperRecognizer {
    /// Loads a Whisper model and allocates its reusable decode state.
    pub fn load(
        model_path: &Path,
        use_gpu: bool,
        shutdown: Arc<AtomicBool>,
        decode_cancel: DecodeCancel,
    ) -> anyhow::Result<Self> {
        let mut ctx_params = WhisperContextParameters::new();
        ctx_params.use_gpu(use_gpu);
        let context = WhisperContext::new_with_params(model_path, ctx_params)
            .with_context(|| format!("load whisper model {}", model_path.display()))?;
        let state = context.create_state().with_context(|| {
            format!(
                "create whisper decode state for model {}",
                model_path.display()
            )
        })?;

        Ok(Self {
            context,
            state: Some(state),
            shutdown,
            decode_cancel,
        })
    }

    fn ensure_state(&mut self) -> Option<&mut WhisperState> {
        if self.state.is_none() {
            match self.context.create_state() {
                Ok(state) => self.state = Some(state),
                Err(error) => {
                    tracing::error!(%error, "failed to recreate whisper decode state");
                    return None;
                }
            }
        }
        self.state.as_mut()
    }
}

impl Recognizer for WhisperRecognizer {
    fn transcribe(&mut self, samples: &[f32]) -> Option<String> {
        self.ensure_state()?;

        let duration_ms = (samples.len() as u64 * 1_000) / 16_000;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("ja"));
        params.set_no_context(true);
        if let Some(audio_ctx) = audio_ctx_for(duration_ms) {
            params.set_audio_ctx(audio_ctx);
        }
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        params.set_single_segment(false);
        let shutdown = Arc::clone(&self.shutdown);
        let decode_cancel = self.decode_cancel.clone();
        params.set_abort_callback_safe(move || {
            shutdown.load(Ordering::Relaxed) || decode_cancel.should_abort()
        });

        if let Err(error) = self.state.as_mut()?.full(params, samples) {
            let aborted =
                self.shutdown.load(Ordering::Relaxed) || self.decode_cancel.should_abort();
            if !aborted {
                tracing::error!(%error, "whisper decode failed (not an abort)");
                self.state = None;
            }
            return None;
        }

        let state = self.state.as_ref()?;
        let mut text = String::new();
        for index in 0..state.full_n_segments() {
            text.push_str(state.get_segment(index)?.to_str().ok()?);
        }
        Some(text)
    }
}
