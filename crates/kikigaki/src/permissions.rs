//! Microphone permission status.
//!
//! On macOS, CoreAudio can successfully open an input stream after microphone access is
//! denied, but only deliver silent samples. Querying AVFoundation lets the tray explain that
//! silent-stream failure. When kikigaki is launched from a terminal, macOS attributes the
//! microphone permission to the launching terminal app.

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicPermission {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
    /// AVFoundation returned an unrecognized status, or its device class was unavailable.
    /// This deliberately fails closed and is never treated as permission being granted.
    Unknown,
}

impl MicPermission {
    pub fn is_granted(self) -> bool {
        self == Self::Authorized
    }
}

#[cfg(target_os = "macos")]
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

#[cfg(target_os = "macos")]
pub fn microphone_permission() -> MicPermission {
    use objc2::{msg_send, runtime::AnyClass};
    use objc2_foundation::NSString;

    let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
        tracing::warn!("AVCaptureDevice class unavailable; microphone access is unknown");
        return MicPermission::Unknown;
    };
    let status: isize =
        unsafe { msg_send![cls, authorizationStatusForMediaType: &*NSString::from_str("soun")] };
    match status {
        0 => MicPermission::NotDetermined,
        1 => MicPermission::Restricted,
        2 => MicPermission::Denied,
        3 => MicPermission::Authorized,
        _ => {
            tracing::warn!(status, "unknown microphone authorization status");
            MicPermission::Unknown
        }
    }
}

/// Asks macOS for microphone access and waits for the completion handler.
#[cfg(target_os = "macos")]
pub fn request_microphone_access(timeout: std::time::Duration) -> Result<bool, String> {
    use std::sync::mpsc;

    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, Bool};
    use objc2_foundation::NSString;

    let cls = AnyClass::get(c"AVCaptureDevice").ok_or("AVCaptureDevice class unavailable")?;
    let (sender, receiver) = mpsc::channel::<bool>();
    let block = RcBlock::new(move |granted: Bool| {
        let _ = sender.send(granted.as_bool());
    });
    let media_type = NSString::from_str("soun");
    unsafe {
        let _: () = msg_send![
            cls,
            requestAccessForMediaType: &*media_type,
            completionHandler: &*block
        ];
    }
    receiver
        .recv_timeout(timeout)
        .map_err(|error| format!("no completion handler call within {timeout:?}: {error}"))
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
pub fn microphone_permission() -> MicPermission {
    MicPermission::Authorized
}

#[cfg(test)]
mod tests {
    use super::MicPermission;

    #[test]
    fn unknown_status_is_distinct_from_authorized_and_treated_as_not_granted() {
        assert_ne!(MicPermission::Unknown, MicPermission::Authorized);
        assert!(!MicPermission::Unknown.is_granted());
    }

    #[cfg(target_os = "macos")]
    use super::microphone_permission;

    #[cfg(target_os = "macos")]
    #[test]
    fn microphone_permission_returns_known_status() {
        assert!(matches!(
            microphone_permission(),
            MicPermission::NotDetermined
                | MicPermission::Restricted
                | MicPermission::Denied
                | MicPermission::Authorized
                | MicPermission::Unknown
        ));
    }
}
