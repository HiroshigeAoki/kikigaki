use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use kikigaki_core::config::DecodingMethod;
use kikigaki_core::models::{Payload, ASR_MODEL_ID, MODELS};
use sha2::{Digest, Sha256};

/// Embedded canonical hotword readings (generated; see scripts/hotword-eval/make_hotwords.py).
const BUILTIN_HOTWORDS: &str = include_str!("../data/hotwords.txt");

/// Paths to the materialized files consumed by sherpa-onnx hotword biasing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotwordSetup {
    /// Canonical hotword readings, one per line.
    pub hotwords_file: PathBuf,
    /// Sentencepiece-style vocabulary synthesized from the installed model tokens.
    pub bpe_vocab: PathBuf,
}

/// Resolves the optional recognizer argument for the requested hotword state.
///
/// sherpa-onnx only supports hotword biasing with modified-beam-search decoding. Callers that
/// requested hotwords with greedy search should warn that recognition will continue without
/// hotwords.
pub fn resolve_hotword_arg(
    enabled: bool,
    score: f32,
    decoding_method: DecodingMethod,
) -> Option<f32> {
    (enabled && decoding_method == DecodingMethod::ModifiedBeamSearch).then_some(score)
}

/// Materializes `hotwords.txt` and a synthesized `bpe.vocab` under `models_dir/hotwords/`.
///
/// This ports `make_bpe_vocab.py`: for `tokens.txt` line `i` (`<piece> <id>`), it emits
/// `<piece>\t<-0.1*i>`. Score formatting matches the Python generator exactly, including the
/// `i == 0` special case: Python always emits the literal `"0.0"`, never `"-0.0"`.
///
/// This is the byte-level BPE (bbpe) vocabulary synthesis used by the production path. The
/// separate cjkchar-era per-character `tokens.txt` preflight from the evaluation tooling is
/// intentionally not carried forward: bbpe's byte fallback can encode any input, so malformed
/// piece rejection plus the caller's graceful-degradation path cover those failure modes.
pub fn materialize(models_dir: &Path) -> anyhow::Result<HotwordSetup> {
    materialize_with_expected_sha(models_dir, manifest_tokens_sha256()?)
}

fn manifest_tokens_sha256() -> anyhow::Result<&'static str> {
    let model = MODELS
        .iter()
        .find(|model| model.id == ASR_MODEL_ID)
        .ok_or_else(|| anyhow!("ASR model {ASR_MODEL_ID} is missing from the model manifest"))?;
    let files = match &model.payload {
        Payload::File(file) => std::slice::from_ref(file),
        Payload::Files(files) | Payload::TarBz2 { files, .. } => files,
    };
    files
        .iter()
        .find(|file| file.name == "tokens.txt")
        .map(|file| file.sha256)
        .ok_or_else(|| anyhow!("tokens.txt is missing from the {ASR_MODEL_ID} manifest"))
}

fn materialize_with_expected_sha(
    models_dir: &Path,
    expected_sha256: &str,
) -> anyhow::Result<HotwordSetup> {
    let tokens_path = models_dir.join(ASR_MODEL_ID).join("tokens.txt");
    let tokens = fs::read(&tokens_path)
        .with_context(|| format!("read installed tokens.txt from {}", tokens_path.display()))?;
    let actual_sha256 = hex::encode(Sha256::digest(&tokens));
    if actual_sha256 != expected_sha256 {
        tracing::warn!(
            path = %tokens_path.display(),
            expected_sha256,
            actual_sha256,
            "tokens.txt digest mismatch; hotword setup disabled"
        );
        bail!(
            "tokens.txt digest mismatch for {}: expected {expected_sha256}, got {actual_sha256}",
            tokens_path.display()
        );
    }

    let tokens_text = std::str::from_utf8(&tokens)
        .with_context(|| format!("tokens.txt is not UTF-8: {}", tokens_path.display()))?;
    let pieces = parse_pieces(tokens_text, &tokens_path)?;
    let vocab = render_vocab(pieces)?;

    let output_dir = models_dir.join("hotwords");
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("create hotword directory {}", output_dir.display()))?;
    let hotwords_file = output_dir.join("hotwords.txt");
    let bpe_vocab = output_dir.join("bpe.vocab");
    atomic_write(&output_dir, &hotwords_file, BUILTIN_HOTWORDS.as_bytes())?;
    atomic_write(&output_dir, &bpe_vocab, &vocab)?;

    Ok(HotwordSetup {
        hotwords_file,
        bpe_vocab,
    })
}

fn parse_pieces(contents: &str, source: &Path) -> anyhow::Result<Vec<String>> {
    let mut pieces = Vec::new();
    let mut seen = HashSet::new();
    for (index, raw_line) in contents.split_inclusive('\n').enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim_end_matches(['\r', '\n']);
        let Some((piece, token_id)) = line.split_once(' ') else {
            bail!(
                "{}:{line_number}: missing space separator",
                source.display()
            );
        };
        if piece.is_empty() || token_id.is_empty() || token_id.contains(' ') {
            bail!(
                "{}:{line_number}: malformed token line; expected <piece> <id>",
                source.display()
            );
        }
        if !seen.insert(piece.to_owned()) {
            bail!(
                "{}:{line_number}: duplicate piece {piece:?}",
                source.display()
            );
        }
        pieces.push(piece.to_owned());
    }
    Ok(pieces)
}

fn render_vocab(pieces: impl IntoIterator<Item = String>) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::new();
    for (index, piece) in pieces.into_iter().enumerate() {
        let score = if index == 0 {
            "0.0".to_owned()
        } else {
            let signed_index = i64::try_from(index).context("token index exceeds i64 range")?;
            format!("{:.1}", (-signed_index) as f64 * 0.1)
        };
        writeln!(output, "{piece}\t{score}").context("render BPE vocabulary")?;
    }
    Ok(output)
}

fn atomic_write(directory: &Path, destination: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("create temporary file in {}", directory.display()))?;
    temporary
        .write_all(contents)
        .with_context(|| format!("write temporary file for {}", destination.display()))?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("persist {} atomically", destination.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use sha2::{Digest, Sha256};

    use super::*;

    const FIXTURE_TOKENS: &[u8] = include_bytes!("../tests/fixtures/hotwords/tokens.txt");
    const FIXTURE_VOCAB: &[u8] = include_bytes!("../tests/fixtures/hotwords/bpe.vocab");

    fn fixture_models_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = dir.path().join(kikigaki_core::models::ASR_MODEL_ID);
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join("tokens.txt"), FIXTURE_TOKENS).unwrap();
        dir
    }

    fn fixture_sha256() -> String {
        hex::encode(Sha256::digest(FIXTURE_TOKENS))
    }

    #[test]
    fn resolves_hotword_argument_for_modified_beam_search() {
        assert_eq!(
            resolve_hotword_arg(true, 3.0, DecodingMethod::ModifiedBeamSearch),
            Some(3.0)
        );
    }

    #[test]
    fn disabled_hotwords_have_no_argument_for_any_decoder() {
        assert_eq!(
            resolve_hotword_arg(false, 3.0, DecodingMethod::ModifiedBeamSearch),
            None
        );
        assert_eq!(
            resolve_hotword_arg(false, 3.0, DecodingMethod::GreedySearch),
            None
        );
    }

    #[test]
    fn greedy_search_caller_warns_when_hotwords_are_degraded() {
        let warning = capture_warning(|| {
            let resolved = resolve_hotword_arg(true, 3.0, DecodingMethod::GreedySearch);
            if resolved.is_none() {
                tracing::warn!(
                    "hotword biasing requires modified_beam_search; continuing without hotwords"
                );
            }
            anyhow::anyhow!("capture subscriber output")
        });

        assert!(
            warning.contains("requires modified_beam_search"),
            "{warning:?}"
        );
    }

    #[test]
    fn fixture_matches_python_generator_byte_for_byte() {
        let dir = fixture_models_dir();
        let setup = materialize_with_expected_sha(dir.path(), &fixture_sha256()).unwrap();
        assert_eq!(fs::read(setup.bpe_vocab).unwrap(), FIXTURE_VOCAB);
        assert_eq!(
            fs::read(setup.hotwords_file).unwrap(),
            BUILTIN_HOTWORDS.as_bytes()
        );
    }

    #[test]
    fn score_formatting_matches_python() {
        let pieces = (0..=15).map(|index| format!("piece{index}"));
        let rendered = render_vocab(pieces).unwrap();
        let lines = std::str::from_utf8(&rendered)
            .unwrap()
            .lines()
            .collect::<Vec<_>>();
        assert_eq!(lines[0], "piece0\t0.0");
        assert_eq!(lines[1], "piece1\t-0.1");
        assert_eq!(lines[15], "piece15\t-1.5");
        assert!(!lines[0].contains("-0.0"));
    }

    #[test]
    fn malformed_token_lines_are_rejected() {
        for (tokens, message) in [
            ("<blk> 0\nmalformed\n", "missing space separator"),
            ("<blk> 0\n piece\n", "malformed token line"),
            ("<blk> 0\npiece \n", "malformed token line"),
            ("<blk> 0\npiece 1 2\n", "malformed token line"),
            ("<blk> 0\n<blk> 1\n", "duplicate piece"),
        ] {
            let error = parse_pieces(tokens, std::path::Path::new("tokens.txt")).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
    }

    #[test]
    fn materialize_is_atomic_and_overwrites_cleanly() {
        let dir = fixture_models_dir();
        let expected_sha = fixture_sha256();
        let first = materialize_with_expected_sha(dir.path(), &expected_sha).unwrap();
        fs::write(&first.bpe_vocab, b"stale").unwrap();
        fs::write(&first.hotwords_file, b"stale").unwrap();

        let second = materialize_with_expected_sha(dir.path(), &expected_sha).unwrap();
        assert_eq!(fs::read(second.bpe_vocab).unwrap(), FIXTURE_VOCAB);
        assert_eq!(
            fs::read(second.hotwords_file).unwrap(),
            BUILTIN_HOTWORDS.as_bytes()
        );

        let entries = fs::read_dir(dir.path().join("hotwords"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 2, "temporary files remained: {entries:?}");
    }

    #[test]
    fn failed_atomic_write_leaves_no_temporary_file() {
        let dir = fixture_models_dir();
        let output_dir = dir.path().join("hotwords");
        fs::create_dir_all(output_dir.join("bpe.vocab")).unwrap();

        let error = materialize_with_expected_sha(dir.path(), &fixture_sha256()).unwrap_err();
        assert!(format!("{error:#}").contains("persist"), "{error:#}");
        let entries = fs::read_dir(output_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 2, "temporary files remained: {entries:?}");
    }

    #[test]
    fn missing_tokens_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let error = materialize(dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("tokens.txt"), "{error:#}");
    }

    #[test]
    fn digest_mismatch_warns_and_errors() {
        let dir = fixture_models_dir();
        let warning = capture_warning(|| materialize(dir.path()).unwrap_err());
        assert!(
            warning.contains("digest mismatch"),
            "warning was {warning:?}"
        );
    }

    fn capture_warning(run: impl FnOnce() -> anyhow::Error) -> String {
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = WarningSubscriber(messages.clone());
        let _error = tracing::subscriber::with_default(subscriber, run);
        let output = messages.lock().unwrap().join("\n");
        output
    }

    struct WarningSubscriber(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl tracing::Subscriber for WarningSubscriber {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            struct Visitor(String);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        self.0.push_str(&format!("{value:?}"));
                    }
                }
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }
}
