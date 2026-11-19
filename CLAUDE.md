# CLAUDE.md

macOS/Windows push-to-talk Japanese dictation app (Rust + Tauri v2). User-facing docs are
Japanese ([README.md](README.md)); developer-facing text (this file, code comments,
commit messages) is English.

## Workspace layout

| Crate | Role |
| --- | --- |
| `crates/kikigaki-core` | Platform-independent audio, protocol, session, and engine support |
| `crates/kikigaki-engine` | In-process speech engine: Silero VAD + Haas-ja (default) or ReazonSpeech transducer on sherpa-onnx; mojicast punctuation on ort for ReazonSpeech (feature `punct`); Haas provides its own punctuation |
| `crates/kikigaki` | Tauri v2 app; platform-specific modules are gated on `target_os = "macos"` / `"windows"` so `cargo test --lib` runs on any platform |

## Verification

`scripts/check.sh` is the gate: fmt, clippy with `-D warnings`, and tests across the
feature matrix (default / `remote-engine` / `--no-default-features`). Run it before
declaring a change done. It runs fully on Linux; macOS- and Windows-only code only compiles on their respective platforms.

## Building the macOS app

Development happens on Linux; macOS builds run on a separate build Mac over SSH.

- `scripts/mac-sync-build.sh` — sync the source tree to the Mac and build.
- `scripts/release-mac.sh` — release build with signing and packaging gates; produces the DMG.
- Both require env vars documented at the top of each script (`KIKIGAKI_MAC`,
  `KIKIGAKI_KEYCHAIN_PASSWORD`). No secrets live in this repo.
- `KIKIGAKI_MODELS_DIR` is only for test tools that need real models; it does not
  override the GUI's `models_dir` setting.

## Building the Windows app

Development happens on Linux; Windows builds run on a separate build machine over SSH.

- `scripts/win-sync-build.sh` — sync the source tree to the Windows build machine and build.
- `scripts/win-build.ps1` — Windows-side build/test driver invoked by `win-sync-build.sh`;
  provides `build` and `test` subcommands.
- `scripts/release-win.ps1` — release build with signing-equivalent gates (GPL scan, DLL
  allowlist, ORT provenance, release metadata) run against the installed NSIS payload, not the
  pre-bundle tree; installs, smoke-tests, exercises autostart, and uninstalls as part of the
  gate; produces the NSIS installer in a versioned `release-out\windows\<version>\` directory
  beside `target\`, outside the synced `src\` tree.
- `scripts/fetch-sherpa-win.ps1` — downloads and hash-verifies the vendored sherpa-onnx
  Windows shared-library archive (five pinned files: three runtime DLLs plus two `.lib` import
  libraries); invoked by `win-build.ps1` and `release-win.ps1`.
- Requires `KIKIGAKI_WIN` to specify the SSH destination of the Windows build machine. No secrets live in this repo.

## Other scripts

- `scripts/test-real-models.sh` — integration tests against real models; downloads
  archives to `SHERPA_ONNX_ARCHIVE_DIR` and extracts to `KIKIGAKI_MODELS_DIR`
  (defaults under `~/.cache`). Works on Linux.
- `scripts/setup-hayamimi.sh` — clones/sets up hayamimi under `KIKIGAKI_ENGINE_ROOT`
  (default `~/dev/voice-engine`) for `remote-engine` sidecar development.
- `scripts/fetch-onnxruntime.sh` — fetches and hash-verifies the vendored ONNX
  Runtime dylib; invoked by the build.
- `scripts/ui-harness/` — dev-only worst-case fixtures for the settings window
  (mocked Tauri bridge, Demo/Worst/Empty/... toggle); `node scripts/ui-harness/shoot.js`
  screenshots every fixture light/dark at 480x640 into `scripts/ui-harness/out/`.
  Re-run it after any settings UI change. Needs playwright-core + Chromium.
- `scripts/hotword-eval/` — offline hotword-biasing evaluation tooling (list
  generator, synthesized bbpe vocab, Mac TTS audio, `hotword-eval` bin, scorer);
  self-tests run in `check.sh`. See `docs/plans/2026-09-01-hotword-eval.md`.

## Conventions

- TDD for product logic; tests are colocated in each crate.
- No direct commits to `main`: branch, then PR, merged with a real merge commit
  (`gh pr merge --merge`, never squash).
- Machine- and maintainer-specific notes live in `CLAUDE.local.md` (gitignored).
