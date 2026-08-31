//! kikigaki library crate: Tauri-free modules are declared unconditionally so
//! `cargo test -p kikigaki --lib` runs on any platform; macOS-only modules that touch AVFoundation,
//! CoreGraphics, or Tauri itself are gated to `target_os = "macos"`.
//!
//! The push-to-talk actor lives in the Tauri-free `controller` module. The macOS shell adapts it
//! to Tauri without exposing platform handles to the controller thread.

pub mod controller;
pub mod correction;
pub mod history;
pub mod learned;
pub mod settings;
mod shutdown;
pub mod status;
pub mod tray;

mod cli;
mod fetch;
#[cfg(any(test, target_os = "macos"))]
mod onboarding;
#[cfg(any(test, target_os = "macos"))]
mod permissions;
mod startup;

#[cfg(target_os = "macos")]
mod mic;
#[cfg(target_os = "macos")]
mod paste;
#[cfg(target_os = "macos")]
mod shell;
#[cfg(all(target_os = "macos", feature = "remote-engine"))]
mod sidecar;

/// Entry point called by `main.rs`.
pub fn run() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        shell::run()
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("kikigaki: Phase 3a runs on macOS only");
        Ok(())
    }
}
