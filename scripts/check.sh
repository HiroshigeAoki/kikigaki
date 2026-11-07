#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname -- "$0")/.."

echo "== Tauri command registration consistency =="
python3 - <<'PY'
import json
import re
from pathlib import Path

app = Path("crates/kikigaki")

def arrays(path, pattern):
    # Ignore comments so a commented-out registration cannot satisfy the gate.
    source = re.sub(r"/\*.*?\*/|//[^\n]*", "", path.read_text(), flags=re.S)
    matches = re.findall(pattern, source, flags=re.S)
    if not matches:
        raise SystemExit(f"FAIL: no command registration array found in {path}")
    return "\n".join(matches)

handlers = set(re.findall(r"\b[a-z][a-z0-9_]*\b", arrays(
    app / "src/shell.rs", r"generate_handler!\s*\[([^\]]*)\]")))
manifest = set(re.findall(r'"([a-z][a-z0-9_]*)"', arrays(
    app / "build.rs", r"\bcommands\s*\(\s*&\s*\[([^\]]*)\]")))
permissions = set(json.loads((app / "capabilities/settings.json").read_text())["permissions"])
if not handlers:
    raise SystemExit("FAIL: generate_handler! contains no commands")
errors = []
for command in sorted(handlers):
    if command not in manifest:
        errors.append(f"FAIL: {command} missing from crates/kikigaki/build.rs commands manifest")
    permission = "allow-" + command.replace("_", "-")
    if permission not in permissions:
        errors.append(f"FAIL: {permission} missing from crates/kikigaki/capabilities/settings.json")
if errors:
    raise SystemExit("\n".join(errors))
print(f"PASS: {len(handlers)} handler commands have manifest and settings capability entries")
PY

echo "== settings JavaScript syntax =="
if command -v node >/dev/null 2>&1; then
  node --check crates/kikigaki/ui/settings.js
  node --check crates/kikigaki/ui/settings-format.js
  node --test crates/kikigaki/ui-tests/settings-format.test.js
else
  echo "SKIP: node is unavailable; settings JavaScript syntax checks and format tests not run"
fi

echo "== builtin dictionary generator self-test =="
if command -v python3 >/dev/null 2>&1; then
  python3 scripts/gen-dict/gen_builtin_dict.py --self-test
  python3 scripts/gen-dict/gen_builtin_dict.py render --check
else
  echo "SKIP: python3 is unavailable; builtin dictionary generator self-test not run"
fi

echo "== hotword evaluation tooling self-tests =="
if command -v python3 >/dev/null 2>&1; then
  python3 scripts/hotword-eval/make_hotwords.py --self-test
  python3 scripts/hotword-eval/make_hotwords.py render --check
  python3 scripts/hotword-eval/make_bpe_vocab.py --self-test
  python3 scripts/hotword-eval/score.py --self-test
  python3 scripts/hotword-eval/pad-wavs.py --self-test
else
  echo "SKIP: python3 is unavailable; hotword evaluation self-tests not run"
fi

echo "== icon assets =="
if command -v uv >/dev/null 2>&1; then
  uv run scripts/icons/make_icons.py --self-test
  uv run scripts/icons/make_icons.py --check
else
  echo "SKIP: uv is unavailable; icon asset checks not run"
fi

echo "== fmt =="
cargo fmt --all --check

echo "== default features (lib) =="
cargo clippy -p kikigaki-core -p kikigaki-engine -p kikigaki --lib -- -D warnings
cargo test -p kikigaki-core -p kikigaki-engine -p kikigaki --lib

echo "== kikigaki-engine bins (hotword-eval stays compile- and test-checked) =="
cargo clippy -p kikigaki-engine --all-targets -- -D warnings
cargo build -p kikigaki-engine --bins
cargo test -p kikigaki-engine --bins

echo "== remote-engine feature: core (incl. the mock-server integration test, not just --lib) =="
cargo clippy -p kikigaki-core --features remote-engine -- -D warnings
cargo test -p kikigaki-core --features remote-engine

echo "== remote-engine feature: the app's own passthrough compiles on Linux too =="
cargo clippy -p kikigaki --lib --no-default-features --features remote-engine -- -D warnings
cargo test -p kikigaki --lib --no-default-features --features remote-engine

echo "== no default features (punct off) =="
cargo test -p kikigaki-core -p kikigaki-engine -p kikigaki --lib --no-default-features

echo "== whisper feature: engine crate compiles and its feature-independent tests still pass =="
cargo clippy -p kikigaki-engine --features whisper --lib -- -D warnings
cargo test -p kikigaki-engine --features whisper --lib

echo "== whisper feature: app lib compiles and tests pass (isolated: no punct; shell.rs factory arm is macOS-only) =="
cargo clippy -p kikigaki --lib --no-default-features --features whisper -- -D warnings
cargo test -p kikigaki --lib --no-default-features --features whisper

echo "== whisper feature: the shipped Mac combination links too (default punct + whisper) =="
cargo test -p kikigaki --lib --features whisper

echo "== whisper feature: hotword-eval's --engine whisper code path compiles and its own tests pass =="
cargo clippy -p kikigaki-engine --bin hotword-eval --features whisper -- -D warnings
cargo test -p kikigaki-engine --bin hotword-eval --features whisper
