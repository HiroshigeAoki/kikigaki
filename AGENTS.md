# AGENTS.md

macOS push-to-talk Japanese dictation app (Rust + Tauri v2). User-facing docs are
Japanese ([README.md](README.md)); developer-facing text (these instructions, code comments,
commit messages) is English.

## Verification

`scripts/check.sh` is the gate: fmt, clippy with `-D warnings`, and tests across the
feature matrix (default / `remote-engine` / `--no-default-features`). Run it before
declaring a change done. It runs fully on Linux; macOS-only code (`target_os = "macos"`)
only compiles on a Mac.

## Building the macOS app

- Development happens on Linux; macOS builds run on a build Mac over SSH via
  `scripts/mac-sync-build.sh` / `scripts/release-mac.sh`; env vars are documented in the
  `mac-sync-build.sh` header. No secrets live in this repo.
- `KIKIGAKI_MODELS_DIR` is only for test tools; it does not override the GUI's `models_dir` setting.

## Conventions

- TDD for product logic; tests are colocated in each crate.
- After any settings UI change, re-run `node scripts/ui-harness/shoot.js` (needs
  playwright-core + Chromium) and check the screenshots.
- No direct commits to `main`: branch, then PR, merged with a real merge commit
  (`gh pr merge --merge`, never squash).
- Machine- and maintainer-specific notes live in `CLAUDE.local.md` (gitignored).
