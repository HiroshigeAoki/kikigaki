//! Windows platform implementations.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use crate::permissions::MicPermission;
use crate::platform::{ProbeBackend, ProbeOutcome};

/// Windows-specific probe timeout: unlike macOS's 120-second consent callback wait, Windows
/// displays its own privacy prompt when an input stream opens, so the probe only waits long enough
/// to learn whether the started stream delivers data.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

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
    let result = if HEALTHY_STREAM.load(Ordering::Relaxed) {
        MicPermission::Authorized
    } else {
        result
    };
    CACHED_PERMISSION.store(result as u8, Ordering::Relaxed);
    result
}

/// A healthy capture stream makes a second hardware probe unnecessary.
static HEALTHY_STREAM: AtomicBool = AtomicBool::new(false);

pub(crate) fn capture_delivered_audio() {
    HEALTHY_STREAM.store(true, Ordering::Relaxed);
    if microphone_permission() != MicPermission::Authorized {
        CACHED_PERMISSION.store(MicPermission::Authorized as u8, Ordering::Relaxed);
    }
}

pub(crate) fn capture_discarded() {
    HEALTHY_STREAM.store(false, Ordering::Relaxed);
}

/// Startup continues after one second even if device activation is hung.
pub fn init_microphone_permission_probe() {
    if microphone_permission() == MicPermission::NotDetermined {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(probe_and_cache());
        });
        let _ = rx.recv_timeout(Duration::from_secs(1));
    }
}

/// Called only on a worker, never from the controller or onboarding poll.
pub fn request_microphone_access(_timeout: Duration) -> Result<bool, String> {
    if HEALTHY_STREAM.load(Ordering::Relaxed) {
        CACHED_PERMISSION.store(MicPermission::Authorized as u8, Ordering::Relaxed);
        return Ok(true);
    }
    Ok(probe_and_cache().is_granted())
}

fn probe_error(error: cpal::Error) -> ProbeOutcome {
    if error.kind() == cpal::ErrorKind::PermissionDenied {
        ProbeOutcome::AccessDenied
    } else {
        ProbeOutcome::StreamOpenOrPlayFailed
    }
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
        let config = match device.default_input_config() {
            Ok(config) => config,
            Err(error) if error.kind() == cpal::ErrorKind::PermissionDenied => {
                return ProbeOutcome::AccessDenied
            }
            Err(_) => return ProbeOutcome::ConfigQueryFailed,
        };
        let got_callback = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&got_callback);
        let access_denied = Arc::new(AtomicBool::new(false));
        let denied_flag = Arc::clone(&access_denied);
        // Permission probing only needs callback delivery, independent of sample format.
        let stream = device.build_input_stream_raw(
            config.config(),
            config.sample_format(),
            move |_, _| {
                flag.store(true, Ordering::SeqCst);
            },
            move |error| {
                if error.kind() == cpal::ErrorKind::PermissionDenied {
                    denied_flag.store(true, Ordering::Relaxed);
                }
                tracing::warn!(%error, "microphone probe stream error");
            },
            Some(crate::mic::STREAM_BUILD_TIMEOUT),
        );
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => return probe_error(error),
        };
        if let Err(error) = stream.play() {
            return probe_error(error);
        }
        let deadline = Instant::now() + timeout;
        while !got_callback.load(Ordering::SeqCst)
            && !access_denied.load(Ordering::Relaxed)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        if access_denied.load(Ordering::Relaxed) {
            return ProbeOutcome::AccessDenied;
        }
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
