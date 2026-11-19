//! Windows platform implementations.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crate::permissions::MicPermission;
use crate::platform::{ProbeBackend, ProbeOutcome};

/// Windows-specific probe timeout: unlike macOS's 120-second consent callback wait, Windows
/// displays its own privacy prompt when an input stream opens, so the probe only waits long enough
/// to learn whether the started stream delivers data.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// `NotDetermined` doubles as "never probed yet", the cache's initial value.
static CACHED_PERMISSION: AtomicU8 = AtomicU8::new(MicPermission::NotDetermined as u8);

/// Reads the cached permission in O(1) without touching audio hardware, so this is safe from the
/// controller loop and the onboarding poll.
pub fn microphone_permission() -> MicPermission {
    MicPermission::from_u8(CACHED_PERMISSION.load(Ordering::Relaxed))
}

fn probe_and_cache() -> MicPermission {
    let result = crate::platform::probe_with::<CpalProbeBackend>(PROBE_TIMEOUT);
    // The byte is the complete cached state and guards no other memory, so Relaxed is sufficient.
    CACHED_PERMISSION.store(result as u8, Ordering::Relaxed);
    result
}

/// Performs the one-time startup probe synchronously before `onboarding::scan`. It is bounded by
/// `PROBE_TIMEOUT` and runs during Tauri setup before the controller thread exists; never call it
/// from the controller loop or the two-second onboarding poll. A first-launch `Busy`/`Unavailable`
/// result is therefore re-probed only by the explicit onboarding request, never by
/// `retry_bootstrap`.
pub fn init_microphone_permission_probe() {
    if CACHED_PERMISSION.load(Ordering::Relaxed) == MicPermission::NotDetermined as u8 {
        probe_and_cache();
    }
}

/// Explicitly probes Windows microphone access and updates the cache. The caller's timeout is
/// ignored because the macOS 120-second consent callback wait is not meaningful on Windows; the
/// probe is instead bounded by `PROBE_TIMEOUT`.
pub fn request_microphone_access(_timeout: Duration) -> Result<bool, String> {
    Ok(probe_and_cache().is_granted())
}

struct CpalProbeBackend;

impl ProbeBackend for CpalProbeBackend {
    fn probe(timeout: Duration) -> ProbeOutcome {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let host = cpal::default_host();
        let Some(device) = host.default_input_device() else {
            return ProbeOutcome::NoDevice;
        };
        let Ok(config) = device.default_input_config() else {
            return ProbeOutcome::ConfigQueryFailed;
        };
        let got_callback = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&got_callback);
        // Permission probing only needs callback delivery, independent of sample format.
        let stream = device.build_input_stream_raw(
            config.config(),
            config.sample_format(),
            move |_, _| {
                flag.store(true, Ordering::SeqCst);
            },
            |error| tracing::warn!(%error, "microphone probe stream error"),
            None,
        );
        let Ok(stream) = stream else {
            return ProbeOutcome::StreamOpenOrPlayFailed;
        };
        if stream.play().is_err() {
            return ProbeOutcome::StreamOpenOrPlayFailed;
        }
        std::thread::sleep(timeout);
        ProbeOutcome::Completed {
            delivered_callback: got_callback.load(Ordering::SeqCst),
        }
    }
}

/// Windows has no Accessibility-trust concept: synthetic input and clipboard access require no
/// separate consent step, so onboarding hides the accessibility step entirely.
pub fn accessibility_trusted() -> bool {
    true
}

pub fn autostart_available() -> bool {
    true
}

pub fn autostart_unavailable_message() -> &'static str {
    // Unreachable on Windows because `autostart_available()` is always true.
    "自動起動を設定できません"
}

pub(crate) fn settings_pane_uri(pane: crate::shell::OnboardingPane) -> Option<&'static str> {
    use crate::shell::OnboardingPane;

    match pane {
        OnboardingPane::Microphone => Some("ms-settings:privacy-microphone"),
        OnboardingPane::Accessibility => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microphone_permission_cache_round_trips() {
        CACHED_PERMISSION.store(MicPermission::Busy as u8, Ordering::Relaxed);
        assert_eq!(microphone_permission(), MicPermission::Busy);
        CACHED_PERMISSION.store(MicPermission::Authorized as u8, Ordering::Relaxed);
        assert_eq!(microphone_permission(), MicPermission::Authorized);
    }
}
