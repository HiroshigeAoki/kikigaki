#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::sync::mpsc::{self, Receiver, SyncSender};

use kikigaki_core::models;
use kikigaki_core::startup::{run_startup, StartupEvent, StartupPlan};

use crate::fetch::ReqwestFetcher;

const EVENT_SLOTS: usize = 32;

pub(crate) struct StartupCoordinator {
    plan: StartupPlan,
    tx: SyncSender<StartupEvent>,
    rx: Receiver<StartupEvent>,
    running: bool,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl StartupCoordinator {
    pub(crate) fn new(plan: StartupPlan) -> Self {
        let (tx, rx) = mpsc::sync_channel(EVENT_SLOTS);
        Self {
            plan,
            tx,
            rx,
            running: false,
            handle: None,
        }
    }

    pub(crate) fn start(&mut self) -> anyhow::Result<bool> {
        if self.running {
            return Ok(false);
        }
        let plan = self.plan.clone();
        let tx = self.tx.clone();
        let handle = std::thread::Builder::new()
            .name("model-startup".into())
            .spawn(move || match ReqwestFetcher::new() {
                Ok(fetcher) => run_startup(plan, &fetcher, tx),
                Err(error) => {
                    let _ = tx.send(StartupEvent::Failed(format!("{error:#}")));
                }
            })?;
        self.handle = Some(handle);
        self.running = true;
        Ok(true)
    }

    pub(crate) fn try_recv(&mut self) -> Option<StartupEvent> {
        let event = self.rx.try_recv().ok()?;
        if matches!(event, StartupEvent::Installed(_) | StartupEvent::Failed(_)) {
            self.running = false;
        }
        Some(event)
    }

    pub(crate) fn invalidate_model_load_failure(&self, failed_model: Option<&'static str>) -> bool {
        let Some(id) = failed_model else {
            return false;
        };
        let Some(model) = self.plan.required.iter().find(|model| model.id == id) else {
            tracing::warn!(id, "engine reported an unknown failed model");
            return true;
        };
        models::invalidate(&self.plan.models_dir, model.id);
        true
    }

    pub(crate) fn required(&self) -> &[&'static kikigaki_core::models::Model] {
        &self.plan.required
    }

    pub(crate) fn join(&mut self, timeout: std::time::Duration) -> bool {
        let Some(handle) = self.handle.take() else {
            return true;
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(handle.join().is_ok());
        });
        done_rx.recv_timeout(timeout).unwrap_or(false)
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct StartupCoordinatorPort(StartupCoordinator);

#[cfg(target_os = "macos")]
impl StartupCoordinatorPort {
    pub(crate) fn new(plan: StartupPlan) -> Self {
        Self(StartupCoordinator::new(plan))
    }

    #[allow(dead_code)]
    pub(crate) fn required(&self) -> &[&'static kikigaki_core::models::Model] {
        self.0.required()
    }
}

#[cfg(target_os = "macos")]
impl crate::controller::StartupPort for StartupCoordinatorPort {
    fn start(&mut self) -> anyhow::Result<bool> {
        self.0.start()
    }

    fn try_recv(&mut self) -> Option<StartupEvent> {
        self.0.try_recv()
    }

    fn required(&self) -> &[&'static kikigaki_core::models::Model] {
        self.0.required()
    }

    fn invalidate_model_load_failure(&mut self, failed_model: Option<&'static str>) -> bool {
        self.0.invalidate_model_load_failure(failed_model)
    }

    fn join(&mut self, timeout: std::time::Duration) -> bool {
        self.0.join(timeout)
    }

    fn rescan_onboarding(
        &mut self,
        settings: &crate::settings::SettingsCoordinator,
    ) -> Option<crate::status::OnboardingState> {
        crate::onboarding::scan(settings, self)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kikigaki_core::config::EngineKind;
    use kikigaki_core::models::MODELS;

    use super::*;

    #[test]
    fn invalidates_only_manifest_model_load_failures() {
        let temp = tempfile::tempdir().unwrap();
        let model_dir = temp.path().join(MODELS[0].id);
        std::fs::create_dir(&model_dir).unwrap();
        std::fs::write(model_dir.join(".ok"), "marker").unwrap();
        let coordinator = StartupCoordinator::new(StartupPlan {
            engine: EngineKind::Local,
            models_dir: PathBuf::from(temp.path()),
            required: vec![&MODELS[0]],
        });

        assert!(coordinator.invalidate_model_load_failure(Some(MODELS[0].id)));
        assert!(!model_dir.join(".ok").exists());
        assert!(!coordinator.invalidate_model_load_failure(None));
    }
}
