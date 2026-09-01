#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname -- "$0")/.."

echo "== builtin dictionary generator self-test =="
if command -v python3 >/dev/null 2>&1; then
  python3 scripts/gen-dict/gen_builtin_dict.py --self-test
else
  echo "SKIP: python3 is unavailable; builtin dictionary generator self-test not run"
fi

echo "== fmt =="
cargo fmt --all --check

echo "== default features (lib) =="
cargo clippy -p kikigaki-core -p kikigaki-engine -p kikigaki --lib -- -D warnings
cargo test -p kikigaki-core -p kikigaki-engine -p kikigaki --lib

echo "== remote-engine feature: core (incl. the mock-server integration test, not just --lib) =="
cargo clippy -p kikigaki-core --features remote-engine -- -D warnings
cargo test -p kikigaki-core --features remote-engine

echo "== remote-engine feature: the app's own passthrough compiles on Linux too =="
cargo clippy -p kikigaki --lib --no-default-features --features remote-engine -- -D warnings
cargo test -p kikigaki --lib --no-default-features --features remote-engine

echo "== no default features (punct off) =="
cargo test -p kikigaki-core -p kikigaki-engine -p kikigaki --lib --no-default-features
