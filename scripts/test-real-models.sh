#!/usr/bin/env bash
set -euo pipefail

: "${SHERPA_ONNX_ARCHIVE_DIR:=$HOME/.cache/sherpa-onnx-archives}"
: "${KIKIGAKI_MODELS_DIR:=$HOME/.cache/kikigaki/models}"
export SHERPA_ONNX_ARCHIVE_DIR KIKIGAKI_MODELS_DIR

required_files=(
  "reazonspeech-ja-en-2025-01-17/encoder-epoch-35-avg-1.int8.onnx"
  "reazonspeech-ja-en-2025-01-17/decoder-epoch-35-avg-1.int8.onnx"
  "reazonspeech-ja-en-2025-01-17/joiner-epoch-35-avg-1.int8.onnx"
  "reazonspeech-ja-en-2025-01-17/tokens.txt"
  "silero-vad/silero_vad.onnx"
  "mojicast-punct/punct_bert.int8.onnx"
  "mojicast-punct/vocab.txt"
)

if [[ ! -d "$KIKIGAKI_MODELS_DIR" ]]; then
  echo "models directory missing: $KIKIGAKI_MODELS_DIR" >&2
  exit 1
fi
for relative in "${required_files[@]}"; do
  if [[ ! -f "$KIKIGAKI_MODELS_DIR/$relative" ]]; then
    echo "required model file missing: $KIKIGAKI_MODELS_DIR/$relative" >&2
    exit 1
  fi
done

test_wavs_dir="${KIKIGAKI_TEST_WAVS_DIR:-$KIKIGAKI_MODELS_DIR/reazonspeech-ja-en-2025-01-17/test_wavs}"
for wav in test_ja_1.wav test_ja_2.wav; do
  if [[ ! -f "$test_wavs_dir/$wav" ]]; then
    echo "required test WAV missing: $test_wavs_dir/$wav" >&2
    exit 1
  fi
done

test_output="$(mktemp)"
trap 'rm -f "$test_output"' EXIT
cargo test --offline --locked -p kikigaki-engine --all-features -- --nocapture 2>&1 | tee "$test_output"

if grep -q "SKIPPED" "$test_output"; then
  echo "real-model test run contained SKIPPED" >&2
  exit 1
fi
