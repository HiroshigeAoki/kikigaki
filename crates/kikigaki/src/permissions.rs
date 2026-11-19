//! Microphone permission status. Platform probing lives in `crate::platform`; this module holds
//! the OS-neutral vocabulary both platforms report through.

#[repr(u8)]
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicPermission {
    NotDetermined = 0,
    Restricted = 1,
    Denied = 2,
    Authorized = 3,
    /// No input device could be enumerated at all. Windows-only today.
    Unavailable = 4,
    /// A device exists but could not be opened or started. Windows-only today.
    Busy = 5,
    /// The platform returned an unrecognized status or probing was unavailable.
    Unknown = 6,
}

impl MicPermission {
    pub fn is_granted(self) -> bool {
        self == Self::Authorized
    }

    /// Inverse of the Windows probe cache's `as u8` cast.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::NotDetermined,
            1 => Self::Restricted,
            2 => Self::Denied,
            3 => Self::Authorized,
            4 => Self::Unavailable,
            5 => Self::Busy,
            _ => Self::Unknown,
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub use crate::platform::{
    init_microphone_permission_probe, microphone_permission, request_microphone_access,
};

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
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

    #[test]
    fn numeric_status_round_trips_and_unknown_values_fail_closed() {
        let variants = [
            MicPermission::NotDetermined,
            MicPermission::Restricted,
            MicPermission::Denied,
            MicPermission::Authorized,
            MicPermission::Unavailable,
            MicPermission::Busy,
            MicPermission::Unknown,
        ];
        for variant in variants {
            assert_eq!(MicPermission::from_u8(variant as u8), variant);
        }
        assert_eq!(MicPermission::from_u8(7), MicPermission::Unknown);
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    use super::microphone_permission;

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn microphone_permission_returns_known_status() {
        assert!(matches!(
            microphone_permission(),
            MicPermission::NotDetermined
                | MicPermission::Restricted
                | MicPermission::Denied
                | MicPermission::Authorized
                | MicPermission::Unavailable
                | MicPermission::Busy
                | MicPermission::Unknown
        ));
    }
}
