use kikigaki_core::engine::EnginePhase;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UiStatus {
    pub state: &'static str,
    pub level: &'static str,
    pub phase: &'static str,
    pub message: Option<String>,
    pub detail: Option<String>,
    pub hotkey: String,
    pub punct_enabled: bool,
    pub strip_trailing_period: bool,
    pub long_hotkey: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LongStateView {
    Installing { percent: u8 },
    Loading,
    Ready,
    Failed { message: String },
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

/// Display title and optional technical or actionable detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusText {
    pub title: String,
    pub detail: Option<String>,
}

impl StatusText {
    pub fn new(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            detail: Some(detail.into()),
        }
    }
}

impl From<String> for StatusText {
    fn from(title: String) -> Self {
        Self {
            title,
            detail: None,
        }
    }
}

impl From<&str> for StatusText {
    fn from(title: &str) -> Self {
        title.to_owned().into()
    }
}

/// What the tray shows besides the session state, and how `Reconnect` recovers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Nothing to report.
    Ok,
    /// Model download/extract progress line (phase comes from the supervisor).
    Installing(StatusText),
    /// Transient warning that does not change the engine phase
    /// (microphone/accessibility permission, microphone unavailable).
    Warning(StatusText),
    /// Startup coordinator failed (install/start/post-processing/model-load invalidation).
    /// Phase is forced to `Failed`; `Reconnect` re-runs the startup coordinator.
    StartupFailed(StatusText),
    /// The engine failed to restart; phase comes from the supervisor;
    /// `Reconnect` restarts the engine.
    EngineFailed(StatusText),
}

impl Status {
    /// Severity of the effective status shown in the settings card.
    pub fn level(&self) -> &'static str {
        match self {
            Self::Warning(_) => "warning",
            Self::StartupFailed(_) | Self::EngineFailed(_) => "error",
            Self::Ok | Self::Installing(_) => "ok",
        }
    }

    /// Returns the message shown by the tray, if any.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Ok => None,
            Self::Installing(message)
            | Self::Warning(message)
            | Self::StartupFailed(message)
            | Self::EngineFailed(message) => Some(&message.title),
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Ok => None,
            Self::Installing(text)
            | Self::Warning(text)
            | Self::StartupFailed(text)
            | Self::EngineFailed(text) => text.detail.as_deref(),
        }
    }

    pub fn startup_failed(detail: impl Into<String>) -> Self {
        Self::StartupFailed(StatusText::new(
            "音声エンジンを起動できませんでした",
            detail,
        ))
    }

    pub fn engine_failed(detail: impl Into<String>) -> Self {
        Self::EngineFailed(StatusText::new("音声エンジンが停止しました", detail))
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
    fn ui_status_serializes_detail_and_null_like_message() {
        let mut status = UiStatus {
            state: "disconnected",
            level: "error",
            phase: "failed",
            message: Some("起動できませんでした".into()),
            detail: Some("raw error".into()),
            hotkey: "Alt+Space".into(),
            punct_enabled: false,
            strip_trailing_period: false,
            long_hotkey: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["message"], "起動できませんでした");
        assert_eq!(json["detail"], "raw error");
        status.message = None;
        status.detail = None;
        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json.get("message"), Some(&serde_json::Value::Null));
        assert_eq!(json.get("detail"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn text_variants_keep_title_and_optional_detail_separate() {
        assert_eq!(Status::Ok.detail(), None);
        for make in [
            Status::Installing,
            Status::Warning,
            Status::StartupFailed,
            Status::EngineFailed,
        ] {
            let status = make(StatusText::new("日本語", "raw error"));
            assert_eq!(status.message(), Some("日本語"));
            assert_eq!(status.detail(), Some("raw error"));
            assert_eq!(make("title".into()).detail(), None);
        }
    }

    #[test]
    fn long_state_view_serializes_as_tagged() {
        for (state, expected) in [
            (
                LongStateView::Installing { percent: 45 },
                serde_json::json!({"kind":"installing","percent":45}),
            ),
            (
                LongStateView::Loading,
                serde_json::json!({"kind":"loading"}),
            ),
            (LongStateView::Ready, serde_json::json!({"kind":"ready"})),
            (
                LongStateView::Failed {
                    message: "failed".into(),
                },
                serde_json::json!({"kind":"failed","message":"failed"}),
            ),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), expected);
        }
    }

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
