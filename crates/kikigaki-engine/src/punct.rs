use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use kikigaki_core::postprocess::Punctuator;
use kikigaki_core::punct::{
    apply_question_marks, encode, nfkc, punctuate_windowed, Thresholds, Vocab,
};
use ort::session::Session;
use ort::value::Tensor;

/// Mojicast character-level Japanese punctuator backed by ONNX Runtime.
pub struct MojicastPunctuator {
    session: Session,
    vocab: Vocab,
    thresholds: Thresholds,
}

impl MojicastPunctuator {
    /// Loads ONNX Runtime, the INT8 punctuation model, and its BERT vocabulary.
    pub fn new(
        models_dir: PathBuf,
        thresholds: Thresholds,
        num_threads: usize,
    ) -> anyhow::Result<Self> {
        ensure!(num_threads > 0, "punctuation thread count must be positive");
        let runtime_path = runtime_path(&models_dir);
        ort::init_from(&runtime_path)
            .with_context(|| format!("load ONNX Runtime from {}", runtime_path.display()))?
            .commit();

        let punct_dir = models_dir.join("mojicast-punct");
        let model_path = punct_dir.join("punct_bert.int8.onnx");
        let session = Session::builder()
            .context("create punctuation session builder")?
            .with_intra_threads(num_threads)
            .map_err(|error| -> ort::Error { error.into() })
            .context("configure punctuation inference threads")?
            .commit_from_file(&model_path)
            .with_context(|| format!("load punctuation model from {}", model_path.display()))?;
        let vocab_path = punct_dir.join("vocab.txt");
        let vocab_text = fs::read_to_string(&vocab_path)
            .with_context(|| format!("read punctuation vocabulary {}", vocab_path.display()))?;
        let vocab = Vocab::parse(&vocab_text)
            .with_context(|| format!("parse punctuation vocabulary {}", vocab_path.display()))?;

        Ok(Self {
            session,
            vocab,
            thresholds,
        })
    }

    /// Returns model `(comma, period)` probabilities for normalized input characters.
    pub fn probabilities(&mut self, text: &str) -> anyhow::Result<Vec<(f32, f32)>> {
        let normalized = nfkc(text.trim());
        let chars: Vec<char> = normalized.chars().collect();
        let mut probabilities = Vec::with_capacity(chars.len());
        for window in chars.chunks(kikigaki_core::punct::MAX_CHARS) {
            probabilities.extend(self.run_window(window)?);
        }
        Ok(probabilities)
    }

    fn run_window(&mut self, chars: &[char]) -> anyhow::Result<Vec<(f32, f32)>> {
        let (ids, mask) = encode(&self.vocab, chars);
        let sequence_len = ids.len();
        let input_ids = Tensor::from_array(([1usize, sequence_len], ids))?;
        let attention_mask = Tensor::from_array(([1usize, sequence_len], mask))?;
        let outputs = self.session.run(ort::inputs![
            "input_ids" => input_ids,
            "attention_mask" => attention_mask
        ])?;
        let logits = outputs
            .get("logits")
            .context("punctuation model output is missing logits")?;
        let (shape, data) = logits
            .try_extract_tensor::<f32>()
            .context("punctuation logits are not an f32 tensor")?;
        probabilities_from_logits(shape, data, chars.len())
    }
}

impl Punctuator for MojicastPunctuator {
    fn punctuate(&mut self, text: &str) -> anyhow::Result<String> {
        let normalized = nfkc(text.trim());
        if normalized.is_empty() {
            return Ok(normalized);
        }
        let chars: Vec<char> = normalized.chars().collect();
        let thresholds = self.thresholds;
        let punctuated = punctuate_windowed(&chars, |window| self.run_window(window), thresholds)?;
        Ok(apply_question_marks(&punctuated))
    }
}

/// Resolves the bundled runtime, with a models-directory fallback for development and tests that
/// stage ONNX Runtime separately. The download manifest no longer installs this fallback.
fn runtime_path(models_dir: &Path) -> PathBuf {
    if let Some(bundled) = bundled_runtime_path() {
        return bundled;
    }
    models_dir
        .join("onnxruntime-1.27.1")
        .join(if cfg!(target_os = "macos") {
            "libonnxruntime.dylib"
        } else {
            "libonnxruntime.so"
        })
}

/// The ONNX Runtime copy Tauri places in the app bundle (`Contents/Frameworks/`). A hardened-
/// runtime app can only dlopen libraries sealed into its own bundle, so when we run from a bundle
/// this copy wins over the development-only models-directory fallback.
fn bundled_runtime_path() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let contents = exe.parent()?.parent()?;
    let candidate = contents.join("Frameworks").join("libonnxruntime.dylib");
    candidate.is_file().then_some(candidate)
}

fn probabilities_from_logits(
    shape: &[i64],
    data: &[f32],
    char_count: usize,
) -> anyhow::Result<Vec<(f32, f32)>> {
    let expected_sequence_len = char_count + 2;
    let expected_sequence_len_i64 =
        i64::try_from(expected_sequence_len).context("punctuation input is too long")?;
    ensure!(
        shape == [1, expected_sequence_len_i64, 2],
        "punctuation logits shape must be [1, {expected_sequence_len}, 2], got {shape:?}"
    );
    ensure!(
        data.len() == expected_sequence_len * 2,
        "punctuation logits contain {} values, expected {}",
        data.len(),
        expected_sequence_len * 2
    );
    ensure!(
        data.iter().all(|value| value.is_finite()),
        "punctuation logits contain non-finite values"
    );

    Ok((0..char_count)
        .map(|index| {
            let offset = (index + 1) * 2;
            (sigmoid(data[offset]), sigmoid(data[offset + 1]))
        })
        .collect())
}

fn sigmoid(value: f32) -> f32 {
    1.0 / (1.0 + (-value).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logits_require_exact_shape_and_finite_values() {
        assert_eq!(
            probabilities_from_logits(&[1, 4, 2], &[0.0; 8], 2).unwrap(),
            vec![(0.5, 0.5); 2]
        );
        assert!(probabilities_from_logits(&[1, 3, 2], &[0.0; 6], 2).is_err());
        assert!(probabilities_from_logits(&[1, 4, 1], &[0.0; 4], 2).is_err());
        let mut non_finite = [0.0; 8];
        non_finite[3] = f32::NAN;
        assert!(probabilities_from_logits(&[1, 4, 2], &non_finite, 2).is_err());
    }
}
