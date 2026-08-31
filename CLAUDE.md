# CLAUDE.md

macOS push-to-talk Japanese dictation app (Rust + Tauri v2). User-facing docs are
Japanese ([README.md](README.md)); developer-facing text (this file, code comments,
commit messages) is English.

## Workspace layout

| Crate | Role |
| --- | --- |
| `crates/kikigaki-core` | Platform-independent audio, protocol, session, and engine support |
| `crates/kikigaki-engine` | In-process speech engine: Silero VAD + ReazonSpeech transducer on sherpa-onnx; mojicast punctuation on ort (feature `punct`) |
| `crates/kikigaki` | Tauri v2 app; macOS-only modules are gated on `target_os = "macos"` so `cargo test --lib` runs on any platform |

## Verification

`scripts/check.sh` is the gate: fmt, clippy with `-D warnings`, and tests across the
feature matrix (default / `remote-engine` / `--no-default-features`). Run it before
declaring a change done. It runs fully on Linux; macOS-only code only compiles on a Mac.

## Building the macOS app

Development happens on Linux; macOS builds run on a separate build Mac over SSH.

- `scripts/mac-sync-build.sh` — sync the source tree to the Mac and build.
- `scripts/release-mac.sh` — release build with signing and packaging gates; produces the DMG.
- Both require env vars documented at the top of each script (`KIKIGAKI_MAC`,
  `KIKIGAKI_KEYCHAIN_PASSWORD`). No secrets live in this repo.
- `KIKIGAKI_MODELS_DIR` is only for test tools that need real models; it does not
  override the GUI's `models_dir` setting.

## Other scripts

- `scripts/test-real-models.sh` — integration tests against real models; downloads
  archives to `SHERPA_ONNX_ARCHIVE_DIR` and extracts to `KIKIGAKI_MODELS_DIR`
  (defaults under `~/.cache`). Works on Linux.
- `scripts/setup-hayamimi.sh` — clones/sets up hayamimi under `KIKIGAKI_ENGINE_ROOT`
  (default `~/dev/voice-engine`) for `remote-engine` sidecar development.
- `scripts/fetch-onnxruntime.sh` — fetches and hash-verifies the vendored ONNX
  Runtime dylib; invoked by the build.

## Conventions

- TDD for product logic; tests are colocated in each crate.
- No direct commits to `main`: branch, then PR, merged with a real merge commit
  (`gh pr merge --merge`, never squash).
- Machine- and maintainer-specific notes live in `CLAUDE.local.md` (gitignored).
