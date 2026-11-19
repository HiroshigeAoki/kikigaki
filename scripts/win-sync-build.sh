#!/usr/bin/env bash
# Sync this checkout (main or any worktree) to the Windows build machine and build it there.
#
#   scripts/win-sync-build.sh              # sync + cargo tauri build
#   KIKIGAKI_WIN=user@host scripts/win-sync-build.sh
#
# Destination on the Windows box: C:\dev\kikigaki-<branch>\src (slashes in the branch name become
# '-'). Windows has no rsync; this streams a tar archive over ssh instead and extracts it with the
# built-in bsdtar (`tar`, present on Windows 10+). `src\` is wiped and recreated every sync (no
# stale files); `target\` and the sherpa fetch cache live outside it and persist. The source SHA is
# written to SYNC_SHA in the destination.
# KIKIGAKI_WIN may be an ssh alias (e.g. KIKIGAKI_WIN=win-build), and the remote login shell is cmd.exe.
# The build machine enforces Smart App Control, so a freshly compiled build script or exe,
# including makensis.exe during `cargo tauri build`, may fail once with os error 4551; rerun after
# ~45 s.
set -euo pipefail

HOST=${KIKIGAKI_WIN:?set KIKIGAKI_WIN to user@host of the Windows build machine}
ROOT=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
BRANCH=$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)
SHA=$(git -C "$ROOT" rev-parse --short HEAD)
DIRTY=$(git -C "$ROOT" status --porcelain | wc -l | tr -d ' ')
SAFE_BRANCH=$(printf '%s' "$BRANCH" | tr '/' '-')
WIN_ROOT="C:\\dev\\kikigaki-${SAFE_BRANCH}"
WIN_SRC="${WIN_ROOT}\\src"

echo "source: $ROOT ($BRANCH @ $SHA, $DIRTY uncommitted paths)"
echo "dest:   $HOST:$WIN_SRC"
printf '%s %s dirty=%s\n' "$BRANCH" "$SHA" "$DIRTY" > "$ROOT/SYNC_SHA"

ssh -o ControlPath=none "$HOST" "cmd /C \"(if exist \"$WIN_SRC\" rmdir /S /Q \"$WIN_SRC\") && mkdir \"$WIN_SRC\"\""
# --exclude target/.git/.worktrees mirrors mac-sync-build.sh; tar over ssh replaces rsync.
tar -C "$ROOT" --exclude target --exclude .git --exclude .worktrees \
  --exclude vendor/onnxruntime -cf - . \
  | ssh -o ControlPath=none "$HOST" "tar -xf - -C \"$WIN_SRC\""

ssh -o ControlPath=none "$HOST" "cd /d \"$WIN_SRC\" && type SYNC_SHA && powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\\win-build.ps1 build"
