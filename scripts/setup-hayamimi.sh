#!/usr/bin/env bash
set -euo pipefail

ENGINE_ROOT=${KIKIGAKI_ENGINE_ROOT:-"$HOME/dev/voice-engine"}
HAYAMIMI_DIR="$ENGINE_ROOT/hayamimi"

mkdir -p "$ENGINE_ROOT"
if [[ ! -e "$HAYAMIMI_DIR" ]]; then
  git clone https://github.com/oboroge0/hayamimi.git "$HAYAMIMI_DIR"
elif [[ ! -d "$HAYAMIMI_DIR/.git" ]]; then
  echo "setup-hayamimi: $HAYAMIMI_DIR exists but is not a Git checkout" >&2
  exit 1
fi

PYTHON=
for candidate in \
  "$HOME/.pyenv/versions/3.13.7/bin/python3" \
  "$HOME/.pyenv/versions/3.10.6/bin/python3" \
  /opt/homebrew/bin/python3; do
  if [[ -x "$candidate" ]] && "$candidate" -c 'import ssl' >/dev/null 2>&1; then
    PYTHON=$candidate
    break
  fi
done

if [[ -z "$PYTHON" ]]; then
  echo "setup-hayamimi: no supported Python with ssl was found" >&2
  exit 1
fi

cd "$HAYAMIMI_DIR"
if [[ ! -x .venv/bin/python ]]; then
  "$PYTHON" -m venv .venv
fi
.venv/bin/python -m pip install -r requirements.txt
.venv/bin/python scripts/download_models.py --minimal
