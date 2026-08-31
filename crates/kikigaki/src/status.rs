use kikigaki_core::engine::EnginePhase;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UiStatus {
    pub state: &'static str,
    pub phase: &'static str,
    pub message: Option<String>,
    pub hotkey: String,
    pub punct_enabled: bool,
    pub strip_trailing_period: bool,
    pub engine: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DownloadProgress {
    pub model: String,
    pub done: u32,
    pub total: u32,
    pub bytes: u64,
    pub total_bytes: u64,
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OnboardingState {
    pub microphone: &'static str,
    pub accessibility_trusted: bool,
    pub models_installed: bool,
    pub download: Option<DownloadProgress>,
    pub consent_copy: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutostartState {
    pub enabled: bool,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BootstrapError {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UiSnapshot {
    /// `settings.js::applyRevisionedSnapshot` discards snapshots at or below this revision.
    pub revision: u64,
    pub status: UiStatus,
    pub settings: crate::settings::SettingsSnapshot,
    pub autostart: AutostartState,
    pub onboarding: Option<OnboardingState>,
    pub history: Vec<crate::history::HistoryEntry>,
    pub learned_rules: Vec<crate::learned::LearnedRule>,
    pub bootstrap_error: Option<BootstrapError>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiEventKind {
    Snapshot(Box<UiSnapshot>),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UiEvent {
    pub revision: u64,
    #[serde(flatten)]
    pub kind: UiEventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UiError {
    pub code: &'static str,
    pub message: String,
}

/// What the tray shows besides the session state, and how `Reconnect` recovers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Nothing to report.
    Ok,
    /// Model download/extract progress line (phase comes from the supervisor).
    Installing(String),
    /// Transient warning that does not change the engine phase
    /// (microphone/accessibility permission, microphone unavailable).
    Warning(String),
    /// Startup coordinator failed (install/start/post-processing/model-load invalidation).
    /// Phase is forced to `Failed`; `Reconnect` re-runs the startup coordinator.
    StartupFailed(String),
    /// The engine failed to restart; phase comes from the supervisor;
    /// `Reconnect` restarts the engine.
    EngineFailed(String),
}

impl Status {
    /// Returns the message shown by the tray, if any.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Ok => None,
            Self::Installing(message)
            | Self::Warning(message)
            | Self::StartupFailed(message)
            | Self::EngineFailed(message) => Some(message),
        }
    }

    /// Returns the displayed phase, forcing startup failures to `Failed`.
    pub fn phase(&self, supervisor: EnginePhase) -> EnginePhase {
        if self.is_startup_failed() {
            EnginePhase::Failed
        } else {
            supervisor
        }
    }

    /// Returns whether reconnecting should re-run startup rather than restart the engine.
    pub fn is_startup_failed(&self) -> bool {
        matches!(self, Self::StartupFailed(_))
    }

    /// Clears messages that do not represent a startup failure.
    pub fn clear_transient(&mut self) {
        if !self.is_startup_failed() {
            *self = Self::Ok;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_maps_every_status_variant() {
        assert_eq!(Status::Ok.message(), None);
        assert_eq!(
            Status::Installing("installing".into()).message(),
            Some("installing")
        );
        assert_eq!(Status::Warning("warning".into()).message(), Some("warning"));
        assert_eq!(
            Status::StartupFailed("startup failed".into()).message(),
            Some("startup failed")
        );
        assert_eq!(
            Status::EngineFailed("engine failed".into()).message(),
            Some("engine failed")
        );
    }

    #[test]
    fn only_startup_failed_forces_failed_phase() {
        for status in [
            Status::Ok,
            Status::Installing("installing".into()),
            Status::Warning("warning".into()),
            Status::EngineFailed("engine failed".into()),
        ] {
            assert_eq!(status.phase(EnginePhase::Ready), EnginePhase::Ready);
            assert!(!status.is_startup_failed());
        }

        let status = Status::StartupFailed("startup failed".into());
        assert_eq!(status.phase(EnginePhase::Ready), EnginePhase::Failed);
        assert!(status.is_startup_failed());
    }

    #[test]
    fn clear_transient_clears_every_non_startup_message() {
        for mut status in [
            Status::Ok,
            Status::Installing("installing".into()),
            Status::Warning("warning".into()),
            Status::EngineFailed("engine failed".into()),
        ] {
            status.clear_transient();
            assert_eq!(status, Status::Ok);
        }

        let mut status = Status::StartupFailed("startup failed".into());
        status.clear_transient();
        assert_eq!(status, Status::StartupFailed("startup failed".into()));
    }
}
