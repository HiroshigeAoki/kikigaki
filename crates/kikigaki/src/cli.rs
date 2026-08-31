#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

//! Build-time capability flags. Phase 3a has no CLI surface; model downloads happen through the
//! onboarding window (design §6).

use kikigaki_core::config::Capabilities;

/// Returns the capabilities compiled into this binary.
pub fn capabilities() -> Capabilities {
    Capabilities {
        punct: cfg!(feature = "punct"),
        remote_engine: cfg!(feature = "remote-engine"),
    }
}
