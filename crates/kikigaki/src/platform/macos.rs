//! macOS platform implementations.

use crate::permissions::MicPermission;

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

/// macOS requests permission on demand; no startup probe is needed.
pub fn init_microphone_permission_probe() {}

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

/// Returns whether Accessibility trust permits synthetic input.
pub fn accessibility_trusted() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }

    unsafe { AXIsProcessTrusted() }
}

/// Returns whether this app bundle is installed where Launch Services can relocate it.
pub fn autostart_available() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().nth(3).map(std::path::Path::to_path_buf))
        .is_some_and(|app_bundle| {
            let path = app_bundle.to_string_lossy();
            path.starts_with("/Applications/")
                || dirs::home_dir()
                    .is_some_and(|home| app_bundle.starts_with(home.join("Applications")))
        })
}

pub fn autostart_unavailable_message() -> &'static str {
    "/Applications 以外にインストールされているため設定できません"
}

pub(crate) fn settings_pane_uri(pane: crate::shell::OnboardingPane) -> Option<&'static str> {
    use crate::shell::OnboardingPane;

    Some(match pane {
        OnboardingPane::Microphone => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        }
        OnboardingPane::Accessibility => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
    })
}
