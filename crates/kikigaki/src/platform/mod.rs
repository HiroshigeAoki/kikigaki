//! Platform-specific behavior behind a small set of free functions. `shell.rs`, `paste.rs`, and
//! `onboarding.rs` call through here instead of hardcoding macOS or Windows APIs directly, so the
//! Tauri-free `controller` module and the bulk of the shell never need their own `cfg` gates.

/// The raw shape of what a microphone probe attempt observed, before classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    NoDevice,
    AccessDenied,
    ConfigQueryFailed,
    StreamOpenOrPlayFailed,
    /// The stream started and may have delivered at least one callback before its timeout.
    Completed {
        delivered_callback: bool,
    },
}

/// Classifies a raw microphone probe without touching real audio hardware.
pub fn classify_probe_outcome(outcome: ProbeOutcome) -> crate::permissions::MicPermission {
    use crate::permissions::MicPermission;

    match outcome {
        ProbeOutcome::AccessDenied => MicPermission::Denied,
        ProbeOutcome::NoDevice => MicPermission::Unavailable,
        ProbeOutcome::ConfigQueryFailed | ProbeOutcome::StreamOpenOrPlayFailed => {
            MicPermission::Busy
        }
        ProbeOutcome::Completed {
            delivered_callback: true,
        } => MicPermission::Authorized,
        ProbeOutcome::Completed {
            delivered_callback: false,
        } => MicPermission::Unknown,
    }
}

/// Injectable microphone probe backend used to test classification on every OS.
pub trait ProbeBackend {
    fn probe(timeout: std::time::Duration) -> ProbeOutcome;
}

/// Runs an injected probe backend and classifies its result.
pub fn probe_with<B: ProbeBackend>(
    timeout: std::time::Duration,
) -> crate::permissions::MicPermission {
    classify_probe_outcome(B::probe(timeout))
}

/// The modifier held while the synthetic `v` keystroke pastes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteModifier {
    /// Cmd on macOS.
    Meta,
    /// Ctrl on Windows.
    Control,
}

/// Returns the paste modifier for an explicit OS policy.
pub fn paste_modifier_for(os: kikigaki_core::config::Os) -> PasteModifier {
    match os {
        kikigaki_core::config::Os::Windows => PasteModifier::Control,
        kikigaki_core::config::Os::MacOs => PasteModifier::Meta,
    }
}

/// Returns the paste modifier for the current supported desktop platform.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn paste_modifier() -> PasteModifier {
    paste_modifier_for(kikigaki_core::config::Os::current())
}

/// Opens a file or directory in the OS default handler.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    open_target(path.as_os_str())
}

/// Opens a URI in the OS default handler.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn open_uri(uri: &str) -> std::io::Result<()> {
    open_target(std::ffi::OsStr::new(uri))
}

#[cfg(target_os = "macos")]
fn open_target(target: &std::ffi::OsStr) -> std::io::Result<()> {
    let status = std::process::Command::new("open").arg(target).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "open exited with status {status}"
        )))
    }
}

/// Uses `ShellExecuteW` directly because `cmd /C start` cannot safely quote shell metacharacters
/// and can flash a console window even when the app uses `windows_subsystem = "windows"`.
#[cfg(target_os = "windows")]
fn open_target(target: &std::ffi::OsStr) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let mut target = target.encode_wide().collect::<Vec<_>>();
    if target.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "open target contains an interior NUL",
        ));
    }
    target.push(0);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            windows_sys::core::w!("open"),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result as isize;
    if code <= 32 {
        Err(std::io::Error::other(format!(
            "ShellExecuteW failed with code {code}"
        )))
    } else {
        Ok(())
    }
}

/// `"macos"` or `"windows"`: the native desktop platform name.
/// Explicit per-target constants ensure that an accidentally compiled
/// third target reports `"unsupported"` instead of silently claiming to be macOS.
#[cfg(target_os = "windows")]
pub const NAME: &str = "windows";
/// The native desktop platform name.
#[cfg(target_os = "macos")]
pub const NAME: &str = "macos";
/// The native desktop platform name.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const NAME: &str = "unsupported";

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(test)]
mod probe_tests {
    use super::*;
    use crate::permissions::MicPermission;

    #[test]
    fn classify_probe_outcome_covers_every_mic_permission_this_can_produce() {
        assert_eq!(
            classify_probe_outcome(ProbeOutcome::AccessDenied),
            MicPermission::Denied
        );
        assert_eq!(
            classify_probe_outcome(ProbeOutcome::NoDevice),
            MicPermission::Unavailable
        );
        assert_eq!(
            classify_probe_outcome(ProbeOutcome::ConfigQueryFailed),
            MicPermission::Busy
        );
        assert_eq!(
            classify_probe_outcome(ProbeOutcome::StreamOpenOrPlayFailed),
            MicPermission::Busy
        );
        assert_eq!(
            classify_probe_outcome(ProbeOutcome::Completed {
                delivered_callback: true
            }),
            MicPermission::Authorized
        );
        assert_eq!(
            classify_probe_outcome(ProbeOutcome::Completed {
                delivered_callback: false
            }),
            MicPermission::Unknown
        );
    }

    struct FakeBackend;

    impl ProbeBackend for FakeBackend {
        fn probe(_timeout: std::time::Duration) -> ProbeOutcome {
            ProbeOutcome::Completed {
                delivered_callback: true,
            }
        }
    }

    #[test]
    fn probe_with_dispatches_through_an_injected_backend() {
        assert_eq!(
            probe_with::<FakeBackend>(std::time::Duration::ZERO),
            MicPermission::Authorized
        );
    }
}

#[cfg(test)]
mod paste_modifier_tests {
    use super::*;

    #[test]
    fn paste_modifier_for_matches_os_policy() {
        assert_eq!(
            paste_modifier_for(kikigaki_core::config::Os::Windows),
            PasteModifier::Control
        );
        assert_eq!(
            paste_modifier_for(kikigaki_core::config::Os::MacOs),
            PasteModifier::Meta
        );
    }
}

pub(crate) const MICROPHONE_PERMISSION_DETAIL: &str = if cfg!(target_os = "windows") {
    "Windows の設定 › プライバシーとセキュリティ › マイク で kikigaki を許可してください"
} else {
    "システム設定 › プライバシーとセキュリティ › マイク で kikigaki を許可してください"
};
