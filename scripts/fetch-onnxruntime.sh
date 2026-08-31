#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
DEST_DIR="$ROOT/crates/kikigaki/vendor/onnxruntime"
DEST="$DEST_DIR/libonnxruntime.dylib"
ARCHIVE_URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.6/sherpa-onnx-v1.13.6-osx-arm64-shared-lib.tar.bz2"
ARCHIVE_SHA256="d628e43aed6b719be163549876f41c909b75df26b8f439a5af69de03896bc6f5"
FILE_SHA256="18e1f2535084522445927f21ccb4f903b093b47911045750e56b6ce2f2106d79"
# The `onnxruntime-1.27.1` manifest entry is gone. This script is the single source of the pinned
# archive and library hashes used to stage the runtime for the app bundle.

if [[ -f "$DEST" ]] && shasum -a 256 "$DEST" | awk '{print $1}' | grep -qx "$FILE_SHA256"; then
  echo "fetch-onnxruntime: $DEST already verified, skipping"
  exit 0
fi

mkdir -p "$DEST_DIR"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
curl -fsSL -o "$TMP/ort.tar.bz2" "$ARCHIVE_URL"
echo "$ARCHIVE_SHA256  $TMP/ort.tar.bz2" | shasum -a 256 -c -
tar -xjf "$TMP/ort.tar.bz2" -C "$TMP"
FOUND=$(find "$TMP" -name 'libonnxruntime.dylib' -print -quit)
[[ -n "$FOUND" ]] || { echo "fetch-onnxruntime: libonnxruntime.dylib not found in archive" >&2; exit 1; }
shasum -a 256 "$FOUND" | awk '{print $1}' | grep -qx "$FILE_SHA256" || { echo "fetch-onnxruntime: sha256 mismatch" >&2; exit 1; }
cp "$FOUND" "$DEST"
echo "fetch-onnxruntime: staged $DEST"
