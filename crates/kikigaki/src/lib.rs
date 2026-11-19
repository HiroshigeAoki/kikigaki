//! kikigaki library crate: Tauri-free modules are declared unconditionally so
//! `cargo test -p kikigaki --lib` runs on any platform; native desktop modules are gated
//! to macOS and Windows.
//!
//! The push-to-talk actor lives in the Tauri-free `controller` module. The desktop shell adapts it
//! to Tauri without exposing platform handles to the controller thread.

pub mod controller;
pub mod history;
pub mod learned;
pub mod platform;
pub mod settings;
mod shutdown;
pub mod status;
pub mod transcript;
pub mod tray;

mod cli;
mod fetch;
#[cfg(any(test, target_os = "macos", target_os = "windows"))]
mod onboarding;
mod permissions;
mod startup;

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod mic;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod paste;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod shell;
#[cfg(all(target_os = "macos", feature = "remote-engine"))]
mod sidecar;

/// Entry point called by `main.rs`.
pub fn run() -> anyhow::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        shell::run()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        eprintln!("kikigaki: runs on macOS or Windows only");
        Ok(())
    }
}
