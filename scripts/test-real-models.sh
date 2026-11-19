#!/usr/bin/env bash
set -euo pipefail

: "${SHERPA_ONNX_ARCHIVE_DIR:=$HOME/.cache/sherpa-onnx-archives}"
: "${KIKIGAKI_MODELS_DIR:=$HOME/.cache/kikigaki/models}"
export SHERPA_ONNX_ARCHIVE_DIR KIKIGAKI_MODELS_DIR

# Keep the pinned revision, filenames and SHA-256 values in sync with
# crates/kikigaki-core/src/models/mod.rs.
haas_id="haas-ja-v1.0-onnx-quantized"
haas_url="https://huggingface.co/oboroge0/Haas-ja-v1.0-onnx-quantized/resolve/7f5ab0a4b1bf39db78afd6691eeecbc4f9d42e39"
haas_files=(encoder.quantized.onnx encoder.data decoder.onnx joiner.onnx tokens.txt)
haas_sha256=(
  aba291d61d3d594bc6fb8c7f90acbd980e7bc7bf6b7caaf3965ddc25e6f42fac
  30befbf6aeb75383e6551f5e58de53efdb76972ddc7d0f76070c517899873586
  2947cb5512699cbba753fe45fe221428aadaa9856597debbb7aabe19cc253622
  8a68320e750890ff9e944a794b41ae1c42c6f439ac4c273503458a6b42adc12d
  ec78b13c5d9f156cf0d5fdbd6b5aec5785680f319c92630c11d5d11b14b83a84
)

verify_sha256() {
  local actual
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$1")"
  else
    actual="$(shasum -a 256 "$1")"
  fi
  actual="${actual%% *}"
  if [[ "$actual" != "$2" ]]; then
    echo "SHA-256 mismatch for $1: expected $2, got $actual" >&2
    return 1
  fi
}

required_files=(
  "reazonspeech-ja-en-2025-01-17/encoder-epoch-35-avg-1.int8.onnx"
  "reazonspeech-ja-en-2025-01-17/decoder-epoch-35-avg-1.int8.onnx"
  "reazonspeech-ja-en-2025-01-17/joiner-epoch-35-avg-1.int8.onnx"
  "reazonspeech-ja-en-2025-01-17/tokens.txt"
  "silero-vad/silero_vad.onnx"
  "mojicast-punct/punct_bert.int8.onnx"
  "mojicast-punct/vocab.txt"
  "whisper-large-v3-turbo-q8_0/ggml-large-v3-turbo-q8_0.bin"
)

if [[ ! -d "$KIKIGAKI_MODELS_DIR" ]]; then
  echo "models directory missing: $KIKIGAKI_MODELS_DIR" >&2
  exit 1
fi
# Download only missing files; reject corrupt cached files instead of trusting their presence.
mkdir -p "$KIKIGAKI_MODELS_DIR/$haas_id"
haas_temporary=""
test_output=""
cleanup() {
  if [[ -n "$haas_temporary" ]]; then rm -f "$haas_temporary"; fi
  if [[ -n "$test_output" ]]; then rm -f "$test_output"; fi
}
trap cleanup EXIT
for index in "${!haas_files[@]}"; do
  file="${haas_files[$index]}"
  destination="$KIKIGAKI_MODELS_DIR/$haas_id/$file"
  if [[ ! -f "$destination" ]]; then
    haas_temporary="$(mktemp "$KIKIGAKI_MODELS_DIR/$haas_id/.download.XXXXXX")"
    curl --fail --location --retry 3 --output "$haas_temporary" "$haas_url/$file"
    verify_sha256 "$haas_temporary" "${haas_sha256[$index]}"
    mv "$haas_temporary" "$destination"
    haas_temporary=""
  else
    verify_sha256 "$destination" "${haas_sha256[$index]}"
  fi
  required_files+=("$haas_id/$file")
done

for relative in "${required_files[@]}"; do
  if [[ ! -f "$KIKIGAKI_MODELS_DIR/$relative" ]]; then
    echo "required model file missing: $KIKIGAKI_MODELS_DIR/$relative" >&2
    if [[ "$relative" == "whisper-large-v3-turbo-q8_0/ggml-large-v3-turbo-q8_0.bin" ]]; then
      # Keep this pinned URL in sync with the Whisper entry in
      # crates/kikigaki-core/src/models/mod.rs.
      echo "download it from: https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo-q8_0.bin" >&2
    fi
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
if [[ "$(uname -s)" == "Darwin" ]]; then
  features="punct,whisper,whisper-metal"
else
  features="punct,whisper"
fi
cargo test --offline --locked -p kikigaki-engine --features "$features" -- --nocapture 2>&1 | tee "$test_output"

if grep -q "SKIPPED" "$test_output"; then
  echo "real-model test run contained SKIPPED" >&2
  exit 1
fi
