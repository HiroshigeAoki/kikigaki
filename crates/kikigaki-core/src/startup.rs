//! Portable model-installation coordinator used before an engine is constructed.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use crate::config::{punct_effective, Capabilities, Config, EngineKind};
use crate::engine::panic_reason;
use crate::models::fetch::Fetcher;
use crate::models::{ensure_installed, required, Model, Progress};

pub use crate::models::InstallReport;

const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// Fully resolved work required before constructing a transcription engine.
#[derive(Debug, Clone)]
pub struct StartupPlan {
    /// Engine that will be constructed after installation succeeds.
    pub engine: EngineKind,
    /// Root directory containing model installation directories.
    pub models_dir: PathBuf,
    /// Models required by the selected engine and effective capabilities.
    pub required: Vec<&'static Model>,
}

impl StartupPlan {
    /// Builds a startup plan from validated, path-resolved configuration.
    pub fn from_config(config: &Config, caps: Capabilities) -> Self {
        Self {
            engine: config.engine,
            models_dir: config.models_dir.clone(),
            required: required(config.engine, caps, punct_effective(config, caps)),
        }
    }
}

/// Status emitted by the startup worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupEvent {
    /// A model payload is being downloaded or processed.
    Installing {
        /// Stable model identifier.
        id: &'static str,
        /// Cumulative bytes downloaded for the current payload.
        done_bytes: u64,
        /// Expected payload byte count.
        total_bytes: u64,
    },
    /// All required models are installed and verified.
    Installed(InstallReport),
    /// Startup failed before an engine could be constructed.
    Failed(String),
}

/// Installs a startup plan and sends terminal success or failure to the application.
///
/// The caller runs this function on a background thread. Installer panics are caught and
/// converted into `Failed`, matching the engine worker's failure-reporting behavior. Progress
/// events for each model are emitted at most once per second.
pub fn run_startup(plan: StartupPlan, fetcher: &dyn Fetcher, tx: SyncSender<StartupEvent>) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut last_update = HashMap::<&'static str, Instant>::new();
        let mut progress = |update: Progress<'_>| {
            let now = Instant::now();
            let should_send = last_update.get(update.id).is_none_or(|previous| {
                now.saturating_duration_since(*previous) >= PROGRESS_INTERVAL
            });
            if should_send {
                last_update.insert(update.id, now);
                let _ = tx.send(StartupEvent::Installing {
                    id: update.id,
                    done_bytes: update.done_bytes,
                    total_bytes: update.total_bytes,
                });
            }
        };
        ensure_installed(&plan.models_dir, &plan.required, fetcher, &mut progress)
    }));

    let terminal = match result {
        Ok(Ok(report)) => StartupEvent::Installed(report),
        Ok(Err(error)) => StartupEvent::Failed(format!("{error:#}")),
        Err(payload) => StartupEvent::Failed(panic_reason(payload.as_ref())),
    };
    let _ = tx.send(terminal);
}

/// Resolves the models directory in command-line, environment, portable, config order.
pub fn resolve_models_dir(
    cli: Option<PathBuf>,
    env: Option<OsString>,
    exe_dir: &Path,
    configured: &Path,
) -> PathBuf {
    if let Some(path) = cli {
        return path;
    }
    if let Some(path) = env.filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    let portable = exe_dir.join("models");
    if portable.exists() {
        portable
    } else {
        configured.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::mpsc;

    use anyhow::bail;

    use super::*;
    use crate::config::{Capabilities, Config, EngineKind};
    use crate::models::fetch::Fetcher;
    use crate::models::MODELS;

    struct FailingFetcher;

    impl Fetcher for FailingFetcher {
        fn fetch(
            &self,
            url: &str,
            _dest: &mut dyn Write,
            _progress: &mut dyn FnMut(u64),
        ) -> anyhow::Result<()> {
            bail!("fixture download failed for {url}")
        }
    }

    struct PanickingFetcher;

    impl Fetcher for PanickingFetcher {
        fn fetch(
            &self,
            _url: &str,
            _dest: &mut dyn Write,
            _progress: &mut dyn FnMut(u64),
        ) -> anyhow::Result<()> {
            panic!("fixture fetch panic")
        }
    }

    #[test]
    fn remote_plan_needs_only_effective_local_punctuation_models() {
        let mut config = Config {
            engine: EngineKind::Remote,
            ..Config::default()
        };
        let caps = Capabilities {
            punct: true,
            remote_engine: true,
        };

        assert!(StartupPlan::from_config(&config, caps).required.is_empty());

        config.remote.server_punctuates = false;
        assert_eq!(
            StartupPlan::from_config(&config, caps)
                .required
                .iter()
                .map(|model| model.id)
                .collect::<Vec<_>>(),
            ["mojicast-punct"]
        );
    }

    #[test]
    fn empty_install_report_is_warm() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            engine: EngineKind::Remote,
            models_dir: temp.path().join("models"),
            ..Config::default()
        };
        let plan = StartupPlan::from_config(
            &config,
            Capabilities {
                punct: true,
                remote_engine: true,
            },
        );
        let (tx, rx) = mpsc::sync_channel(1);

        run_startup(plan, &FailingFetcher, tx);

        let StartupEvent::Installed(report) = rx.recv().unwrap() else {
            panic!("expected installed event");
        };
        let cold = !report.installed.is_empty();
        assert!(!cold);
    }

    #[test]
    fn startup_failure_carries_the_reason_and_throttles_progress() {
        let temp = tempfile::tempdir().unwrap();
        let plan = StartupPlan {
            engine: EngineKind::Local,
            models_dir: temp.path().join("models"),
            required: vec![&MODELS[0]],
        };
        let (tx, rx) = mpsc::sync_channel(16);

        run_startup(plan, &FailingFetcher, tx);

        let events = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StartupEvent::Installing { .. }))
                .count(),
            1
        );
        let StartupEvent::Failed(reason) = events.last().unwrap() else {
            panic!("expected failed event, got {:?}", events.last());
        };
        assert!(reason.contains("fixture download failed"), "{reason}");
    }

    #[test]
    fn startup_worker_converts_panics_to_failures() {
        let temp = tempfile::tempdir().unwrap();
        let plan = StartupPlan {
            engine: EngineKind::Local,
            models_dir: temp.path().join("models"),
            required: vec![&MODELS[0]],
        };
        let (tx, rx) = mpsc::sync_channel(16);

        run_startup(plan, &PanickingFetcher, tx);

        let reason = rx
            .try_iter()
            .find_map(|event| match event {
                StartupEvent::Failed(reason) => Some(reason),
                _ => None,
            })
            .unwrap();
        assert!(reason.contains("fixture fetch panic"), "{reason}");
    }

    #[test]
    fn models_directory_precedence_checks_portable_directory_exists() {
        let temp = tempfile::tempdir().unwrap();
        let exe_dir = temp.path().join("bin");
        std::fs::create_dir(&exe_dir).unwrap();
        let configured = PathBuf::from("configured-models");

        assert_eq!(
            resolve_models_dir(
                Some(PathBuf::from("cli-models")),
                Some(OsString::from("env-models")),
                &exe_dir,
                &configured,
            ),
            PathBuf::from("cli-models")
        );
        assert_eq!(
            resolve_models_dir(
                None,
                Some(OsString::from("env-models")),
                &exe_dir,
                &configured,
            ),
            PathBuf::from("env-models")
        );
        assert_eq!(
            resolve_models_dir(None, Some(OsString::new()), &exe_dir, &configured),
            configured
        );

        std::fs::create_dir(exe_dir.join("models")).unwrap();
        assert_eq!(
            resolve_models_dir(None, None, &exe_dir, &configured),
            exe_dir.join("models")
        );
        std::fs::remove_dir(exe_dir.join("models")).unwrap();
        assert_eq!(
            resolve_models_dir(None, None, &exe_dir, &configured),
            configured
        );
    }
}
