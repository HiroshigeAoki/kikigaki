//! whisper-rs-backed [`Recognizer`] implementation.

use std::ffi::c_void;
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

struct AbortState {
    shutdown: Arc<AtomicBool>,
    decode_cancel: DecodeCancel,
}

unsafe extern "C" fn whisper_abort(user_data: *mut c_void) -> bool {
    // SAFETY: The caller guarantees that `user_data` points to a live `AbortState`.
    let abort_state = unsafe { &*user_data.cast::<AbortState>() };
    abort_state.shutdown.load(Ordering::Relaxed) || abort_state.decode_cancel.should_abort()
}

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
    abort_state: Box<AbortState>,
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
            abort_state: Box::new(AbortState {
                shutdown,
                decode_cancel,
            }),
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
        let abort_state = std::ptr::from_ref(self.abort_state.as_ref())
            .cast_mut()
            .cast::<c_void>();
        // SAFETY: `abort_state` is owned by `self`, has a stable heap address, and outlives the
        // synchronous `state.full(params, samples)` call. The callback only reads atomics.
        unsafe {
            params.set_abort_callback(Some(whisper_abort));
            params.set_abort_callback_user_data(abort_state);
        }

        if let Err(error) = self.state.as_mut()?.full(params, samples) {
            let aborted = self.abort_state.shutdown.load(Ordering::Relaxed)
                || self.abort_state.decode_cancel.should_abort();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_callback_reflects_shutdown_and_decode_cancellation() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let decode_cancel = DecodeCancel::new();
        let abort_state = AbortState {
            shutdown: Arc::clone(&shutdown),
            decode_cancel: decode_cancel.clone(),
        };
        let user_data = std::ptr::from_ref(&abort_state).cast_mut().cast();

        // SAFETY: `user_data` points to `abort_state`, which remains alive for every call.
        unsafe {
            assert!(!whisper_abort(user_data));

            shutdown.store(true, Ordering::Relaxed);
            assert!(whisper_abort(user_data));

            shutdown.store(false, Ordering::Relaxed);
            decode_cancel.begin_for_test(7);
            decode_cancel.cancel(7);
            assert!(whisper_abort(user_data));
        }
    }
}
