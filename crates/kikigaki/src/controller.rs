//! Tauri-free push-to-talk controller and its injected platform ports.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use kikigaki_core::config::{Capabilities, Config, EngineKind};
use kikigaki_core::engine::{Engine, EngineCmd, EngineMsg, EnginePhase, EngineSupervisor, Waker};
use kikigaki_core::metrics;
use kikigaki_core::postprocess::{to_protocol_final, PostprocessWorker};
use kikigaki_core::protocol::{Event, SAMPLE_RATE};
use kikigaki_core::session::{Input, Output, Session, SessionConfig, State};
use kikigaki_core::startup::StartupEvent;

use crate::correction::Correction;
use crate::history::History;
use crate::learned::Learned;
use crate::settings::{SettingsCoordinator, SettingsPatch};
use crate::status::{
    AutostartState, BootstrapError, DownloadProgress, OnboardingState, Status, UiError, UiSnapshot,
    UiStatus,
};

pub enum HotkeyEdge {
    Pressed,
    Released,
}

pub enum ControllerCmd {
    /// Delivered by event producers so the run loop drains their queues without waiting for the
    /// poll tick.
    Wake,
    ApplySettings {
        patch: SettingsPatch,
        reply: ReplyTx<crate::settings::SettingsSnapshot>,
    },
    RememberCorrection {
        entry_id: u64,
        corrected: String,
        reply: ReplyTx<()>,
    },
    DeleteLearnedRule {
        id: u64,
        reply: ReplyTx<()>,
    },
    ClearHistory {
        reply: ReplyTx<()>,
    },
    RetryDownload {
        reply: ReplyTx<()>,
    },
    BeginHotkeyCapture {
        reply: ReplyTx<()>,
    },
    EndHotkeyCapture {
        new_chord: Option<String>,
        reply: ReplyTx<crate::settings::SettingsSnapshot>,
    },
    SetWindowFocused(bool),
    RescanOnboarding,
    Quit,
}

type ReplyTx<T> = SyncSender<Result<T, UiError>>;

pub trait ControllerEventSink: Send {
    fn publish(&self, snapshot: UiSnapshot);

    #[cfg(test)]
    fn record_snapshot_build(&self) {}
}

pub trait HotkeyPort: Send {
    fn register(&self, chord: &str) -> Result<(), String>;
    fn unregister(&self, chord: &str) -> Result<(), String>;
}

pub trait Paster: Send {
    fn paste(&self, text: String, deadline: Instant) -> Result<(), String>;
}

pub trait EngineFactory: Send {
    fn build(&self) -> anyhow::Result<Box<dyn Engine>>;
}

pub trait MicPort: Send {
    fn start(&mut self, sink: kikigaki_core::engine::AudioSink) -> anyhow::Result<()>;
    fn stop(&mut self);
}

pub trait PostprocessFactory: Send {
    fn build(
        &self,
        learned: std::sync::Arc<kikigaki_core::replace::Rules>,
        waker: Option<Waker>,
    ) -> anyhow::Result<PostprocessWorker>;
}

pub trait StartupPort: Send {
    fn start(&mut self) -> anyhow::Result<bool>;
    fn try_recv(&mut self) -> Option<kikigaki_core::startup::StartupEvent>;
    fn required(&self) -> &[&'static kikigaki_core::models::Model];
    fn invalidate_model_load_failure(&mut self, failed_model: Option<&'static str>) -> bool;
    fn join(&mut self, timeout: Duration) -> bool;
    fn rescan_onboarding(
        &mut self,
        settings: &SettingsCoordinator,
    ) -> Option<crate::status::OnboardingState>;
}

pub trait Clock: Send {
    fn now(&self) -> Instant;
}

pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

pub enum BootstrapOutcome {
    ReadyToStart,
    NeedsOnboarding(crate::status::OnboardingState),
    ConfigError(crate::status::BootstrapError),
}

#[cfg(any(test, target_os = "macos"))]
impl BootstrapOutcome {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::ReadyToStart => "ready_to_start",
            Self::NeedsOnboarding(_) => "needs_onboarding",
            Self::ConfigError(_) => "config_error",
        }
    }
}

#[derive(Clone)]
pub struct ControllerClient {
    tx: SyncSender<ControllerCmd>,
}

#[derive(Clone)]
pub struct HotkeySender {
    tx: Sender<HotkeyEdge>,
    client: ControllerClient,
}

const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const PASTE_TIMEOUT: Duration = Duration::from_secs(3);
const MICROPHONE_PERMISSION_DENIED: &str =
    "Microphone permission denied — System Settings › Privacy";

impl ControllerClient {
    pub fn sender_for_handle(&self) -> SyncSender<ControllerCmd> {
        self.tx.clone()
    }

    pub fn send_and_wait<T>(
        &self,
        build: impl FnOnce(ReplyTx<T>) -> ControllerCmd,
    ) -> Result<T, UiError> {
        let (reply, rx) = mpsc::sync_channel(1);
        match self.tx.try_send(build(reply)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(busy()),
            Err(TrySendError::Disconnected(_)) => return Err(controller_gone()),
        }
        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(UiError {
                code: "timeout",
                message: "内部処理がタイムアウトしました".into(),
            }),
        }
    }

    pub fn send(&self, cmd: ControllerCmd) {
        let _ = self.tx.try_send(cmd);
    }

    pub fn wake(&self) {
        let _ = self.tx.try_send(ControllerCmd::Wake);
    }
}

impl HotkeySender {
    pub fn send(&self, edge: HotkeyEdge) {
        let _ = self.tx.send(edge);
        self.client.wake();
    }
}

fn busy() -> UiError {
    UiError {
        code: "busy",
        message: "処理中です。しばらくしてからお試しください".into(),
    }
}

fn controller_gone() -> UiError {
    UiError {
        code: "controller_gone",
        message: "内部エラーが発生しました".into(),
    }
}

pub struct ControllerConfig {
    pub settings: SettingsCoordinator,
    pub capabilities: Capabilities,
    pub models_dir: PathBuf,
    pub bootstrap: BootstrapOutcome,
    pub process_started_at: Instant,
}

pub fn client_channels() -> (
    ControllerClient,
    Receiver<ControllerCmd>,
    HotkeySender,
    Receiver<HotkeyEdge>,
) {
    let (tx, rx) = mpsc::sync_channel(64);
    let (hotkey_tx, hotkey_rx) = mpsc::channel();
    let client = ControllerClient { tx };
    let hotkey = HotkeySender {
        tx: hotkey_tx,
        client: client.clone(),
    };
    (client, rx, hotkey, hotkey_rx)
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    cfg: ControllerConfig,
    rx: Receiver<ControllerCmd>,
    hotkey_rx: Receiver<HotkeyEdge>,
    tx: SyncSender<ControllerCmd>,
    sink: Box<dyn ControllerEventSink>,
    hotkeys: Box<dyn HotkeyPort>,
    paster: Box<dyn Paster>,
    engine_factory: Box<dyn EngineFactory>,
    mic: Box<dyn MicPort>,
    postprocess_factory: Box<dyn PostprocessFactory>,
    startup: Box<dyn StartupPort>,
    clock: Box<dyn Clock>,
) -> (ControllerHandle, Receiver<()>) {
    spawn_with_poll_interval(
        cfg,
        rx,
        hotkey_rx,
        tx,
        sink,
        hotkeys,
        paster,
        engine_factory,
        mic,
        postprocess_factory,
        startup,
        clock,
        POLL_INTERVAL,
    )
}

#[allow(clippy::too_many_arguments)]
fn spawn_with_poll_interval(
    cfg: ControllerConfig,
    rx: Receiver<ControllerCmd>,
    hotkey_rx: Receiver<HotkeyEdge>,
    tx: SyncSender<ControllerCmd>,
    sink: Box<dyn ControllerEventSink>,
    hotkeys: Box<dyn HotkeyPort>,
    paster: Box<dyn Paster>,
    engine_factory: Box<dyn EngineFactory>,
    mic: Box<dyn MicPort>,
    postprocess_factory: Box<dyn PostprocessFactory>,
    startup: Box<dyn StartupPort>,
    clock: Box<dyn Clock>,
    poll_interval: Duration,
) -> (ControllerHandle, Receiver<()>) {
    let (exited_tx, exited_rx) = mpsc::channel();
    let controller_client = ControllerClient { tx: tx.clone() };
    let join = std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || {
            run(
                cfg,
                rx,
                hotkey_rx,
                sink,
                hotkeys,
                paster,
                engine_factory,
                mic,
                postprocess_factory,
                startup,
                clock,
                controller_client,
                poll_interval,
            );
            tracing::info!("controller stopped");
            drop(exited_tx);
        })
        .expect("spawn controller thread");
    (
        ControllerHandle {
            tx,
            join: Some(join),
        },
        exited_rx,
    )
}

pub struct ControllerHandle {
    tx: SyncSender<ControllerCmd>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ControllerHandle {
    pub fn request_quit(&self) {
        let _ = self.tx.try_send(ControllerCmd::Quit);
    }

    pub fn join(mut self, timeout: Duration) -> bool {
        let Some(handle) = self.join.take() else {
            return true;
        };
        let (done_tx, done_rx) = mpsc::channel();
        let watcher = std::thread::Builder::new()
            .name("controller-join-watchdog".into())
            .spawn(move || {
                let _ = done_tx.send(handle.join().is_ok());
            })
            .expect("spawn controller join watchdog");
        let joined = done_rx.recv_timeout(timeout).unwrap_or(false);
        drop(watcher);
        joined
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    cfg: ControllerConfig,
    rx: Receiver<ControllerCmd>,
    hotkey_rx: Receiver<HotkeyEdge>,
    sink: Box<dyn ControllerEventSink>,
    hotkeys: Box<dyn HotkeyPort>,
    paster: Box<dyn Paster>,
    engine_factory: Box<dyn EngineFactory>,
    mic: Box<dyn MicPort>,
    postprocess_factory: Box<dyn PostprocessFactory>,
    startup: Box<dyn StartupPort>,
    clock: Box<dyn Clock>,
    controller_client: ControllerClient,
    poll_interval: Duration,
) {
    let ControllerConfig {
        settings,
        capabilities: _,
        models_dir: _,
        bootstrap,
        process_started_at,
    } = cfg;
    let session = Session::new(SessionConfig {
        silence_pad_ms: settings.config().silence_pad_ms,
        final_timeout: Duration::from_millis(settings.config().final_timeout_ms),
    });
    let learned_path = settings.config().learned_file.clone();
    let (onboarding, bootstrap_error, auto_start) = match bootstrap {
        BootstrapOutcome::ReadyToStart => {
            tracing::info!(outcome = "ready_to_start", "bootstrap outcome applied");
            (None, None, true)
        }
        BootstrapOutcome::NeedsOnboarding(state) => {
            tracing::info!(outcome = "needs_onboarding", "bootstrap outcome applied");
            (Some(state), None, false)
        }
        BootstrapOutcome::ConfigError(error) => {
            tracing::info!(
                outcome = "config_error",
                code = error.code,
                "bootstrap outcome applied"
            );
            (None, Some(error), false)
        }
    };
    let (learned, initial_status) = match Learned::load(learned_path.clone()) {
        Ok(learned) => (learned, Status::Ok),
        Err(_error) => {
            tracing::warn!("failed to load learned rules");
            (
                Learned::empty(learned_path),
                Status::Warning("学習済みの置き換えを読み込めませんでした".into()),
            )
        }
    };
    let waker: Waker = Arc::new(move || controller_client.wake());
    let supervisor = EngineSupervisor::new(Box::new(move || engine_factory.build()))
        .with_waker(Arc::clone(&waker));
    let mut controller = Controller {
        settings,
        session,
        mic,
        supervisor: Some(supervisor),
        startup,
        postprocess_factory,
        waker,
        postprocess: None,
        sink,
        hotkeys,
        paster,
        clock,
        history: History::default(),
        learned,
        status: initial_status,
        onboarding,
        bootstrap_error,
        download_in_flight: false,
        window_focused: false,
        capturing_hotkey: false,
        last_snapshot: None,
        dirty: true,
        revision: 0,
        process_started_at,
        startup_cold: false,
        first_ready: true,
        pending_finals: HashMap::new(),
        active_generation: 0,
        shutting_down: false,
    };

    if auto_start {
        match controller.startup.start() {
            Ok(started) => controller.download_in_flight = started,
            Err(error) => {
                controller.status = Status::StartupFailed(format!("Startup failed — {error}"));
            }
        }
    }
    controller.publish_if_changed();

    let mut outputs = Vec::new();
    loop {
        match rx.recv_timeout(poll_interval) {
            Ok(ControllerCmd::Quit) => {
                controller.begin_shutdown();
                break;
            }
            Ok(command) => controller.handle_command(command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                controller.begin_shutdown();
                break;
            }
        }
        while let Ok(edge) = hotkey_rx.try_recv() {
            controller.handle_hotkey_edge(edge, &mut outputs);
        }
        controller.handle_startup_events(&mut outputs);
        controller.handle_postprocess_events(&mut outputs);
        controller.handle_engine_messages(&mut outputs);
        outputs.extend(
            controller
                .session
                .handle(Input::Tick, controller.clock.now()),
        );
        controller.execute_outputs(std::mem::take(&mut outputs));
        controller.publish_if_changed();
    }
}

struct Controller {
    settings: SettingsCoordinator,
    session: Session,
    mic: Box<dyn MicPort>,
    supervisor: Option<EngineSupervisor>,
    startup: Box<dyn StartupPort>,
    postprocess_factory: Box<dyn PostprocessFactory>,
    waker: Waker,
    postprocess: Option<PostprocessWorker>,
    sink: Box<dyn ControllerEventSink>,
    hotkeys: Box<dyn HotkeyPort>,
    paster: Box<dyn Paster>,
    clock: Box<dyn Clock>,
    history: History,
    learned: Learned,
    status: Status,
    onboarding: Option<OnboardingState>,
    bootstrap_error: Option<BootstrapError>,
    download_in_flight: bool,
    window_focused: bool,
    capturing_hotkey: bool,
    last_snapshot: Option<UiSnapshot>,
    dirty: bool,
    revision: u64,
    process_started_at: Instant,
    startup_cold: bool,
    first_ready: bool,
    pending_finals: HashMap<u64, (String, String)>,
    active_generation: u64,
    shutting_down: bool,
}

impl Controller {
    fn handle_command(&mut self, command: ControllerCmd) {
        if matches!(command, ControllerCmd::Wake) {
            return;
        }
        self.dirty = true;
        match command {
            ControllerCmd::Wake => {}
            ControllerCmd::ApplySettings { patch, reply } => {
                let result = self.settings.apply(patch);
                if let Ok(snapshot) = &result {
                    if let Some(worker) = self.postprocess.as_ref() {
                        if let Err(error) =
                            worker.configure(snapshot.punct_enabled, snapshot.strip_trailing_period)
                        {
                            tracing::warn!(%error, "configure post-processing");
                        }
                    }
                }
                let _ = reply.send(result);
            }
            ControllerCmd::RememberCorrection {
                entry_id,
                corrected,
                reply,
            } => {
                let result = self.remember_correction(entry_id, corrected);
                let _ = reply.send(result);
            }
            ControllerCmd::DeleteLearnedRule { id, reply } => {
                let result = self
                    .learned
                    .delete(id)
                    .map_err(|error| persistence_error("置き換えを削除できませんでした", error))
                    .map(|rules| {
                        self.reload_rules(rules);
                    });
                let _ = reply.send(result);
            }
            ControllerCmd::ClearHistory { reply } => {
                self.history.clear();
                let _ = reply.send(Ok(()));
            }
            ControllerCmd::RetryDownload { reply } => {
                let result = if self.download_in_flight {
                    Err(UiError {
                        code: "already_downloading",
                        message: "ダウンロード中です".into(),
                    })
                } else {
                    match self.startup.start() {
                        Ok(true) => {
                            self.download_in_flight = true;
                            Ok(())
                        }
                        Ok(false) => Err(UiError {
                            code: "already_downloading",
                            message: "ダウンロード中です".into(),
                        }),
                        Err(error) => Err(persistence_error(
                            "ダウンロードを開始できませんでした",
                            error,
                        )),
                    }
                };
                let _ = reply.send(result);
            }
            ControllerCmd::BeginHotkeyCapture { reply } => {
                if self.session.state() != State::Idle {
                    let _ = reply.send(Err(UiError {
                        code: "not_idle",
                        message: "聞き取り中はホットキーを変更できません".into(),
                    }));
                    return;
                }
                let old = self.settings.config().hotkey.clone();
                match self.hotkeys.unregister(&old) {
                    Ok(()) => {
                        self.capturing_hotkey = true;
                        let _ = reply.send(Ok(()));
                    }
                    Err(message) => {
                        let _ = reply.send(Err(UiError {
                            code: "register_failed",
                            message,
                        }));
                    }
                }
            }
            ControllerCmd::EndHotkeyCapture { new_chord, reply } => {
                self.end_hotkey_capture(new_chord, reply);
            }
            ControllerCmd::SetWindowFocused(focused) => self.window_focused = focused,
            ControllerCmd::RescanOnboarding => {
                let was_onboarding = self.onboarding.is_some();
                let previous_download = self
                    .onboarding
                    .as_ref()
                    .and_then(|state| state.download.clone());
                let mut rescanned = self.startup.rescan_onboarding(&self.settings);
                if let Some(state) = rescanned.as_mut().filter(|state| !state.models_installed) {
                    state.download = previous_download;
                }
                self.onboarding = rescanned;
                let engine_has_started = self
                    .supervisor
                    .as_ref()
                    .and_then(EngineSupervisor::sink)
                    .is_some();
                if was_onboarding
                    && self.onboarding.is_none()
                    && !engine_has_started
                    && !self.download_in_flight
                {
                    match self.startup.start() {
                        Ok(started) => self.download_in_flight = started,
                        Err(error) => {
                            self.status =
                                Status::StartupFailed(format!("Startup failed — {error}"));
                        }
                    }
                }
                self.publish_if_changed();
            }
            ControllerCmd::Quit => self.begin_shutdown(),
        }
    }

    fn remember_correction(&mut self, entry_id: u64, corrected: String) -> Result<(), UiError> {
        let raw = self
            .history
            .get(entry_id)
            .map(|entry| entry.raw.clone())
            .ok_or_else(|| UiError {
                code: "not_found",
                message: "履歴が見つかりません".into(),
            })?;
        let (from, to) = match crate::correction::diff(&raw, &corrected) {
            Correction::Word { from, to } | Correction::Sentence { from, to } => (from, to),
            Correction::None { message } => {
                return Err(UiError {
                    code: "nothing_to_learn",
                    message,
                })
            }
        };
        let rules = self
            .learned
            .remember(vec![from], to)
            .map_err(|error| persistence_error("置き換えを保存できませんでした", error))?;
        self.reload_rules(rules);
        Ok(())
    }

    fn reload_rules(&mut self, rules: std::sync::Arc<kikigaki_core::replace::Rules>) {
        if let Some(worker) = self.postprocess.as_ref() {
            if let Err(error) = worker.reload_rules(rules) {
                tracing::warn!(%error, "reload learned rules");
            }
        }
    }

    fn reregister_or_warn(&mut self, chord: &str) -> Result<(), UiError> {
        self.hotkeys.register(chord).map_err(|message| {
            self.status = Status::Warning("ホットキーが無効 — 再設定してください".into());
            UiError {
                code: "register_failed",
                message,
            }
        })
    }

    fn end_hotkey_capture(
        &mut self,
        new_chord: Option<String>,
        reply: ReplyTx<crate::settings::SettingsSnapshot>,
    ) {
        let was_capturing = self.capturing_hotkey;
        self.capturing_hotkey = false;
        let old = self.settings.config().hotkey.clone();
        let Some(chord) = new_chord else {
            let result = self.reregister_or_warn(&old);
            let _ = reply.send(result.map(|()| self.settings.snapshot()));
            return;
        };
        if chord == old {
            let result = if was_capturing {
                self.reregister_or_warn(&old)
            } else {
                Ok(())
            };
            let _ = reply.send(result.map(|()| self.settings.snapshot()));
            return;
        }
        if let Err(message) = self.hotkeys.register(&chord) {
            let _ = self.reregister_or_warn(&old);
            let _ = reply.send(Err(UiError {
                code: "register_failed",
                message,
            }));
            return;
        }
        let _ = self.hotkeys.unregister(&old);
        match self.settings.apply(SettingsPatch {
            hotkey: Some(chord.clone()),
            punctuation: None,
            builtin_replace_dict: None,
        }) {
            Ok(snapshot) => {
                tracing::info!(chord = %chord, "hotkey re-registered after settings change");
                let _ = reply.send(Ok(snapshot));
            }
            Err(error) => {
                let _ = self.hotkeys.unregister(&chord);
                let _ = self.reregister_or_warn(&old);
                let _ = reply.send(Err(error));
            }
        }
    }

    fn handle_hotkey_edge(&mut self, edge: HotkeyEdge, outputs: &mut Vec<Output>) {
        if self.window_focused || self.capturing_hotkey {
            return;
        }
        self.dirty = true;
        let input = match edge {
            HotkeyEdge::Pressed => Input::Pressed,
            HotkeyEdge::Released => Input::Released,
        };
        outputs.extend(self.session.handle(input, self.clock.now()));
    }

    fn handle_startup_events(&mut self, outputs: &mut Vec<Output>) {
        while let Some(event) = self.startup.try_recv() {
            self.dirty = true;
            match event {
                StartupEvent::Installing {
                    id,
                    done_bytes,
                    total_bytes,
                } => {
                    self.status = Status::Installing(progress_message(id, done_bytes, total_bytes));
                    let required = self.startup.required();
                    let total = u32::try_from(required.len()).unwrap_or(u32::MAX);
                    let done = required
                        .iter()
                        .position(|model| model.id == id)
                        .and_then(|index| u32::try_from(index).ok())
                        .unwrap_or(0)
                        .saturating_add(u32::from(total_bytes > 0 && done_bytes >= total_bytes))
                        .min(total);
                    if let Some(onboarding) = self.onboarding.as_mut() {
                        onboarding.download = Some(DownloadProgress {
                            model: id.into(),
                            done,
                            total,
                            bytes: done_bytes,
                            total_bytes,
                            failed: false,
                        });
                    }
                }
                StartupEvent::Installed(report) => {
                    self.startup_cold = !report.installed.is_empty();
                    self.download_in_flight = false;
                    self.status = Status::Ok;
                    if let Some(onboarding) = self.onboarding.as_mut() {
                        onboarding.models_installed = true;
                        onboarding.download = None;
                    }
                    let engine_started = self.supervisor.as_mut().is_some_and(|supervisor| {
                        match supervisor.start() {
                            Ok(()) => true,
                            Err(error) => {
                                tracing::error!(error = %format!("{error:#}"), "failed to start transcription engine");
                                self.status =
                                    Status::StartupFailed(format!("Engine start failed — {error}"));
                                false
                            }
                        }
                    });
                    if !engine_started {
                        outputs.extend(
                            self.session
                                .handle(Input::EngineDisconnected, self.clock.now()),
                        );
                        continue;
                    }
                    if self.postprocess.is_none() {
                        match self
                            .postprocess_factory
                            .build(self.learned.as_core_rules(), Some(Arc::clone(&self.waker)))
                        {
                            Ok(worker) => {
                                let settings = self.settings.snapshot();
                                if let Err(error) = worker.configure(
                                    settings.punct_enabled,
                                    settings.strip_trailing_period,
                                ) {
                                    tracing::warn!(%error, "configure new post-processing worker");
                                }
                                self.postprocess = Some(worker);
                            }
                            Err(error) => {
                                tracing::error!(error = %format!("{error:#}"), "failed to start post-processing");
                                self.status = Status::StartupFailed(format!(
                                    "Post-processing start failed — {error}"
                                ));
                                if let Some(supervisor) = self.supervisor.as_mut() {
                                    supervisor.shutdown();
                                }
                                outputs.extend(
                                    self.session
                                        .handle(Input::EngineDisconnected, self.clock.now()),
                                );
                            }
                        }
                    }
                }
                StartupEvent::Failed(reason) => {
                    self.download_in_flight = false;
                    self.status = Status::StartupFailed(reason.clone());
                    let required = self.startup.required();
                    let fallback_model = required
                        .first()
                        .map_or_else(|| "models".to_owned(), |model| model.id.to_owned());
                    let total = u32::try_from(required.len()).unwrap_or(u32::MAX);
                    if let Some(onboarding) = self.onboarding.as_mut() {
                        match onboarding.download.as_mut() {
                            Some(download) => download.failed = true,
                            None => {
                                onboarding.download = Some(DownloadProgress {
                                    model: fallback_model,
                                    done: 0,
                                    total,
                                    bytes: 0,
                                    total_bytes: 0,
                                    failed: true,
                                });
                            }
                        }
                    }
                    outputs.extend(
                        self.session
                            .handle(Input::EngineDisconnected, self.clock.now()),
                    );
                }
            }
        }
    }

    fn handle_postprocess_events(&mut self, outputs: &mut Vec<Output>) {
        loop {
            let processed = self
                .postprocess
                .as_ref()
                .and_then(PostprocessWorker::try_recv);
            let Some(processed) = processed else {
                break;
            };
            self.dirty = true;
            self.pending_finals.insert(
                processed.gen,
                (processed.raw.clone(), processed.text.clone()),
            );
            self.pending_finals
                .retain(|generation, _| *generation >= self.active_generation);
            outputs.extend(self.session.handle(
                Input::Engine(Event::Final(to_protocol_final(&processed))),
                self.clock.now(),
            ));
        }
    }

    fn handle_engine_messages(&mut self, outputs: &mut Vec<Output>) {
        loop {
            let message = self
                .supervisor
                .as_mut()
                .and_then(EngineSupervisor::try_recv);
            let Some(message) = message else {
                break;
            };
            self.dirty = true;
            let input = match message {
                EngineMsg::Ready => {
                    let process_to_ready_ms = self
                        .clock
                        .now()
                        .saturating_duration_since(self.process_started_at)
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX);
                    tracing::info!(process_to_ready_ms, "engine ready");
                    if self.first_ready {
                        append_metric(
                            self.settings.config(),
                            metrics::MetricRow::Startup(metrics::Startup {
                                ts: metric_timestamp(),
                                process_to_ready_ms,
                                cold: self.startup_cold,
                            }),
                        );
                        self.first_ready = false;
                    }
                    self.status = Status::Ok;
                    Input::Engine(Event::Ready { sr: SAMPLE_RATE })
                }
                final_message @ EngineMsg::Final { gen, .. } => {
                    if let Some(worker) = self.postprocess.as_ref() {
                        if let Err(error) = worker.submit(gen, final_message) {
                            tracing::error!(%error, gen, "submit final for post-processing");
                        }
                    } else {
                        tracing::error!(gen, "post-processing is unavailable");
                    }
                    continue;
                }
                EngineMsg::Disconnected {
                    reason,
                    failed_model,
                } => {
                    tracing::info!(failed_model, "engine disconnected");
                    let startup_failed = failed_model.is_some()
                        && self.startup.invalidate_model_load_failure(failed_model);
                    self.status = if startup_failed {
                        Status::StartupFailed(reason)
                    } else {
                        Status::EngineFailed(reason)
                    };
                    Input::EngineDisconnected
                }
            };
            outputs.extend(self.session.handle(input, self.clock.now()));
        }
    }

    fn execute_outputs(&mut self, outputs: Vec<Output>) {
        let mut pending = VecDeque::from(outputs);
        while let Some(output) = pending.pop_front() {
            self.dirty = true;
            match output {
                Output::Begin { gen } => {
                    self.active_generation = gen;
                    if let Some(supervisor) = self.supervisor.as_ref() {
                        send_command(supervisor, EngineCmd::Begin { gen });
                    }
                }
                Output::StartMic => {
                    let sink = self.supervisor.as_ref().and_then(EngineSupervisor::sink);
                    if let Some(sink) = sink {
                        let permission_denied = microphone_permission_denied();
                        if permission_denied {
                            self.status = Status::Warning(MICROPHONE_PERMISSION_DENIED.into());
                        }
                        if let Err(error) = self.mic.start(sink) {
                            self.status =
                                Status::Warning(format!("Microphone unavailable — {error}"));
                        } else if !permission_denied {
                            self.status.clear_transient();
                        }
                    }
                }
                Output::StopMic => self.mic.stop(),
                Output::EndUtterance { gen, pad_ms } => {
                    if let Some(supervisor) = self.supervisor.as_ref() {
                        send_command(supervisor, EngineCmd::End { gen, pad_ms });
                    }
                }
                Output::Cancel { gen } => {
                    if let Some(supervisor) = self.supervisor.as_ref() {
                        send_command(supervisor, EngineCmd::Cancel { gen });
                    }
                    self.pending_finals.remove(&gen);
                }
                Output::Paste(text) => {
                    let text_len = text.chars().count();
                    let deadline = self.clock.now() + PASTE_TIMEOUT;
                    match self.paster.paste(text, deadline) {
                        Ok(()) => {
                            tracing::info!(outcome = "ok", text_len, "paste completed");
                            self.status.clear_transient();
                            if let Some((raw, text)) =
                                self.pending_finals.remove(&self.active_generation)
                            {
                                self.history.push(raw, text, Utc::now());
                            }
                            pending.extend(self.session.pasted(self.clock.now()));
                        }
                        Err(error) => {
                            let outcome = if self.clock.now() > deadline
                                || error.contains("deadline")
                                || error.contains("timed out")
                                || error.contains("did not reply")
                            {
                                "timeout"
                            } else {
                                "error"
                            };
                            tracing::info!(outcome, text_len, "paste completed");
                            tracing::error!(%error, "paste failed");
                            self.status = Status::Warning(
                                "Accessibility permission denied — System Settings › Privacy"
                                    .into(),
                            );
                            self.pending_finals.remove(&self.active_generation);
                            pending.extend(self.session.paste_failed(self.clock.now()));
                        }
                    }
                }
                Output::Record(mut row) => {
                    row.ts = metric_timestamp();
                    append_metric(self.settings.config(), metrics::MetricRow::Utterance(row));
                }
                Output::SetState(_) => {}
                Output::Reconnect => {
                    if self.status.is_startup_failed() {
                        self.status = Status::Ok;
                        match self.startup.start() {
                            Ok(started) => self.download_in_flight = started,
                            Err(error) => {
                                self.status =
                                    Status::StartupFailed(format!("Startup failed — {error}"));
                            }
                        }
                    } else if let Some(supervisor) = self.supervisor.as_mut() {
                        match supervisor.restart() {
                            Ok(_) => {}
                            Err(error) => {
                                self.status = Status::EngineFailed(format!(
                                    "Engine restart failed — {error}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    fn snapshot(&self) -> UiSnapshot {
        #[cfg(test)]
        self.sink.record_snapshot_build();
        let settings = self.settings.snapshot();
        let phase = self
            .supervisor
            .as_ref()
            .map_or(EnginePhase::Failed, EngineSupervisor::phase);
        UiSnapshot {
            revision: self.revision,
            status: UiStatus {
                state: state_name(self.session.state()),
                phase: phase_name(self.status.phase(phase)),
                message: self.status.message().map(str::to_owned),
                hotkey: settings.hotkey.clone(),
                punct_enabled: settings.punct_enabled,
                strip_trailing_period: settings.strip_trailing_period,
                engine: match self.settings.config().engine {
                    EngineKind::Local => "local",
                    EngineKind::Remote => "remote",
                },
            },
            settings,
            // The platform shell replaces this Tauri-free placeholder before publishing.
            autostart: AutostartState {
                enabled: false,
                available: false,
            },
            onboarding: self.onboarding.clone(),
            history: self.history.list(),
            learned_rules: self.learned.list().to_vec(),
            bootstrap_error: self.bootstrap_error.clone(),
        }
    }

    fn publish_if_changed(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let mut snapshot = self.snapshot();
        if self.last_snapshot.as_ref() == Some(&snapshot) {
            return;
        }
        self.revision = self.revision.wrapping_add(1);
        snapshot.revision = self.revision;
        self.last_snapshot = Some(snapshot.clone());
        self.sink.publish(snapshot);
    }

    fn begin_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.dirty = true;
        self.mic.stop();
        self.postprocess.take();
        self.supervisor.take();
        if !self.startup.join(Duration::from_millis(500)) {
            tracing::warn!("startup worker did not join before shutdown deadline");
        }
        self.publish_if_changed();
    }
}

fn persistence_error(prefix: &str, error: anyhow::Error) -> UiError {
    UiError {
        code: "write_failed",
        message: format!("{prefix}: {error:#}"),
    }
}

fn progress_message(id: &str, done_bytes: u64, total_bytes: u64) -> String {
    let percent = if total_bytes == 0 {
        0
    } else {
        done_bytes.saturating_mul(100) / total_bytes
    }
    .min(100);
    format!("Downloading {id} … {percent}%")
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Idle => "idle",
        State::Recording => "recording",
        State::Finalizing => "finalizing",
        State::Disconnected => "disconnected",
    }
}

fn phase_name(phase: EnginePhase) -> &'static str {
    match phase {
        EnginePhase::Starting => "starting",
        EnginePhase::Ready => "ready",
        EnginePhase::Failed => "failed",
    }
}

fn metric_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn append_metric(config: &Config, row: metrics::MetricRow) {
    let line = metrics::to_line(&row);
    if metrics::append_line(&config.metrics_path, &line).is_err() {
        tracing::error!("write latency metric");
    }
}

fn send_command(supervisor: &EngineSupervisor, command: EngineCmd) {
    if let Some(sink) = supervisor.sink() {
        if let Err(error) = sink.send(command) {
            tracing::warn!(%error, "failed to send engine control command");
        }
    }
}

#[cfg(target_os = "macos")]
fn microphone_permission_denied() -> bool {
    !crate::permissions::microphone_permission().is_granted()
}

#[cfg(not(target_os = "macos"))]
fn microphone_permission_denied() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use kikigaki_core::engine::{self, AudioSink, EngineCmd, EngineMsg};
    use kikigaki_core::postprocess::{Pipeline, Punctuator};
    use kikigaki_core::replace::ReplaceFile;

    use super::*;
    use crate::settings::{PunctSetting, SettingsSnapshot};
    use crate::status::{BootstrapError, OnboardingState};

    #[test]
    fn bootstrap_outcomes_have_stable_log_labels() {
        assert_eq!(BootstrapOutcome::ReadyToStart.label(), "ready_to_start");
        assert_eq!(
            BootstrapOutcome::NeedsOnboarding(OnboardingState {
                microphone: "granted",
                accessibility_trusted: true,
                models_installed: true,
                download: None,
                consent_copy: String::new(),
            })
            .label(),
            "needs_onboarding"
        );
        assert_eq!(
            BootstrapOutcome::ConfigError(BootstrapError {
                code: "config_error",
                message: "broken".into(),
            })
            .label(),
            "config_error"
        );
    }

    #[derive(Clone, Default)]
    struct SinkState(Arc<Mutex<Vec<UiSnapshot>>>, Arc<AtomicUsize>);

    struct FakeSink(SinkState);

    impl ControllerEventSink for FakeSink {
        fn publish(&self, snapshot: UiSnapshot) {
            self.0 .0.lock().unwrap().push(snapshot);
        }

        fn record_snapshot_build(&self) {
            self.0 .1.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[derive(Default)]
    struct HotkeyState {
        plan: Mutex<VecDeque<Result<(), String>>>,
        registered: Mutex<Vec<String>>,
        unregistered: Mutex<Vec<String>>,
    }

    struct ScriptedHotkeys(Arc<HotkeyState>);

    impl HotkeyPort for ScriptedHotkeys {
        fn register(&self, chord: &str) -> Result<(), String> {
            self.0.registered.lock().unwrap().push(chord.into());
            self.0.plan.lock().unwrap().pop_front().unwrap_or(Ok(()))
        }

        fn unregister(&self, chord: &str) -> Result<(), String> {
            self.0.unregistered.lock().unwrap().push(chord.into());
            Ok(())
        }
    }

    #[derive(Default)]
    struct PasteState {
        attempts: Mutex<Vec<String>>,
        reject_late: bool,
    }

    struct FakePaster(Arc<PasteState>);

    impl Paster for FakePaster {
        fn paste(&self, text: String, deadline: Instant) -> Result<(), String> {
            self.0.attempts.lock().unwrap().push(text);
            if self.0.reject_late && Instant::now() > deadline {
                return Err("paste attempted after its deadline".into());
            }
            Ok(())
        }
    }

    struct TestEngine {
        sink: AudioSink,
        _commands: Receiver<EngineCmd>,
        events: Receiver<EngineMsg>,
    }

    impl Engine for TestEngine {
        fn sink(&self) -> AudioSink {
            self.sink.clone()
        }

        fn events(&mut self) -> &mut Receiver<EngineMsg> {
            &mut self.events
        }

        fn shutdown(self: Box<Self>) {
            let _ = self.sink.send(EngineCmd::Shutdown);
        }
    }

    struct FakeEngineFactory {
        events: Mutex<Option<Receiver<EngineMsg>>>,
        ready: SyncSender<EngineMsg>,
    }

    impl EngineFactory for FakeEngineFactory {
        fn build(&self) -> anyhow::Result<Box<dyn Engine>> {
            let events = self.events.lock().unwrap().take().unwrap();
            let (sink, commands) = engine::channel();
            self.ready.send(EngineMsg::Ready).unwrap();
            Ok(Box::new(TestEngine {
                sink,
                _commands: commands,
                events,
            }))
        }
    }

    struct FakeMic(Arc<Mutex<usize>>);

    impl MicPort for FakeMic {
        fn start(&mut self, _sink: AudioSink) -> anyhow::Result<()> {
            *self.0.lock().unwrap() += 1;
            Ok(())
        }

        fn stop(&mut self) {}
    }

    struct PeriodPunctuator;

    impl Punctuator for PeriodPunctuator {
        fn punctuate(&mut self, text: &str) -> anyhow::Result<String> {
            Ok(format!("{text}。"))
        }
    }

    struct BuildGate {
        entered: SyncSender<()>,
        release: Receiver<()>,
    }

    struct FakePostprocessFactory {
        replace_file: PathBuf,
        gate: Mutex<Option<BuildGate>>,
    }

    impl PostprocessFactory for FakePostprocessFactory {
        fn build(
            &self,
            learned: Arc<kikigaki_core::replace::Rules>,
            waker: Option<Waker>,
        ) -> anyhow::Result<PostprocessWorker> {
            if let Some(gate) = self.gate.lock().unwrap().take() {
                gate.entered.send(()).unwrap();
                gate.release.recv().unwrap();
            }
            Ok(PostprocessWorker::spawn_with_waker(
                Pipeline::new(
                    ReplaceFile::new(self.replace_file.clone()),
                    Box::new(PeriodPunctuator),
                    false,
                    false,
                    Arc::new(kikigaki_core::replace::Rules::default()),
                    false,
                    learned,
                ),
                waker,
            ))
        }
    }

    #[derive(Default)]
    struct StartupState {
        invalidated: Mutex<Vec<String>>,
        invalidation_results: Mutex<VecDeque<bool>>,
        events: Mutex<VecDeque<StartupEvent>>,
        hold_starts: AtomicBool,
        rescans: AtomicUsize,
        starts: AtomicUsize,
    }

    struct FakeStartup {
        rescans: VecDeque<Option<OnboardingState>>,
        required: Vec<&'static kikigaki_core::models::Model>,
        state: Arc<StartupState>,
    }

    impl StartupPort for FakeStartup {
        fn start(&mut self) -> anyhow::Result<bool> {
            self.state.starts.fetch_add(1, Ordering::Relaxed);
            if !self.state.hold_starts.load(Ordering::Relaxed) {
                self.state
                    .events
                    .lock()
                    .unwrap()
                    .push_back(StartupEvent::Installed(
                        kikigaki_core::startup::InstallReport {
                            installed: Vec::new(),
                            reused: Vec::new(),
                        },
                    ));
            }
            Ok(true)
        }

        fn try_recv(&mut self) -> Option<kikigaki_core::startup::StartupEvent> {
            self.state.events.lock().unwrap().pop_front()
        }

        fn required(&self) -> &[&'static kikigaki_core::models::Model] {
            &self.required
        }

        fn invalidate_model_load_failure(&mut self, failed_model: Option<&'static str>) -> bool {
            if let Some(id) = failed_model {
                self.state.invalidated.lock().unwrap().push(id.into());
            }
            self.state
                .invalidation_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(true)
        }

        fn join(&mut self, _timeout: Duration) -> bool {
            true
        }

        fn rescan_onboarding(
            &mut self,
            _settings: &SettingsCoordinator,
        ) -> Option<OnboardingState> {
            self.state.rescans.fetch_add(1, Ordering::Relaxed);
            self.rescans.pop_front().flatten()
        }
    }

    struct FakeClock(Option<Instant>);

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.0.unwrap_or_else(Instant::now)
        }
    }

    struct Harness {
        _temp: tempfile::TempDir,
        client: ControllerClient,
        hotkey: HotkeySender,
        handle: Option<ControllerHandle>,
        snapshots: SinkState,
        hotkeys: Arc<HotkeyState>,
        pastes: Arc<PasteState>,
        engine_tx: SyncSender<EngineMsg>,
        mic_starts: Arc<Mutex<usize>>,
        metrics_path: PathBuf,
        startup: Arc<StartupState>,
    }

    impl Harness {
        fn stop(&mut self) -> bool {
            let handle = self.handle.take().unwrap();
            handle.request_quit();
            handle.join(Duration::from_secs(5))
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                handle.request_quit();
                let _ = handle.join(Duration::from_secs(5));
            }
        }
    }

    fn onboarding(models_installed: bool, consent_copy: &str) -> OnboardingState {
        OnboardingState {
            microphone: "authorized",
            accessibility_trusted: true,
            models_installed,
            download: None,
            consent_copy: consent_copy.into(),
        }
    }

    fn harness(
        punct: bool,
        bootstrap: BootstrapOutcome,
        hotkey_plan: Vec<Result<(), String>>,
        rescans: Vec<Option<OnboardingState>>,
        reject_late_paste: bool,
        clock: Option<Instant>,
        gate: Option<BuildGate>,
    ) -> Harness {
        harness_with_poll_interval(
            punct,
            bootstrap,
            hotkey_plan,
            rescans,
            reject_late_paste,
            clock,
            gate,
            POLL_INTERVAL,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn harness_with_poll_interval(
        punct: bool,
        bootstrap: BootstrapOutcome,
        hotkey_plan: Vec<Result<(), String>>,
        rescans: Vec<Option<OnboardingState>>,
        reject_late_paste: bool,
        clock: Option<Instant>,
        gate: Option<BuildGate>,
        poll_interval: Duration,
    ) -> Harness {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        let replace_file = temp.path().join("replace.toml");
        let learned_file = temp.path().join("learned.toml");
        let metrics_path = temp.path().join("metrics.jsonl");
        let models_dir = temp.path().join("models");
        std::fs::write(&replace_file, "").unwrap();
        std::fs::write(
            &config_path,
            format!(
                "hotkey = \"Alt+Space\"\nstrip_trailing_period = false\nreplace_file = {:?}\nlearned_file = {:?}\nmetrics_path = {:?}\nmodels_dir = {:?}\n[punct]\nenabled = {}\n",
                replace_file, learned_file, metrics_path, models_dir, punct
            ),
        )
        .unwrap();
        let capabilities = Capabilities {
            punct: true,
            remote_engine: true,
        };
        let settings = SettingsCoordinator::load(config_path, capabilities).unwrap();
        let (client, rx, hotkey, hotkey_rx) = client_channels();
        let tx = client.tx.clone();
        let snapshots = SinkState::default();
        let hotkeys = Arc::new(HotkeyState {
            plan: Mutex::new(hotkey_plan.into()),
            ..HotkeyState::default()
        });
        let pastes = Arc::new(PasteState {
            reject_late: reject_late_paste,
            ..PasteState::default()
        });
        let mic_starts = Arc::new(Mutex::new(0));
        let startup = Arc::new(StartupState::default());
        let (engine_tx, engine_rx) = mpsc::sync_channel(64);
        let (handle, _exited) = spawn_with_poll_interval(
            ControllerConfig {
                settings,
                capabilities,
                models_dir,
                bootstrap,
                process_started_at: Instant::now(),
            },
            rx,
            hotkey_rx,
            tx,
            Box::new(FakeSink(snapshots.clone())),
            Box::new(ScriptedHotkeys(Arc::clone(&hotkeys))),
            Box::new(FakePaster(Arc::clone(&pastes))),
            Box::new(FakeEngineFactory {
                events: Mutex::new(Some(engine_rx)),
                ready: engine_tx.clone(),
            }),
            Box::new(FakeMic(Arc::clone(&mic_starts))),
            Box::new(FakePostprocessFactory {
                replace_file,
                gate: Mutex::new(gate),
            }),
            Box::new(FakeStartup {
                rescans: rescans.into(),
                required: vec![
                    &kikigaki_core::models::MODELS[0],
                    &kikigaki_core::models::MODELS[1],
                ],
                state: Arc::clone(&startup),
            }),
            Box::new(FakeClock(clock)),
            poll_interval,
        );
        Harness {
            _temp: temp,
            client,
            hotkey,
            handle: Some(handle),
            snapshots,
            hotkeys,
            pastes,
            engine_tx,
            mic_starts,
            metrics_path,
            startup,
        }
    }

    /// `settings.js` reads the snapshot's fields directly off the event payload; this pins the
    /// internally-tagged layout (`{"kind":"snapshot","revision":..,"status":..}`) so a serde
    /// attribute change cannot silently orphan the UI again (the first Mac run of Task 4 did
    /// exactly that by looking for a nested `snapshot` key).
    #[test]
    fn snapshot_event_flattens_the_snapshot_next_to_kind() {
        let harness = ready_harness();
        let snapshot = latest(&harness).unwrap();
        let event = crate::status::UiEvent {
            revision: snapshot.revision,
            kind: crate::status::UiEventKind::Snapshot(Box::new(snapshot)),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["kind"], "snapshot");
        assert!(
            value.get("status").is_some(),
            "status must be top-level: {value}"
        );
        assert!(value.get("snapshot").is_none());
        let shutdown = serde_json::to_value(crate::status::UiEvent {
            revision: 8,
            kind: crate::status::UiEventKind::Shutdown,
        })
        .unwrap();
        assert_eq!(shutdown["kind"], "shutdown");
    }

    fn ready_harness() -> Harness {
        let harness = harness(
            false,
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
        );
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        harness
    }

    fn ready_harness_with_poll_interval(poll_interval: Duration) -> Harness {
        let harness = harness_with_poll_interval(
            false,
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
            poll_interval,
        );
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        harness
    }

    fn latest(harness: &Harness) -> Option<UiSnapshot> {
        harness.snapshots.0.lock().unwrap().last().cloned()
    }

    fn wait_snapshot(harness: &Harness, predicate: impl Fn(&UiSnapshot) -> bool) -> UiSnapshot {
        for _ in 0..300 {
            if let Some(snapshot) = latest(harness).filter(|snapshot| predicate(snapshot)) {
                return snapshot;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "snapshot condition not reached; latest={:?}",
            latest(harness)
        );
    }

    fn final_message(gen: u64, text: &str) -> EngineMsg {
        let now = Instant::now();
        EngineMsg::Final {
            gen,
            text: text.into(),
            engine_latency_ms: Some(1.0),
            vad_close_at: now,
            asr_end_at: now + Duration::from_millis(1),
            dropped_chunks: 0,
        }
    }

    fn transcribe(harness: &Harness, gen: u64, text: &str) {
        let attempts = harness.pastes.attempts.lock().unwrap().len();
        harness.hotkey.send(HotkeyEdge::Pressed);
        wait_snapshot(harness, |snapshot| snapshot.status.state == "recording");
        harness.hotkey.send(HotkeyEdge::Released);
        wait_snapshot(harness, |snapshot| snapshot.status.state == "finalizing");
        harness.engine_tx.send(final_message(gen, text)).unwrap();
        for _ in 0..300 {
            if harness.pastes.attempts.lock().unwrap().len() > attempts {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("paste was not attempted");
    }

    #[test]
    fn engine_final_is_processed_without_waiting_for_the_poll_tick() {
        let harness = ready_harness_with_poll_interval(Duration::from_millis(500));
        harness.hotkey.send(HotkeyEdge::Pressed);
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "recording");
        harness.hotkey.send(HotkeyEdge::Released);
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "finalizing");

        let started = Instant::now();
        harness.engine_tx.send(final_message(1, "awake")).unwrap();
        harness.client.wake();
        while harness.pastes.attempts.lock().unwrap().is_empty() {
            assert!(
                started.elapsed() < Duration::from_millis(100),
                "engine final waited for the poll tick"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn hotkey_edge_is_handled_without_waiting_for_the_poll_tick() {
        let harness = ready_harness_with_poll_interval(Duration::from_millis(500));
        let started = Instant::now();
        harness.hotkey.send(HotkeyEdge::Pressed);
        while latest(&harness).unwrap().status.state != "recording" {
            assert!(
                started.elapsed() < Duration::from_millis(100),
                "hotkey edge waited for the poll tick"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn wake_alone_does_not_publish_a_snapshot() {
        let harness = ready_harness_with_poll_interval(Duration::from_millis(500));
        let publishes = harness.snapshots.0.lock().unwrap().len();
        let builds = harness.snapshots.1.load(Ordering::Relaxed);

        harness.client.wake();
        thread::sleep(Duration::from_millis(50));

        assert_eq!(harness.snapshots.0.lock().unwrap().len(), publishes);
        assert_eq!(harness.snapshots.1.load(Ordering::Relaxed), builds);
    }

    fn apply_hotkey(client: &ControllerClient, chord: &str) -> Result<SettingsSnapshot, UiError> {
        client.send_and_wait(|reply| ControllerCmd::ApplySettings {
            patch: SettingsPatch {
                hotkey: Some(chord.into()),
                punctuation: None,
                builtin_replace_dict: None,
            },
            reply,
        })
    }

    #[test]
    fn apply_settings_request_reply_correlates_to_the_right_call() {
        let harness = ready_harness();
        let a = harness.client.clone();
        let b = harness.client.clone();
        let first = thread::spawn(move || apply_hotkey(&a, "Alt+A").unwrap().hotkey);
        let second = thread::spawn(move || apply_hotkey(&b, "Alt+B").unwrap().hotkey);
        assert_eq!(first.join().unwrap(), "Alt+A");
        assert_eq!(second.join().unwrap(), "Alt+B");
    }

    #[test]
    fn concurrent_patches_apply_in_order() {
        let harness = ready_harness();
        let (a_tx, a_rx) = mpsc::sync_channel(1);
        let (b_tx, b_rx) = mpsc::sync_channel(1);
        harness
            .client
            .tx
            .send(ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    hotkey: Some("Alt+A".into()),
                    punctuation: None,
                    builtin_replace_dict: None,
                },
                reply: a_tx,
            })
            .unwrap();
        harness
            .client
            .tx
            .send(ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    hotkey: Some("Alt+B".into()),
                    punctuation: None,
                    builtin_replace_dict: None,
                },
                reply: b_tx,
            })
            .unwrap();
        assert!(a_rx.recv_timeout(Duration::from_secs(1)).unwrap().is_ok());
        assert!(b_rx.recv_timeout(Duration::from_secs(1)).unwrap().is_ok());
        assert_eq!(
            wait_snapshot(&harness, |snapshot| snapshot.settings.hotkey == "Alt+B")
                .settings
                .hotkey,
            "Alt+B"
        );
    }

    #[test]
    fn hotkey_rollback_on_register_failure_including_old_chord_reregister_failure() {
        let harness = harness(
            false,
            BootstrapOutcome::ReadyToStart,
            vec![Err("new failed".into()), Err("old also failed".into())],
            Vec::new(),
            false,
            None,
            None,
        );
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::BeginHotkeyCapture { reply })
            .unwrap();
        let error = harness
            .client
            .send_and_wait(|reply| ControllerCmd::EndHotkeyCapture {
                new_chord: Some("Cmd+Z".into()),
                reply,
            })
            .unwrap_err();
        assert_eq!(error.code, "register_failed");
        wait_snapshot(&harness, |snapshot| {
            snapshot.status.message.as_deref() == Some("ホットキーが無効 — 再設定してください")
        });
    }

    #[test]
    fn unchanged_chord_is_a_no_op_not_a_reregister() {
        let harness = ready_harness();
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::EndHotkeyCapture {
                new_chord: Some("Alt+Space".into()),
                reply,
            })
            .unwrap();
        assert!(harness.hotkeys.registered.lock().unwrap().is_empty());
        assert!(harness.hotkeys.unregistered.lock().unwrap().is_empty());
    }

    #[test]
    fn end_capture_with_unchanged_chord_reregisters_after_real_capture() {
        let harness = ready_harness();
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::BeginHotkeyCapture { reply })
            .unwrap();
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::EndHotkeyCapture {
                new_chord: Some("Alt+Space".into()),
                reply,
            })
            .unwrap();

        assert_eq!(*harness.hotkeys.unregistered.lock().unwrap(), ["Alt+Space"]);
        assert_eq!(*harness.hotkeys.registered.lock().unwrap(), ["Alt+Space"]);

        harness.hotkey.send(HotkeyEdge::Pressed);
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "recording");
    }

    #[test]
    fn model_load_failure_invalidates_the_model_and_marks_startup_failed() {
        let harness = ready_harness();
        harness
            .startup
            .invalidation_results
            .lock()
            .unwrap()
            .push_back(true);
        harness
            .engine_tx
            .send(EngineMsg::Disconnected {
                reason: "bad model".into(),
                failed_model: Some("asr"),
            })
            .unwrap();
        let snapshot = wait_snapshot(&harness, |snapshot| {
            snapshot.status.message.as_deref() == Some("bad model")
        });
        assert_eq!(snapshot.status.phase, "failed");
        assert_eq!(*harness.startup.invalidated.lock().unwrap(), ["asr"]);

        let harness = ready_harness();
        harness
            .engine_tx
            .send(EngineMsg::Disconnected {
                reason: "engine stopped".into(),
                failed_model: None,
            })
            .unwrap();
        let snapshot = wait_snapshot(&harness, |snapshot| {
            snapshot.status.message.as_deref() == Some("engine stopped")
        });
        assert_eq!(snapshot.status.phase, "failed");
        assert!(harness.startup.invalidated.lock().unwrap().is_empty());
    }

    #[test]
    fn first_ready_records_startup_metric_once() {
        let harness = ready_harness();
        harness.engine_tx.send(EngineMsg::Ready).unwrap();

        for _ in 0..100 {
            let startup_rows = std::fs::read_to_string(&harness.metrics_path)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains(r#""kind":"startup""#))
                .count();
            if startup_rows > 0 {
                assert_eq!(startup_rows, 1);
                thread::sleep(Duration::from_millis(80));
                assert_eq!(
                    std::fs::read_to_string(&harness.metrics_path)
                        .unwrap()
                        .lines()
                        .filter(|line| line.contains(r#""kind":"startup""#))
                        .count(),
                    1
                );
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("startup metric was not written");
    }

    #[test]
    fn idle_ticks_do_not_rebuild_the_snapshot() {
        let harness = ready_harness();
        thread::sleep(Duration::from_millis(60));
        let publishes = harness.snapshots.0.lock().unwrap().len();
        let builds = harness.snapshots.1.load(Ordering::Relaxed);

        thread::sleep(Duration::from_millis(120));

        assert_eq!(harness.snapshots.0.lock().unwrap().len(), publishes);
        assert_eq!(harness.snapshots.1.load(Ordering::Relaxed), builds);
    }

    #[test]
    fn begin_hotkey_capture_refused_while_recording() {
        let harness = ready_harness();
        harness.hotkey.send(HotkeyEdge::Pressed);
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "recording");
        let error = harness
            .client
            .send_and_wait(|reply| ControllerCmd::BeginHotkeyCapture { reply })
            .unwrap_err();
        assert_eq!(error.code, "not_idle");
    }

    #[test]
    fn ordered_configure_then_reload_rules_apply_in_order() {
        let harness = ready_harness();
        transcribe(&harness, 1, "x");
        let entry = wait_snapshot(&harness, |snapshot| snapshot.history.len() == 1).history[0].id;
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    hotkey: None,
                    punctuation: Some(PunctSetting::On),
                    builtin_replace_dict: None,
                },
                reply,
            })
            .unwrap();
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::RememberCorrection {
                entry_id: entry,
                corrected: "y".into(),
                reply,
            })
            .unwrap();
        transcribe(&harness, 2, "x");
        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "y。"
        );
    }

    #[test]
    fn window_focus_suppresses_the_hotkey() {
        let harness = ready_harness();
        harness.client.send(ControllerCmd::SetWindowFocused(true));
        thread::sleep(Duration::from_millis(50));
        harness.hotkey.send(HotkeyEdge::Pressed);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(latest(&harness).unwrap().status.state, "idle");
        assert_eq!(*harness.mic_starts.lock().unwrap(), 0);
    }

    #[test]
    fn shutdown_handshake_joins_within_five_seconds_and_bounded_workers_are_dropped() {
        let mut harness = ready_harness();
        assert!(harness.stop());
    }

    #[test]
    fn quit_is_idempotent_on_repeat_delivery() {
        let mut harness = ready_harness();
        harness.client.send(ControllerCmd::Quit);
        harness.client.send(ControllerCmd::Quit);
        let handle = harness.handle.take().unwrap();
        assert!(handle.join(Duration::from_secs(5)));
    }

    #[test]
    fn channel_saturation_returns_busy_instead_of_blocking() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut harness = harness(
            false,
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            Some(BuildGate {
                entered: entered_tx,
                release: release_rx,
            }),
        );
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut replies = Vec::new();
        for _ in 0..64 {
            let (reply, rx) = mpsc::sync_channel(1);
            harness
                .client
                .tx
                .try_send(ControllerCmd::ApplySettings {
                    patch: SettingsPatch {
                        hotkey: None,
                        punctuation: None,
                        builtin_replace_dict: None,
                    },
                    reply,
                })
                .unwrap();
            replies.push(rx);
        }
        let started = Instant::now();
        let error = harness
            .client
            .send_and_wait(|reply| ControllerCmd::ClearHistory { reply })
            .unwrap_err();
        assert_eq!(error.code, "busy");
        assert!(started.elapsed() < Duration::from_millis(100));
        release_tx.send(()).unwrap();
        for reply in replies {
            assert!(reply.recv_timeout(Duration::from_secs(3)).unwrap().is_ok());
        }
        assert!(harness.stop());
    }

    #[test]
    fn paste_timeout_never_pastes_after_its_deadline() {
        let old = Instant::now() - Duration::from_secs(10);
        let harness = harness(
            false,
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            true,
            Some(old),
            None,
        );
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        transcribe(&harness, 1, "late");
        wait_snapshot(&harness, |snapshot| snapshot.status.message.is_some());
        for _ in 0..200 {
            if std::fs::read_to_string(&harness.metrics_path)
                .is_ok_and(|body| body.contains("\"paste_failed\":true"))
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("paste failure metric not written");
    }

    #[test]
    fn end_to_end_final_paste_history_correction_reload_next_final() {
        let harness = ready_harness();
        transcribe(&harness, 1, "misheard");
        let first = wait_snapshot(&harness, |snapshot| snapshot.history.len() == 1);
        assert_eq!(first.history[0].raw, "misheard");
        assert_eq!(first.history[0].text, "misheard");
        let entry_id = first.history[0].id;
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::RememberCorrection {
                entry_id,
                corrected: "corrected".into(),
                reply,
            })
            .unwrap();
        transcribe(&harness, 2, "misheard");
        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "corrected"
        );
    }

    #[test]
    fn onboarding_download_progress_includes_required_package_position_and_bytes() {
        let harness = harness(
            false,
            BootstrapOutcome::NeedsOnboarding(onboarding(false, "models")),
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
        );
        harness
            .startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Installing {
                id: kikigaki_core::models::MODELS[1].id,
                done_bytes: 25,
                total_bytes: 100,
            });

        let snapshot = wait_snapshot(&harness, |snapshot| {
            snapshot
                .onboarding
                .as_ref()
                .and_then(|state| state.download.as_ref())
                .is_some_and(|progress| progress.bytes == 25)
        });
        let progress = snapshot.onboarding.unwrap().download.unwrap();
        assert_eq!(progress.model, kikigaki_core::models::MODELS[1].id);
        assert_eq!((progress.done, progress.total), (1, 2));
        assert_eq!((progress.bytes, progress.total_bytes), (25, 100));
        assert!(!progress.failed);
    }

    #[test]
    fn onboarding_download_failure_is_terminal_even_without_a_progress_event() {
        let harness = harness(
            false,
            BootstrapOutcome::NeedsOnboarding(onboarding(false, "models")),
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
        );
        harness
            .startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Failed("offline".into()));

        let snapshot = wait_snapshot(&harness, |snapshot| {
            snapshot
                .onboarding
                .as_ref()
                .and_then(|state| state.download.as_ref())
                .is_some_and(|progress| progress.failed)
        });
        let progress = snapshot.onboarding.unwrap().download.unwrap();
        assert!(progress.failed);
        assert_eq!(progress.model, kikigaki_core::models::MODELS[0].id);
        assert_eq!((progress.done, progress.total), (0, 2));
        assert_eq!(snapshot.status.message.as_deref(), Some("offline"));
    }

    #[test]
    fn retry_download_rejects_while_in_flight_and_clears_failure_on_progress() {
        let harness = harness(
            false,
            BootstrapOutcome::NeedsOnboarding(onboarding(false, "models")),
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
        );
        harness.startup.events.lock().unwrap().extend([
            StartupEvent::Installing {
                id: kikigaki_core::models::MODELS[0].id,
                done_bytes: 10,
                total_bytes: 100,
            },
            StartupEvent::Failed("offline".into()),
        ]);
        wait_snapshot(&harness, |snapshot| {
            snapshot
                .onboarding
                .as_ref()
                .and_then(|state| state.download.as_ref())
                .is_some_and(|progress| progress.failed)
        });

        harness.startup.hold_starts.store(true, Ordering::Relaxed);
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::RetryDownload { reply })
            .unwrap();
        let error = harness
            .client
            .send_and_wait(|reply| ControllerCmd::RetryDownload { reply })
            .unwrap_err();
        assert_eq!(error.code, "already_downloading");
        assert_eq!(harness.startup.starts.load(Ordering::Relaxed), 1);

        harness
            .startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Installing {
                id: kikigaki_core::models::MODELS[0].id,
                done_bytes: 20,
                total_bytes: 100,
            });
        wait_snapshot(&harness, |snapshot| {
            snapshot
                .onboarding
                .as_ref()
                .and_then(|state| state.download.as_ref())
                .is_some_and(|progress| progress.bytes == 20 && !progress.failed)
        });
    }

    #[test]
    fn rescan_onboarding_preserves_terminal_download_state() {
        let harness = harness(
            false,
            BootstrapOutcome::NeedsOnboarding(onboarding(false, "models")),
            Vec::new(),
            vec![Some(onboarding(false, "models"))],
            false,
            None,
            None,
        );
        harness
            .startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Failed("offline".into()));
        wait_snapshot(&harness, |snapshot| {
            snapshot
                .onboarding
                .as_ref()
                .and_then(|state| state.download.as_ref())
                .is_some_and(|progress| progress.failed)
        });

        harness.client.send(ControllerCmd::RescanOnboarding);
        for _ in 0..100 {
            if harness.startup.rescans.load(Ordering::Relaxed) == 1 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(harness.startup.rescans.load(Ordering::Relaxed), 1);
        assert!(latest(&harness)
            .unwrap()
            .onboarding
            .unwrap()
            .download
            .is_some_and(|progress| progress.failed));
    }

    #[test]
    fn rescan_onboarding_republishes_only_on_change() {
        let initial = onboarding(false, "one");
        let changed = onboarding(true, "two");
        let harness = harness(
            false,
            BootstrapOutcome::NeedsOnboarding(initial.clone()),
            Vec::new(),
            vec![Some(initial), Some(changed.clone()), Some(changed), None],
            false,
            None,
            None,
        );
        wait_snapshot(&harness, |snapshot| snapshot.onboarding.is_some());
        let before = harness.snapshots.0.lock().unwrap().len();
        harness.client.send(ControllerCmd::RescanOnboarding);
        thread::sleep(Duration::from_millis(80));
        assert_eq!(harness.snapshots.0.lock().unwrap().len(), before);
        harness.client.send(ControllerCmd::RescanOnboarding);
        for _ in 0..100 {
            if harness.snapshots.0.lock().unwrap().len() == before + 1 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(harness.snapshots.0.lock().unwrap().len(), before + 1);
        harness.client.send(ControllerCmd::RescanOnboarding);
        thread::sleep(Duration::from_millis(80));
        assert_eq!(harness.snapshots.0.lock().unwrap().len(), before + 1);

        harness.client.send(ControllerCmd::RescanOnboarding);
        let completed = wait_snapshot(&harness, |snapshot| snapshot.onboarding.is_none());
        assert!(completed.onboarding.is_none());
        assert_eq!(harness.startup.starts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn config_error_bootstrap_is_replayable_in_the_snapshot() {
        let harness = harness(
            false,
            BootstrapOutcome::ConfigError(BootstrapError {
                code: "config",
                message: "bad config".into(),
            }),
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
        );
        let snapshot = wait_snapshot(&harness, |snapshot| snapshot.bootstrap_error.is_some());
        assert_eq!(snapshot.bootstrap_error.unwrap().message, "bad config");
    }
}
