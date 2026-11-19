//! Tauri-free push-to-talk controller and its injected platform ports.

use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use kikigaki_core::config::{postprocess_settings, Capabilities, Config, EngineKind};
use kikigaki_core::engine::{Engine, EngineCmd, EngineMsg, EnginePhase, EngineSupervisor, Waker};
use kikigaki_core::metrics;
use kikigaki_core::postprocess::{to_protocol_final, PostprocessWorker};
use kikigaki_core::protocol::{Event, SAMPLE_RATE};
use kikigaki_core::session::{Input, Output, Session, SessionConfig, State};
use kikigaki_core::startup::StartupEvent;

use crate::history::History;
use crate::learned::Learned;
use crate::settings::{SettingsCoordinator, SettingsPatch};
use crate::status::{
    AutostartState, BootstrapError, DownloadProgress, OnboardingState, Status, UiError, UiSnapshot,
    UiStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyKey {
    Standard,
}

pub enum HotkeyEdge {
    Pressed(HotkeyKey),
    Released(HotkeyKey),
}

pub enum ControllerCmd {
    /// Delivered by event producers so the run loop drains their queues without waiting for the
    /// poll tick.
    Wake,
    #[cfg(target_os = "windows")]
    MicPrepared(anyhow::Result<crate::mic::BuiltStream>),
    ApplySettings {
        patch: SettingsPatch,
        reply: ReplyTx<crate::settings::SettingsSnapshot>,
    },
    ClearHistory {
        reply: ReplyTx<()>,
    },
    RetryDownload {
        reply: ReplyTx<()>,
    },
    BeginHotkeyCapture {
        key: HotkeyKey,
        reply: ReplyTx<()>,
    },
    EndHotkeyCapture {
        key: HotkeyKey,
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
    /// Register or replace this key's chord, leaving its previous chord intact on failure.
    fn register(&self, key: HotkeyKey, chord: &str) -> Result<(), String>;
    fn unregister(&self, key: HotkeyKey) -> Result<(), String>;
}

pub trait Paster: Send {
    fn paste(&self, text: String, deadline: Instant) -> Result<(), String>;
}

pub trait EngineFactory: Send + Sync {
    fn build(&self, kind: EngineKind) -> anyhow::Result<Box<dyn Engine>>;

    fn set_hotwords_enabled(&self, _enabled: bool) {}
}

pub trait MicPort: Send {
    #[cfg(target_os = "windows")]
    fn set_controller(&mut self, _client: ControllerClient) {}
    #[cfg(target_os = "windows")]
    fn install_prepared(&mut self, _result: anyhow::Result<crate::mic::BuiltStream>) {}
    fn captured_audio(&self) -> bool {
        false
    }

    fn prepare(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn start(&mut self, sink: kikigaki_core::engine::AudioSink) -> anyhow::Result<()>;
    fn stop(&mut self);
}

pub trait PostprocessFactory: Send {
    fn build(
        &self,
        learned: std::sync::Arc<kikigaki_core::replace::Rules>,
        builtin_enabled: bool,
        load_punctuator: bool,
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

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
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
const MICROPHONE_PERMISSION_DENIED: &str = "マイクの使用が許可されていません";
use crate::platform::MICROPHONE_PERMISSION_DETAIL;

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

fn register_failed(message: String) -> UiError {
    UiError {
        code: "register_failed",
        message,
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
    engine_factory: Arc<dyn EngineFactory>,
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
    engine_factory: Arc<dyn EngineFactory>,
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
    engine_factory: Arc<dyn EngineFactory>,
    mic: Box<dyn MicPort>,
    postprocess_factory: Box<dyn PostprocessFactory>,
    startup: Box<dyn StartupPort>,
    clock: Box<dyn Clock>,
    controller_client: ControllerClient,
    poll_interval: Duration,
) {
    #[cfg(target_os = "windows")]
    let mic = {
        let mut mic = mic;
        mic.set_controller(controller_client.clone());
        mic
    };
    let waker: Waker = Arc::new(move || controller_client.wake());
    let (mut controller, auto_start) = Controller::new(
        cfg,
        sink,
        hotkeys,
        paster,
        engine_factory,
        mic,
        postprocess_factory,
        startup,
        clock,
        waker,
    );

    if auto_start {
        match controller.startup.start() {
            Ok(started) => controller.download_in_flight = started,
            Err(error) => {
                controller.status = Status::startup_failed(error.to_string());
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
        controller.reconcile_hotwords(&mut outputs);
        controller.handle_startup_events(&mut outputs);
        controller.handle_engine_messages(&mut outputs);
        controller.handle_postprocess_events(&mut outputs);
        outputs.extend(
            controller
                .session
                .handle(Input::Tick, controller.clock.now()),
        );
        controller.execute_outputs(std::mem::take(&mut outputs));
        controller.publish_if_changed();
    }
}

const PASTE_FAILED_NOTICE: &str = "文字を貼り付けられません";
const PASTE_FAILED_DETAIL: &str =
    "システム設定 › プライバシーとセキュリティ › アクセシビリティ で kikigaki を許可してください";

struct Controller {
    warm_mic: bool,
    mic_warning: Option<crate::status::StatusText>,
    settings: SettingsCoordinator,
    session: Session,
    mic: Box<dyn MicPort>,
    supervisor: Option<EngineSupervisor>,
    engine_factory: Arc<dyn EngineFactory>,
    startup: Box<dyn StartupPort>,
    standard_engine: EngineKind,
    capabilities: Capabilities,
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
    pending_finals: VecDeque<(u64, String, String)>,
    owners: HashSet<u64>,
    // Tombstones protect this turn; after pruning, a missing owner rejects later finals.
    retired: HashSet<u64>,
    active: Option<u64>,
    hotwords_desired: bool,
    hotwords_applied: bool,
    reconcile_pending: bool,
    engine_build_generation: u64,
    postprocess_degraded: Option<String>,
    shutting_down: bool,
}

impl Controller {
    #[allow(clippy::too_many_arguments)]
    fn new(
        cfg: ControllerConfig,
        sink: Box<dyn ControllerEventSink>,
        hotkeys: Box<dyn HotkeyPort>,
        paster: Box<dyn Paster>,
        engine_factory: Arc<dyn EngineFactory>,
        mic: Box<dyn MicPort>,
        postprocess_factory: Box<dyn PostprocessFactory>,
        startup: Box<dyn StartupPort>,
        clock: Box<dyn Clock>,
        waker: Waker,
    ) -> (Self, bool) {
        let ControllerConfig {
            settings,
            capabilities,
            bootstrap,
            process_started_at,
        } = cfg;
        let session = Session::new(SessionConfig {
            silence_pad_ms: settings.config().silence_pad_ms,
            final_timeout: Duration::from_millis(settings.config().final_timeout_ms),
        });
        let hotwords_enabled = settings.config().builtin_replace_dict;
        let standard_engine = settings.config().engine;
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
        let supervisor_factory = Arc::clone(&engine_factory);
        let supervisor =
            EngineSupervisor::new(Box::new(move || supervisor_factory.build(standard_engine)))
                .with_waker(Arc::clone(&waker));
        let controller = Self {
            warm_mic: cfg!(target_os = "windows"),
            mic_warning: None,
            settings,
            session,
            mic,
            supervisor: Some(supervisor),
            engine_factory,
            startup,
            standard_engine,
            capabilities,
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
            pending_finals: VecDeque::new(),
            owners: HashSet::new(),
            retired: HashSet::new(),
            active: None,
            hotwords_desired: hotwords_enabled,
            hotwords_applied: hotwords_enabled,
            reconcile_pending: false,
            engine_build_generation: 0,
            postprocess_degraded: None,
            shutting_down: false,
        };

        (controller, auto_start)
    }

    fn cancel_generation(&mut self, gen: u64) {
        if let Some(supervisor) = self.supervisor.as_ref() {
            supervisor.cancel(gen);
            send_command(supervisor, EngineCmd::Cancel { gen });
        }
        self.retire_generation(gen);
    }

    fn handle_command(&mut self, command: ControllerCmd) {
        if matches!(command, ControllerCmd::Wake) {
            return;
        }
        self.dirty = true;
        match command {
            #[cfg(target_os = "windows")]
            ControllerCmd::MicPrepared(result) => self.mic.install_prepared(result),
            ControllerCmd::ApplySettings { patch, reply } => {
                let result = self.apply_settings(patch);
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
            ControllerCmd::BeginHotkeyCapture { key: _, reply } => {
                if self.session.state() != State::Idle {
                    let _ = reply.send(Err(UiError {
                        code: "not_idle",
                        message: "聞き取り中はホットキーを変更できません".into(),
                    }));
                    return;
                }
                let result = self.hotkeys.unregister(HotkeyKey::Standard);
                match result {
                    Ok(()) => {
                        self.capturing_hotkey = true;
                        let _ = reply.send(Ok(()));
                    }
                    Err(message) => {
                        let _ = self.restore_capture_hotkeys();
                        let _ = reply.send(Err(register_failed(message)));
                    }
                }
            }
            ControllerCmd::EndHotkeyCapture {
                key,
                new_chord,
                reply,
            } => {
                self.end_hotkey_capture(key, new_chord, reply);
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
                            self.status = Status::startup_failed(error.to_string());
                        }
                    }
                }
                self.publish_if_changed();
            }
            ControllerCmd::Quit => self.begin_shutdown(),
            _ => unreachable!("Wake returned before dispatch"),
        }
    }

    fn reregister_or_warn(&mut self, key: HotkeyKey, chord: &str) -> Result<(), UiError> {
        self.hotkeys.register(key, chord).map_err(|message| {
            self.status = Status::Warning("ホットキーが無効 — 再設定してください".into());
            register_failed(message)
        })
    }

    fn restore_capture_hotkeys(&mut self) -> Result<(), UiError> {
        let standard = self.settings.config().hotkey.clone();
        self.reregister_or_warn(HotkeyKey::Standard, &standard)
    }

    fn end_hotkey_capture(
        &mut self,
        key: HotkeyKey,
        new_chord: Option<String>,
        reply: ReplyTx<crate::settings::SettingsSnapshot>,
    ) {
        let was_capturing = self.capturing_hotkey;
        self.capturing_hotkey = false;
        let old = self.settings.config().hotkey.clone();
        let Some(chord) = new_chord else {
            let result = self.restore_capture_hotkeys();
            let _ = reply.send(result.map(|()| self.settings_snapshot()));
            return;
        };
        if chord == old {
            let result = if was_capturing {
                self.restore_capture_hotkeys()
            } else {
                Ok(())
            };
            let _ = reply.send(result.map(|()| self.settings_snapshot()));
            return;
        }
        if let Err(message) = self.hotkeys.register(key, &chord) {
            let _ = self.restore_capture_hotkeys();
            let _ = reply.send(Err(register_failed(message)));
            return;
        }
        let patch = SettingsPatch {
            hotkey: Some(chord.clone()),
            ..SettingsPatch::default()
        };
        match self.settings.apply(patch) {
            Ok(_) => {
                tracing::info!(?key, chord = %chord, "hotkey re-registered after settings change");
                let _ = reply.send(Ok(self.settings_snapshot()));
            }
            Err(error) => {
                let _ = self.restore_capture_hotkeys();
                let _ = reply.send(Err(error));
            }
        }
    }

    fn handle_hotkey_edge(&mut self, edge: HotkeyEdge, outputs: &mut Vec<Output>) {
        let pressed = matches!(edge, HotkeyEdge::Pressed(_));
        tracing::debug!(pressed, "hotkey edge");
        if !pressed && self.active.is_none_or(|gen| !self.owners.contains(&gen)) {
            return;
        }
        let Some(input) = hotkey_input(edge, self.window_focused, self.capturing_hotkey) else {
            return;
        };
        self.dirty = true;
        let edge_outputs = self.session.handle(input, self.clock.now());
        // Bind each edge's Begin before another edge or engine message can be handled.
        for output in &edge_outputs {
            match output {
                Output::Cancel { gen } => self.retire_generation(*gen),
                Output::Begin { gen } => {
                    self.owners.insert(*gen);
                    self.active = Some(*gen);
                }
                _ => {}
            }
        }
        outputs.extend(edge_outputs);
    }

    fn generation_is_in_progress(&self, gen: u64) -> bool {
        // Segment pastes can drain after release; the end Final still owns this generation.
        self.active == Some(gen)
            && matches!(self.session.state(), State::Recording | State::Finalizing)
    }

    fn retire_generation(&mut self, gen: u64) {
        self.retired.insert(gen);
        if self.active == Some(gen) {
            self.active = None;
        }
    }

    fn submit_final(&self, message: EngineMsg) {
        let EngineMsg::Final { gen, .. } = &message else {
            return;
        };
        let gen = *gen;
        if !self.owners.contains(&gen) || self.retired.contains(&gen) {
            tracing::debug!(gen, "dropping final without a live owner");
            return;
        }
        if let Some(worker) = &self.postprocess {
            let result = worker.submit(
                gen,
                message,
                postprocess_settings(
                    self.settings.config(),
                    self.capabilities,
                    self.standard_engine,
                    self.settings.applied_asr_model(),
                ),
            );
            if let Err(error) = result {
                tracing::error!(%error, "submit configured final for post-processing");
            }
        } else {
            tracing::error!(gen, "post-processing is unavailable");
        }
    }

    fn standard_disconnected(&mut self, outputs: &mut Vec<Output>) {
        if let Some(gen) = self.active {
            self.retire_generation(gen);
        }
        outputs.extend(
            self.session
                .handle(Input::EngineDisconnected, self.clock.now()),
        );
    }

    fn reconcile_hotwords(&mut self, outputs: &mut Vec<Output>) {
        if !self.reconcile_pending {
            return;
        }
        if self.hotwords_desired == self.hotwords_applied {
            self.reconcile_pending = false;
            return;
        }
        if self.standard_engine == EngineKind::Remote {
            self.engine_factory
                .set_hotwords_enabled(self.hotwords_desired);
            self.hotwords_applied = self.hotwords_desired;
            self.reconcile_pending = false;
            return;
        }
        if self.session.state() != State::Idle
            || self
                .supervisor
                .as_ref()
                .is_none_or(|supervisor| supervisor.phase() == EnginePhase::Starting)
        {
            return;
        }

        let previously_applied = self.hotwords_applied;
        self.engine_factory
            .set_hotwords_enabled(self.hotwords_desired);
        self.engine_build_generation = self.engine_build_generation.wrapping_add(1);
        let restart = self
            .supervisor
            .as_mut()
            .expect("supervisor checked above")
            .restart();
        match restart {
            Ok(true) => {
                self.hotwords_applied = self.hotwords_desired;
                self.reconcile_pending = false;
            }
            Ok(false) => {
                self.engine_factory.set_hotwords_enabled(previously_applied);
            }
            Err(error) => {
                self.engine_factory.set_hotwords_enabled(previously_applied);
                self.mic.stop();
                self.status = Status::engine_failed(error.to_string());
                self.standard_disconnected(outputs);
            }
        }
        self.dirty = true;
    }

    fn apply_settings(
        &mut self,
        patch: SettingsPatch,
    ) -> Result<crate::settings::SettingsSnapshot, UiError> {
        if patch.hotkey.is_some() {
            return Err(UiError {
                code: "invalid_patch",
                message: "キーの組み合わせはキー入力で変更してください".into(),
            });
        }
        let snapshot = self.settings.apply(patch)?;
        self.hotwords_desired = snapshot.builtin_replace_dict;
        self.reconcile_pending = self.hotwords_desired != self.hotwords_applied;
        if let Some(worker) = self.postprocess.as_ref() {
            if let Err(error) = worker.set_builtin_enabled(snapshot.builtin_replace_dict) {
                tracing::warn!(%error, "configure builtin replacement dictionary");
                self.postprocess_degraded = Some("内蔵置換辞書を反映できませんでした".into());
            } else {
                self.postprocess_degraded = None;
            }
        }
        Ok(self.settings_snapshot())
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
                    self.status =
                        Status::Installing(progress_message(done_bytes, total_bytes).into());
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
                                self.status = Status::startup_failed(format!(
                                    "Engine start failed — {error}"
                                ));
                                false
                            }
                        }
                    });
                    if !engine_started {
                        self.standard_disconnected(outputs);
                        continue;
                    }
                    if self.postprocess.is_none() {
                        let settings = self.settings.snapshot();
                        match self.postprocess_factory.build(
                            self.learned.as_core_rules(),
                            settings.builtin_replace_dict,
                            !self.settings.config().engine_punctuates(
                                self.standard_engine,
                                self.settings.applied_asr_model(),
                            ),
                            Some(Arc::clone(&self.waker)),
                        ) {
                            Ok(worker) => {
                                self.postprocess = Some(worker);
                            }
                            Err(error) => {
                                tracing::error!(error = %format!("{error:#}"), "failed to start post-processing");
                                self.status = Status::startup_failed(format!(
                                    "Post-processing start failed — {error}"
                                ));
                                if let Some(supervisor) = self.supervisor.as_mut() {
                                    supervisor.shutdown();
                                }
                                self.standard_disconnected(outputs);
                            }
                        }
                    }
                }
                StartupEvent::Failed(reason) => {
                    self.download_in_flight = false;
                    self.status = Status::startup_failed(reason.clone());
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
                    self.standard_disconnected(outputs);
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
            if self.retired.contains(&processed.gen) || !self.owners.contains(&processed.gen) {
                continue;
            }
            let final_outputs = self.session.handle(
                Input::Engine(Event::Final(to_protocol_final(&processed))),
                self.clock.now(),
            );
            if final_outputs
                .iter()
                .any(|output| matches!(output, Output::Paste(_)))
            {
                self.pending_finals
                    .push_back((processed.gen, processed.raw, processed.text));
            }
            outputs.extend(final_outputs);
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
                    let stale = self.standard_engine == EngineKind::Local
                        && self.hotwords_applied != self.hotwords_desired;
                    if stale {
                        self.reconcile_pending = true;
                        tracing::info!(
                            engine_build_generation = self.engine_build_generation,
                            "engine ready for superseded hotword configuration"
                        );
                    } else {
                        let process_to_ready_ms = self
                            .clock
                            .now()
                            .saturating_duration_since(self.process_started_at)
                            .as_millis()
                            .try_into()
                            .unwrap_or(u64::MAX);
                        tracing::info!(process_to_ready_ms, "engine ready");
                        if self.warm_mic && self.session.state() != State::Recording {
                            if let Err(error) = self.mic.prepare() {
                                tracing::warn!(%error, "microphone prepare failed; will open per press");
                            }
                        }
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
                    }
                    Input::Engine(Event::Ready { sr: SAMPLE_RATE })
                }
                message @ EngineMsg::Final { .. } => {
                    self.submit_final(message);
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
                        Status::startup_failed(reason)
                    } else {
                        Status::engine_failed(reason)
                    };
                    self.standard_disconnected(outputs);
                    continue;
                }
            };
            outputs.extend(self.session.handle(input, self.clock.now()));
        }
    }

    fn execute_outputs(&mut self, outputs: Vec<Output>) {
        let mut pending = VecDeque::from(outputs);
        let mut mic_gen = None;
        while let Some(output) = pending.pop_front() {
            self.dirty = true;
            match output {
                Output::Begin { gen } => {
                    mic_gen = Some(gen);
                    if let Some(supervisor) = self.supervisor.as_ref() {
                        send_command(supervisor, EngineCmd::Begin { gen });
                    }
                }
                Output::StartMic => {
                    let sink = mic_gen
                        .and(self.supervisor.as_ref())
                        .and_then(EngineSupervisor::sink);
                    if let Some(sink) = sink {
                        let warning = microphone_warning(
                            crate::permissions::microphone_permission(),
                            cfg!(target_os = "windows"),
                        );
                        let standard_failed = matches!(
                            self.status,
                            Status::StartupFailed(_) | Status::EngineFailed(_)
                        );
                        if let Some((title, detail)) = warning.filter(|_| !standard_failed) {
                            let text = crate::status::StatusText::new(title, detail);
                            self.mic_warning = Some(text.clone());
                            self.status = Status::Warning(text);
                        }
                        if let Err(error) = self.mic.start(sink) {
                            tracing::warn!(%error, "microphone unavailable");
                            if !standard_failed
                                && (!cfg!(target_os = "windows") || warning.is_none())
                            {
                                let text = crate::status::StatusText::new(
                                    "マイクを使えません",
                                    error.to_string(),
                                );
                                self.mic_warning = Some(text.clone());
                                self.status = Status::Warning(text);
                            }
                        } else if warning.is_none()
                            && (!cfg!(target_os = "windows") || self.mic_warning.is_none())
                            && !matches!(
                                &self.status,
                                Status::Installing(_) | Status::EngineFailed(_)
                            )
                        {
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
                    self.cancel_generation(gen);
                }
                Output::Paste(text) => {
                    let final_text = self.pending_finals.pop_front();
                    let final_gen = final_text.as_ref().map(|(gen, _, _)| *gen);
                    if final_gen.is_some_and(|gen| self.retired.contains(&gen)) {
                        // Discard the corresponding pending metrics row as well as the paste.
                        self.session.paste_failed(self.clock.now());
                        continue;
                    }
                    let text_len = text.chars().count();
                    let deadline = self.clock.now() + PASTE_TIMEOUT;
                    match self.paster.paste(text, deadline) {
                        Ok(()) => {
                            tracing::info!(outcome = "ok", text_len, "paste completed");
                            if !matches!(
                                &self.status,
                                Status::Installing(_) | Status::EngineFailed(_)
                            ) {
                                self.status.clear_transient();
                            }
                            if let Some((gen, raw, text)) = final_text {
                                let at = Utc::now();
                                crate::transcript::enqueue(
                                    self.settings.config().transcript_log,
                                    self.settings.config().transcript_dir.clone(),
                                    self.standard_engine.as_str(),
                                    raw.clone(),
                                    text.clone(),
                                    at,
                                );
                                self.history.push(raw, text, at);
                                if !self.generation_is_in_progress(gen) {
                                    self.retire_generation(gen);
                                }
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
                            if !matches!(
                                self.status,
                                Status::StartupFailed(_) | Status::EngineFailed(_)
                            ) {
                                self.status = Status::Warning(crate::status::StatusText::new(
                                    PASTE_FAILED_NOTICE,
                                    PASTE_FAILED_DETAIL,
                                ));
                            }
                            if let Some(gen) =
                                final_gen.filter(|gen| !self.generation_is_in_progress(*gen))
                            {
                                self.retire_generation(gen);
                            }
                            pending.extend(self.session.paste_failed(self.clock.now()));
                        }
                    }
                }
                Output::Record(mut row) => {
                    row.ts = metric_timestamp();
                    row.engine = Some(self.standard_engine.as_str().into());
                    if !self.generation_is_in_progress(row.gen) {
                        self.retire_generation(row.gen);
                    }
                    append_metric(self.settings.config(), metrics::MetricRow::Utterance(row));
                }
                Output::SetState(_) => {}
                Output::Reconnect => {
                    if self.status.is_startup_failed() {
                        self.status = Status::Ok;
                        match self.startup.start() {
                            Ok(started) => self.download_in_flight = started,
                            Err(error) => {
                                self.status = Status::startup_failed(error.to_string());
                            }
                        }
                    } else if let Some(supervisor) = self.supervisor.as_mut() {
                        if supervisor.phase() != EnginePhase::Starting {
                            self.engine_build_generation =
                                self.engine_build_generation.wrapping_add(1);
                        }
                        match supervisor.restart() {
                            Ok(_) => {}
                            Err(error) => {
                                self.status = Status::engine_failed(error.to_string());
                            }
                        }
                    }
                }
            }
        }
        // Prune retired generations after the output drain.
        self.owners.retain(|gen| !self.retired.contains(gen));
        self.retired.clear();
    }

    fn snapshot(&self) -> UiSnapshot {
        #[cfg(test)]
        self.sink.record_snapshot_build();
        let settings = self.settings_snapshot();
        let phase = self
            .supervisor
            .as_ref()
            .map_or(EnginePhase::Failed, EngineSupervisor::phase);
        let status = self.effective_status();
        UiSnapshot {
            revision: self.revision,
            status: UiStatus {
                state: state_name(self.session.state()),
                phase: phase_name(status.phase(phase)),
                level: status.level(),
                message: status.message().map(str::to_owned),
                detail: status.detail().map(str::to_owned),
                hotkey: settings.hotkey.clone(),
                local_punct: settings.local_punct,
                strip_trailing_period: settings.strip_trailing_period,
            },
            settings,
            platform: crate::platform::NAME,
            // The platform shell replaces this Tauri-free placeholder before publishing.
            autostart: AutostartState {
                enabled: false,
                available: false,
            },
            onboarding: self.onboarding.clone(),
            history: self.history.list(),
            bootstrap_error: self.bootstrap_error.clone(),
        }
    }

    fn settings_snapshot(&self) -> crate::settings::SettingsSnapshot {
        self.settings.snapshot()
    }

    fn effective_status(&self) -> Status {
        if matches!(
            self.status,
            Status::StartupFailed(_) | Status::EngineFailed(_)
        ) {
            return match (&self.status, &self.postprocess_degraded) {
                (Status::EngineFailed(engine), Some(postprocess)) => {
                    Status::EngineFailed(crate::status::StatusText {
                        title: engine.title.clone(),
                        detail: Some(engine.detail.as_deref().map_or_else(
                            || postprocess.clone(),
                            |detail| format!("{detail}\n{postprocess}"),
                        )),
                    })
                }
                _ => self.status.clone(),
            };
        }
        if self.status.message() == Some(PASTE_FAILED_NOTICE) {
            return self.status.clone();
        }
        self.postprocess_degraded.as_ref().map_or_else(
            || self.status.clone(),
            |message| Status::Warning(message.clone().into()),
        )
    }

    fn publish_if_changed(&mut self) {
        if cfg!(target_os = "windows")
            && clear_captured_mic_warning(
                &self.status,
                self.mic_warning.as_ref(),
                self.mic.captured_audio(),
            )
        {
            self.mic_warning = None;
            self.status = Status::Ok;
            self.dirty = true;
        }
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
        let deadline = Instant::now() + Duration::from_millis(4500);
        self.postprocess.take();
        self.supervisor.take();
        if !self
            .startup
            .join(deadline.saturating_duration_since(Instant::now()))
        {
            tracing::warn!("startup worker did not join before shutdown deadline");
        }
        self.publish_if_changed();
    }
}

fn hotkey_input(edge: HotkeyEdge, window_focused: bool, capturing_hotkey: bool) -> Option<Input> {
    match edge {
        HotkeyEdge::Pressed(_) if window_focused || capturing_hotkey => None,
        HotkeyEdge::Pressed(_) => Some(Input::Pressed),
        HotkeyEdge::Released(_) => Some(Input::Released),
    }
}

fn persistence_error(prefix: &str, error: anyhow::Error) -> UiError {
    UiError {
        code: "write_failed",
        message: format!("{prefix}: {error:#}"),
    }
}

fn percent(done: u64, total: u64) -> u8 {
    if total == 0 {
        0
    } else {
        (done.saturating_mul(100) / total).min(100) as u8
    }
}

fn progress_message(done_bytes: u64, total_bytes: u64) -> String {
    let percent = percent(done_bytes, total_bytes);
    format!("モデルをダウンロード中… {percent}%")
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

fn clear_captured_mic_warning(
    status: &Status,
    own: Option<&crate::status::StatusText>,
    captured: bool,
) -> bool {
    captured && matches!(status, Status::Warning(text) if Some(text) == own)
}

fn microphone_warning(
    permission: crate::permissions::MicPermission,
    windows: bool,
) -> Option<(&'static str, &'static str)> {
    use crate::permissions::MicPermission;
    if !windows {
        return (!permission.is_granted())
            .then_some((MICROPHONE_PERMISSION_DENIED, MICROPHONE_PERMISSION_DETAIL));
    }
    match permission {
        MicPermission::Denied => Some((MICROPHONE_PERMISSION_DENIED, MICROPHONE_PERMISSION_DETAIL)),
        MicPermission::Busy => Some((
            "マイクを使えません",
            "ほかのアプリがマイクを使っています。閉じてから、もう一度確認してください。",
        )),
        MicPermission::Unavailable => Some((
            "マイクを使えません",
            "マイクが見つかりません。接続してから、もう一度確認してください。",
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use kikigaki_core::engine::{self, AudioSink, EngineCmd, EngineMsg};
    use kikigaki_core::postprocess::{Pipeline, Punctuator};
    use kikigaki_core::replace::ReplaceFile;

    use super::*;
    use crate::settings::SettingsSnapshot;
    use crate::status::{BootstrapError, OnboardingState};

    #[test]
    fn friendly_status_snapshot_preserves_raw_details() {
        let mut h = LifecycleHarness::new();
        h.controller.status = Status::startup_failed("raw startup");
        let json = serde_json::to_value(h.controller.snapshot()).unwrap();
        assert_eq!(
            json["status"]["message"],
            "音声エンジンを起動できませんでした"
        );
        assert_eq!(json["status"]["detail"], "raw startup");
        assert_eq!(
            MICROPHONE_PERMISSION_DENIED,
            "マイクの使用が許可されていません"
        );
        assert_eq!(PASTE_FAILED_NOTICE, "文字を貼り付けられません");
        h.controller.status = Status::engine_failed("raw engine");
        h.controller.postprocess_degraded = Some("raw postprocess".into());
        let status = h.controller.snapshot().status;
        assert_eq!(
            status.message.as_deref(),
            Some("音声エンジンが停止しました")
        );
        assert_eq!(
            status.detail.as_deref(),
            Some("raw engine\nraw postprocess")
        );
        h.controller.postprocess_degraded = None;
        for (title, detail) in [
            (MICROPHONE_PERMISSION_DENIED, MICROPHONE_PERMISSION_DETAIL),
            (PASTE_FAILED_NOTICE, PASTE_FAILED_DETAIL),
        ] {
            h.controller.status = Status::Warning(crate::status::StatusText::new(title, detail));
            let status = h.controller.snapshot().status;
            assert_eq!(status.message.as_deref(), Some(title));
            assert_eq!(status.detail.as_deref(), Some(detail));
        }
        h.controller.status = Status::Ok;
        let json = serde_json::to_value(h.controller.snapshot()).unwrap();
        assert!(json["status"]["detail"].is_null());
        assert_eq!(progress_message(45, 100), "モデルをダウンロード中… 45%");
    }

    #[test]
    fn hotkey_input_handles_standard_edges() {
        for edge in [
            HotkeyEdge::Pressed(HotkeyKey::Standard),
            HotkeyEdge::Released(HotkeyKey::Standard),
        ] {
            assert!(hotkey_input(edge, false, false).is_some());
        }
    }

    #[test]
    fn factory_builds_the_requested_kind() {
        let factory = FakeEngineFactory::new(Arc::new(FakeEngineState::default()));
        for kind in [EngineKind::Local, EngineKind::Remote] {
            let mut engine = factory.build(kind).unwrap();
            assert!(matches!(engine.events().try_recv(), Ok(EngineMsg::Ready)));
            assert_eq!(factory.engine(kind).builds.load(Ordering::Relaxed), 1);
            assert_eq!(*factory.engine(kind).observed_kinds.lock().unwrap(), [kind]);
        }
    }

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
        registered: Mutex<Vec<(HotkeyKey, String)>>,
        unregistered: Mutex<Vec<HotkeyKey>>,
    }

    struct ScriptedHotkeys(Arc<HotkeyState>);

    impl HotkeyPort for ScriptedHotkeys {
        fn register(&self, key: HotkeyKey, chord: &str) -> Result<(), String> {
            self.0.registered.lock().unwrap().push((key, chord.into()));
            self.0.plan.lock().unwrap().pop_front().unwrap_or(Ok(()))
        }

        fn unregister(&self, key: HotkeyKey) -> Result<(), String> {
            self.0.unregistered.lock().unwrap().push(key);
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
        _commands: Arc<Mutex<Receiver<EngineCmd>>>,
        events: Receiver<EngineMsg>,
        state: Arc<FakeEngineState>,
    }

    impl Engine for TestEngine {
        fn sink(&self) -> AudioSink {
            self.sink.clone()
        }

        fn events(&mut self) -> &mut Receiver<EngineMsg> {
            &mut self.events
        }

        fn cancel(&self, gen: u64) {
            self.state.canceled.lock().unwrap().push(gen);
        }

        fn shutdown(self: Box<Self>) {
            self.state
                .shutdown_threads
                .lock()
                .unwrap()
                .push(thread::current().name().unwrap_or("unnamed").into());
            if let Some(gate) = self.state.shutdown_gate.lock().unwrap().take() {
                gate.entered.send(()).unwrap();
                gate.release.recv().unwrap();
            }
            if let Some(delay) = *self.state.shutdown_delay.lock().unwrap() {
                thread::sleep(delay);
            }
            let _ = self.sink.send(EngineCmd::Shutdown);
        }
    }

    enum FakeEngineBuild {
        Ready,
        Pending,
        Fail(String),
    }

    #[derive(Default)]
    struct FakeEngineState {
        enabled: AtomicBool,
        builds: AtomicUsize,
        observed_enabled: Mutex<Vec<bool>>,
        shutdown_delay: Mutex<Option<Duration>>,
        shutdown_gate: Mutex<Option<BuildGate>>,
        shutdown_threads: Mutex<Vec<String>>,
        observed_kinds: Mutex<Vec<EngineKind>>,
        plans: Mutex<VecDeque<FakeEngineBuild>>,
        event_senders: Mutex<Vec<SyncSender<EngineMsg>>>,
        canceled: Mutex<Vec<u64>>,
        command_receivers: Mutex<Vec<Arc<Mutex<Receiver<EngineCmd>>>>>,
    }

    impl FakeEngineState {
        fn send(&self, message: EngineMsg) {
            self.event_senders
                .lock()
                .unwrap()
                .last()
                .expect("engine has been built")
                .send(message)
                .unwrap();
        }
    }

    struct FakeEngineFactory {
        states: Arc<Vec<(EngineKind, Arc<FakeEngineState>)>>,
    }

    impl EngineFactory for FakeEngineFactory {
        fn build(&self, kind: EngineKind) -> anyhow::Result<Box<dyn Engine>> {
            let state = self.engine(kind);
            state.builds.fetch_add(1, Ordering::Relaxed);
            state
                .observed_enabled
                .lock()
                .unwrap()
                .push(state.enabled.load(Ordering::Relaxed));
            state.observed_kinds.lock().unwrap().push(kind);
            let plan = state
                .plans
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(FakeEngineBuild::Ready);
            match plan {
                FakeEngineBuild::Fail(message) => anyhow::bail!(message),
                plan => {
                    let (event_tx, events) = mpsc::sync_channel(64);
                    state.event_senders.lock().unwrap().push(event_tx.clone());
                    if matches!(plan, FakeEngineBuild::Ready) {
                        event_tx.send(EngineMsg::Ready).unwrap();
                    }
                    let (sink, commands) = engine::channel();
                    let commands = Arc::new(Mutex::new(commands));
                    state
                        .command_receivers
                        .lock()
                        .unwrap()
                        .push(Arc::clone(&commands));
                    Ok(Box::new(TestEngine {
                        sink,
                        _commands: commands,
                        events,
                        state,
                    }))
                }
            }
        }

        fn set_hotwords_enabled(&self, enabled: bool) {
            for state in self.states.iter().map(|(_, state)| state) {
                state.enabled.store(enabled, Ordering::Relaxed);
            }
        }
    }

    fn engine_states() -> Arc<Vec<(EngineKind, Arc<FakeEngineState>)>> {
        Arc::new(Vec::from([
            (EngineKind::Local, Arc::new(FakeEngineState::default())),
            (EngineKind::Remote, Arc::new(FakeEngineState::default())),
        ]))
    }

    fn engine_state(
        states: &[(EngineKind, Arc<FakeEngineState>)],
        kind: EngineKind,
    ) -> Arc<FakeEngineState> {
        Arc::clone(&states.iter().find(|(key, _)| *key == kind).unwrap().1)
    }

    impl FakeEngineFactory {
        fn new(standard: Arc<FakeEngineState>) -> Self {
            let mut states = engine_states();
            Arc::get_mut(&mut states).unwrap()[0].1 = standard;
            Self::from_states(states)
        }

        fn from_states(states: Arc<Vec<(EngineKind, Arc<FakeEngineState>)>>) -> Self {
            Self { states }
        }

        fn engine(&self, kind: EngineKind) -> Arc<FakeEngineState> {
            engine_state(&self.states, kind)
        }
    }

    #[derive(Default)]
    struct MicState {
        prepares: AtomicUsize,
        prepare_fails: AtomicBool,
        starts: AtomicUsize,
        stops: AtomicUsize,
        running: AtomicBool,
    }

    struct FakeMic(Arc<MicState>);

    impl MicPort for FakeMic {
        fn prepare(&mut self) -> anyhow::Result<()> {
            self.0.prepares.fetch_add(1, Ordering::Relaxed);
            anyhow::ensure!(
                !self.0.prepare_fails.load(Ordering::Relaxed),
                "scripted prepare failure"
            );
            Ok(())
        }

        fn start(&mut self, _sink: AudioSink) -> anyhow::Result<()> {
            self.0.starts.fetch_add(1, Ordering::Relaxed);
            self.0.running.store(true, Ordering::Relaxed);
            Ok(())
        }

        fn stop(&mut self) {
            self.0.stops.fetch_add(1, Ordering::Relaxed);
            self.0.running.store(false, Ordering::Relaxed);
        }
    }

    #[derive(Default)]
    struct PunctuatorState {
        panic: AtomicBool,
        panicked: AtomicBool,
    }

    struct PeriodPunctuator(Arc<PunctuatorState>);

    impl Punctuator for PeriodPunctuator {
        fn punctuate(&mut self, text: &str) -> anyhow::Result<String> {
            if self.0.panic.load(Ordering::Relaxed) {
                self.0.panicked.store(true, Ordering::Relaxed);
                panic!("scripted post-processing failure");
            }
            Ok(format!("{text}。"))
        }
    }

    struct BuildGate {
        entered: SyncSender<()>,
        release: Receiver<()>,
    }

    struct FakePostprocessFactory {
        replace_file: PathBuf,
        builtin: Arc<kikigaki_core::replace::Rules>,
        gate: Mutex<Option<BuildGate>>,
        punctuator: Arc<PunctuatorState>,
        load_punctuator: Arc<Mutex<Vec<bool>>>,
    }

    impl FakePostprocessFactory {
        fn new(replace_file: PathBuf, gate: Option<BuildGate>) -> Self {
            Self {
                replace_file,
                builtin: Arc::new(kikigaki_core::replace::builtin_rules().unwrap()),
                gate: Mutex::new(gate),
                punctuator: Arc::new(PunctuatorState::default()),
                load_punctuator: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl PostprocessFactory for FakePostprocessFactory {
        fn build(
            &self,
            learned: Arc<kikigaki_core::replace::Rules>,
            builtin_enabled: bool,
            load_punctuator: bool,
            waker: Option<Waker>,
        ) -> anyhow::Result<PostprocessWorker> {
            self.load_punctuator.lock().unwrap().push(load_punctuator);
            if let Some(gate) = self.gate.lock().unwrap().take() {
                gate.entered.send(()).unwrap();
                gate.release.recv().unwrap();
            }
            Ok(PostprocessWorker::spawn_with_waker(
                Pipeline::new(
                    ReplaceFile::new(self.replace_file.clone()),
                    Box::new(PeriodPunctuator(Arc::clone(&self.punctuator))),
                    kikigaki_core::postprocess::PostprocessSettings::default(),
                    Arc::clone(&self.builtin),
                    builtin_enabled,
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
        enforce_install_lock: AtomicBool,
        install_lock_held: AtomicBool,
        rescans: AtomicUsize,
        starts: AtomicUsize,
        join_budgets: Mutex<Vec<Duration>>,
    }

    struct FakeStartup {
        rescans: VecDeque<Option<OnboardingState>>,
        required: Vec<&'static kikigaki_core::models::Model>,
        state: Arc<StartupState>,
        owns_install_lock: bool,
    }

    impl StartupPort for FakeStartup {
        fn start(&mut self) -> anyhow::Result<bool> {
            self.state.starts.fetch_add(1, Ordering::Relaxed);
            if self.state.enforce_install_lock.load(Ordering::Relaxed) {
                if self.state.install_lock_held.swap(true, Ordering::Relaxed) {
                    anyhow::bail!("another install in progress for local");
                }
                self.owns_install_lock = true;
            }
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
            let event = self.state.events.lock().unwrap().pop_front();
            if self.owns_install_lock
                && matches!(
                    event,
                    Some(StartupEvent::Installed(_) | StartupEvent::Failed(_))
                )
            {
                self.state.install_lock_held.store(false, Ordering::Relaxed);
                self.owns_install_lock = false;
            }
            event
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

        fn join(&mut self, timeout: Duration) -> bool {
            self.state.join_budgets.lock().unwrap().push(timeout);
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
        engines: Arc<Vec<(EngineKind, Arc<FakeEngineState>)>>,
        mic: Arc<MicState>,
        punctuator: Arc<PunctuatorState>,
        metrics_path: PathBuf,
        startup: Arc<StartupState>,
    }

    impl Harness {
        fn engine(&self, kind: EngineKind) -> Arc<FakeEngineState> {
            engine_state(&self.engines, kind)
        }

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
        harness_with_initial_builtin_and_poll_interval(
            punct,
            false,
            bootstrap,
            hotkey_plan,
            rescans,
            reject_late_paste,
            clock,
            gate,
            poll_interval,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn harness_with_initial_builtin_and_poll_interval(
        punct: bool,
        builtin_replace_dict: bool,
        bootstrap: BootstrapOutcome,
        hotkey_plan: Vec<Result<(), String>>,
        rescans: Vec<Option<OnboardingState>>,
        reject_late_paste: bool,
        clock: Option<Instant>,
        gate: Option<BuildGate>,
        poll_interval: Duration,
    ) -> Harness {
        harness_with_engine_options(
            punct,
            builtin_replace_dict,
            EngineKind::Local,
            VecDeque::new(),
            bootstrap,
            hotkey_plan,
            rescans,
            reject_late_paste,
            clock,
            gate,
            poll_interval,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn harness_with_engine_options(
        punct: bool,
        builtin_replace_dict: bool,
        engine_kind: EngineKind,
        engine_plans: VecDeque<FakeEngineBuild>,
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
        let engine_name = engine_kind.as_str();
        std::fs::write(
            &config_path,
            format!(
                "hotkey = \"Alt+Space\"\nengine = \"{engine_name}\"\nstrip_trailing_period = false\nbuiltin_replace_dict = {builtin_replace_dict}\nreplace_file = {:?}\nlearned_file = {:?}\nmetrics_path = {:?}\nmodels_dir = {:?}\n[asr]\nmodel = \"reazonspeech\"\n[punct]\nenabled = {}\n",
                replace_file, learned_file, metrics_path, models_dir, punct,
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
        let mic = Arc::new(MicState::default());
        let postprocess_factory = FakePostprocessFactory::new(replace_file, gate);
        let punctuator = Arc::clone(&postprocess_factory.punctuator);
        let startup = Arc::new(StartupState::default());
        let engines = engine_states();
        engines
            .iter()
            .find(|(key, _)| *key == engine_kind)
            .unwrap()
            .1
            .plans
            .lock()
            .unwrap()
            .extend(engine_plans);
        for state in engines.iter().map(|(_, state)| state) {
            state.enabled.store(builtin_replace_dict, Ordering::Relaxed);
        }
        let (handle, _exited) = spawn_with_poll_interval(
            ControllerConfig {
                settings,
                capabilities,
                bootstrap,
                process_started_at: Instant::now(),
            },
            rx,
            hotkey_rx,
            tx,
            Box::new(FakeSink(snapshots.clone())),
            Box::new(ScriptedHotkeys(Arc::clone(&hotkeys))),
            Box::new(FakePaster(Arc::clone(&pastes))),
            Arc::new(FakeEngineFactory::from_states(Arc::clone(&engines))),
            Box::new(FakeMic(Arc::clone(&mic))),
            Box::new(postprocess_factory),
            Box::new(FakeStartup {
                rescans: rescans.into(),
                required: vec![
                    &kikigaki_core::models::MODELS[0],
                    &kikigaki_core::models::MODELS[1],
                ],
                state: Arc::clone(&startup),
                owns_install_lock: false,
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
            engines,
            mic,
            punctuator,
            metrics_path,
            startup,
        }
    }

    // Direct turns reuse the injected ports above; gates make teardown assertions deterministic.
    struct LifecycleHarness {
        controller: Controller,
        engines: Arc<Vec<(EngineKind, Arc<FakeEngineState>)>>,
        hotkeys: Arc<HotkeyState>,
        startup: Arc<StartupState>,
        pastes: Arc<PasteState>,
        _temp: tempfile::TempDir,
    }

    impl Drop for LifecycleHarness {
        fn drop(&mut self) {
            self.controller.begin_shutdown();
        }
    }

    impl LifecycleHarness {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("config.toml");
            let replace_file = temp.path().join("replace.toml");
            std::fs::write(&replace_file, "").unwrap();
            std::fs::write(&path, format!("replace_file = {:?}\nmetrics_path = {:?}\ntranscript_dir = {:?}\nlearned_file = {:?}\n[asr]\nmodel = \"reazonspeech\"\n[punct]\nenabled = false\n", replace_file, temp.path().join("metrics.jsonl"), temp.path().join("transcripts"), temp.path().join("learned.toml"))).unwrap();
            let capabilities = Capabilities {
                punct: true,
                remote_engine: true,
            };
            let settings = SettingsCoordinator::load(path, capabilities).unwrap();
            let engines = engine_states();
            let factory: Arc<dyn EngineFactory> =
                Arc::new(FakeEngineFactory::from_states(Arc::clone(&engines)));
            let hotkeys = Arc::new(HotkeyState::default());
            let startup = Arc::new(StartupState::default());
            let pastes = Arc::new(PasteState::default());
            let (controller, _) = Controller::new(
                ControllerConfig {
                    settings,
                    capabilities,
                    bootstrap: BootstrapOutcome::ReadyToStart,
                    process_started_at: Instant::now(),
                },
                Box::new(FakeSink(SinkState::default())),
                Box::new(ScriptedHotkeys(Arc::clone(&hotkeys))),
                Box::new(FakePaster(Arc::clone(&pastes))),
                factory,
                Box::new(FakeMic(Arc::new(MicState::default()))),
                Box::new(FakePostprocessFactory::new(replace_file, None)),
                Box::new(FakeStartup {
                    rescans: VecDeque::new(),
                    required: Vec::new(),
                    state: Arc::clone(&startup),
                    owns_install_lock: false,
                }),
                Box::new(RealClock),
                Arc::new(|| {}),
            );
            Self {
                controller,
                engines,
                hotkeys,
                startup,
                pastes,
                _temp: temp,
            }
        }

        fn engine(&self, kind: EngineKind) -> Arc<FakeEngineState> {
            engine_state(&self.engines, kind)
        }

        fn turn(&mut self) {
            let c = &mut self.controller;
            let mut outputs = Vec::new();
            c.reconcile_hotwords(&mut outputs);
            c.handle_startup_events(&mut outputs);
            c.handle_engine_messages(&mut outputs);
            c.handle_postprocess_events(&mut outputs);
            c.execute_outputs(outputs);
        }

        fn boot(&mut self, pending: bool) {
            if pending {
                self.engine(EngineKind::Local)
                    .plans
                    .lock()
                    .unwrap()
                    .push_back(FakeEngineBuild::Pending);
            }
            self.controller.startup.start().unwrap();
            self.turn();
        }

        fn disconnected(&mut self, model: Option<&'static str>) {
            self.engine(EngineKind::Local)
                .send(EngineMsg::Disconnected {
                    reason: "engine failed".into(),
                    failed_model: model,
                });
            self.turn();
        }
    }

    #[test]
    fn warm_mic_ready_prepares_and_press_release_does_not_prepare_again() {
        let mut h = LifecycleHarness::new();
        let mic = Arc::new(MicState::default());
        h.controller.mic = Box::new(FakeMic(Arc::clone(&mic)));
        h.controller.warm_mic = true;
        h.boot(false);
        assert!(poll(|| {
            h.turn();
            mic.prepares.load(Ordering::Relaxed) == 1
        }));
        assert_eq!(mic.starts.load(Ordering::Relaxed), 0);
        let mut outputs = Vec::new();
        h.controller
            .handle_hotkey_edge(HotkeyEdge::Pressed(HotkeyKey::Standard), &mut outputs);
        h.controller.execute_outputs(std::mem::take(&mut outputs));
        assert_eq!(mic.starts.load(Ordering::Relaxed), 1);
        h.engine(EngineKind::Local).send(EngineMsg::Ready);
        h.turn();
        assert_eq!(h.controller.session.state(), State::Recording);
        assert_eq!(mic.prepares.load(Ordering::Relaxed), 1);
        h.controller
            .handle_hotkey_edge(HotkeyEdge::Released(HotkeyKey::Standard), &mut outputs);
        h.controller.execute_outputs(outputs);
        assert_eq!(mic.stops.load(Ordering::Relaxed), 1);
        assert_eq!(mic.prepares.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cold_mic_ready_does_not_prepare() {
        let mut h = LifecycleHarness::new();
        let mic = Arc::new(MicState::default());
        h.controller.mic = Box::new(FakeMic(Arc::clone(&mic)));
        h.controller.warm_mic = false;
        h.boot(false);
        assert!(poll(|| {
            h.turn();
            h.controller.session.state() == State::Idle
        }));
        assert_eq!(mic.prepares.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn failed_warm_prepare_preserves_ready_and_press_retries_normally() {
        let mut h = LifecycleHarness::new();
        let mic = Arc::new(MicState::default());
        mic.prepare_fails.store(true, Ordering::Relaxed);
        h.controller.mic = Box::new(FakeMic(Arc::clone(&mic)));
        h.controller.warm_mic = true;
        h.boot(false);
        assert!(poll(|| {
            h.turn();
            mic.prepares.load(Ordering::Relaxed) == 1
        }));
        assert_eq!(h.controller.session.state(), State::Idle);
        assert!(matches!(h.controller.status, Status::Ok));
        let mut outputs = Vec::new();
        h.controller
            .handle_hotkey_edge(HotkeyEdge::Pressed(HotkeyKey::Standard), &mut outputs);
        h.controller.execute_outputs(outputs);
        assert_eq!(mic.starts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn superseded_hotword_ready_does_not_prepare_microphone() {
        let mut h = LifecycleHarness::new();
        let mic = Arc::new(MicState::default());
        h.controller.mic = Box::new(FakeMic(Arc::clone(&mic)));
        h.controller.warm_mic = true;
        h.boot(true);
        assert!(poll(|| {
            h.turn();
            !h.engine(EngineKind::Local)
                .event_senders
                .lock()
                .unwrap()
                .is_empty()
        }));
        h.controller.hotwords_desired = !h.controller.hotwords_applied;
        h.engine(EngineKind::Local).send(EngineMsg::Ready);
        h.turn();
        assert_eq!(mic.prepares.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn postprocess_factory_load_flag_uses_applied_asr_model() {
        use kikigaki_core::config::AsrModel;
        for (model, expected) in [(AsrModel::Haas, false), (AsrModel::ReazonSpeech, true)] {
            let mut h = LifecycleHarness::new();
            let path = h._temp.path().join("config.toml");
            let config = std::fs::read_to_string(&path).unwrap();
            std::fs::write(
                &path,
                config.replace(
                    "reazonspeech",
                    if model == AsrModel::Haas {
                        "haas"
                    } else {
                        "reazonspeech"
                    },
                ),
            )
            .unwrap();
            h.controller.settings = SettingsCoordinator::load(
                path,
                Capabilities {
                    punct: true,
                    remote_engine: true,
                },
            )
            .unwrap();
            assert_eq!(h.controller.settings.applied_asr_model(), model);
            assert_eq!(h.controller.standard_engine, EngineKind::Local);
            let factory = FakePostprocessFactory::new(h._temp.path().join("replace.toml"), None);
            let flags = Arc::clone(&factory.load_punctuator);
            h.controller.postprocess_factory = Box::new(factory);
            h.boot(false);
            assert_eq!(*flags.lock().unwrap(), [expected]);
        }
    }

    mod routing_tests {
        use super::*;

        struct AudioMic;
        impl MicPort for AudioMic {
            fn start(&mut self, sink: AudioSink) -> anyhow::Result<()> {
                sink.send(EngineCmd::Audio(vec![0.25]))?;
                Ok(())
            }
            fn stop(&mut self) {}
        }

        fn ready() -> (LifecycleHarness, Receiver<()>) {
            let mut h = LifecycleHarness::new();
            let (tx, rx) = mpsc::channel();
            h.controller.waker = Arc::new(move || {
                let _ = tx.send(());
            });
            h.controller.mic = Box::new(AudioMic);
            h.boot(false);
            (h, rx)
        }

        fn edges(h: &mut LifecycleHarness, edges: impl IntoIterator<Item = HotkeyEdge>) {
            let mut outputs = Vec::new();
            for edge in edges {
                h.controller.handle_hotkey_edge(edge, &mut outputs);
            }
            h.controller.execute_outputs(outputs);
        }

        fn utterance(h: &mut LifecycleHarness, key: HotkeyKey) {
            edges(h, [HotkeyEdge::Pressed(key), HotkeyEdge::Released(key)]);
            assert_eq!(h.controller.session.state(), State::Finalizing);
        }

        fn finish(
            h: &mut LifecycleHarness,
            rx: &Receiver<()>,
            kind: EngineKind,
            gen: u64,
            text: &str,
        ) {
            h.engine(kind).send(final_message(gen, text));
            h.turn();
            rx.recv_timeout(Duration::from_secs(1))
                .expect("final was not processed");
            h.turn();
        }

        fn commands(h: &LifecycleHarness, kind: EngineKind) -> Vec<EngineCmd> {
            h.engine(kind)
                .command_receivers
                .lock()
                .unwrap()
                .iter()
                .flat_map(|rx| rx.lock().unwrap().try_iter().collect::<Vec<_>>())
                .collect()
        }

        // A sentinel traverses the worker's FIFO and proves every earlier submission was drained.
        fn assert_no_submission(h: &mut LifecycleHarness) {
            let worker = h.controller.postprocess.as_ref().unwrap();
            worker
                .submit(
                    u64::MAX,
                    final_message(u64::MAX, "barrier"),
                    kikigaki_core::postprocess::PostprocessSettings::default(),
                )
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                if let Some(result) = worker.try_recv() {
                    assert_eq!(
                        result.gen,
                        u64::MAX,
                        "a rejected final reached postprocessing"
                    );
                    break;
                }
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
        }

        fn rows(h: &LifecycleHarness) -> Vec<serde_json::Value> {
            std::fs::read_to_string(&h.controller.settings.config().metrics_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .filter(|row| row["kind"] == "utterance")
                .collect()
        }

        fn transcripts(h: &LifecycleHarness) -> Vec<serde_json::Value> {
            transcripts_with_count(h, 1)
        }

        fn transcripts_with_count(h: &LifecycleHarness, count: usize) -> Vec<serde_json::Value> {
            let dir = &h.controller.settings.config().transcript_dir;
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let rows: Vec<_> = std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                    .flat_map(|body| {
                        body.lines()
                            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                            .collect::<Vec<_>>()
                    })
                    .collect();
                if rows.len() >= count {
                    return rows;
                }
                assert!(Instant::now() < deadline, "transcript was not written");
                thread::yield_now();
            }
        }

        mod review_fix {
            use super::*;

            fn enable_transcripts(h: &mut LifecycleHarness) {
                h.controller
                    .apply_settings(SettingsPatch {
                        transcript_log: Some(true),
                        ..SettingsPatch::default()
                    })
                    .unwrap();
            }

            #[test]
            fn standard_multi_segment_utterance_pastes_every_segment_and_the_end_final() {
                let (mut h, rx) = ready();
                enable_transcripts(&mut h);
                edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
                for (index, text) in ["first", "second"].into_iter().enumerate() {
                    finish(&mut h, &rx, EngineKind::Local, 1, text);
                    assert_eq!(h.controller.session.state(), State::Recording);
                    assert_eq!(h.controller.active, Some(1));
                    assert!(!h.controller.retired.contains(&1));
                    assert!(h.controller.owners.contains(&1));
                    assert_eq!(transcripts_with_count(&h, index + 1).len(), index + 1);
                }
                edges(&mut h, [HotkeyEdge::Released(HotkeyKey::Standard)]);
                assert_eq!(h.controller.session.state(), State::Finalizing);
                finish(&mut h, &rx, EngineKind::Local, 1, "end");
                assert_eq!(
                    *h.pastes.attempts.lock().unwrap(),
                    ["first", "second", "end"]
                );
                let metrics = rows(&h);
                assert_eq!(metrics.len(), 3);
                assert!(metrics.iter().all(|row| row["gen"] == 1
                    && row["engine"] == "local"
                    && row["paste_failed"] == false));
                assert!(metrics[0]["since_press_ms"].is_number());
                assert!(metrics[1]["since_press_ms"].is_number());
                assert!(metrics[2]["release_to_final_ms"].is_number());
                let transcript = transcripts_with_count(&h, 3);
                assert_eq!(
                    transcript
                        .iter()
                        .map(|row| row["text"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["first", "second", "end"]
                );
                assert!(transcript.iter().all(|row| row["engine"] == "local"));
                assert_eq!(h.controller.session.state(), State::Idle);
                assert!(h.controller.active.is_none());
                assert!(h.controller.owners.is_empty());
                h.engine(EngineKind::Local).send(final_message(1, "late"));
                h.turn();
                assert_no_submission(&mut h);
            }

            #[test]
            fn queued_mid_utterance_pastes_remain_live_if_release_precedes_output_drain() {
                let (mut h, rx) = ready();
                edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
                for text in ["first", "second"] {
                    h.engine(EngineKind::Local).send(final_message(1, text));
                }
                let mut outputs = Vec::new();
                h.controller.handle_engine_messages(&mut outputs);
                for _ in 0..2 {
                    rx.recv_timeout(Duration::from_secs(1)).unwrap();
                }
                h.controller.handle_postprocess_events(&mut outputs);
                h.controller
                    .handle_hotkey_edge(HotkeyEdge::Released(HotkeyKey::Standard), &mut outputs);
                h.controller.execute_outputs(outputs);
                assert_eq!(*h.pastes.attempts.lock().unwrap(), ["first", "second"]);
                assert_eq!(h.controller.session.state(), State::Finalizing);
                assert_eq!(h.controller.active, Some(1));
                assert_eq!(rows(&h).len(), 2);
                finish(&mut h, &rx, EngineKind::Local, 1, "end");
                assert_eq!(
                    *h.pastes.attempts.lock().unwrap(),
                    ["first", "second", "end"]
                );
                assert_eq!(rows(&h).len(), 3);
                assert_eq!(h.controller.session.state(), State::Idle);
                assert!(h.controller.active.is_none());
            }

            #[test]
            fn mid_utterance_paste_failure_does_not_drop_later_segments() {
                struct FirstPasteFails {
                    state: Arc<PasteState>,
                    first: AtomicBool,
                }
                impl Paster for FirstPasteFails {
                    fn paste(&self, text: String, _deadline: Instant) -> Result<(), String> {
                        self.state.attempts.lock().unwrap().push(text);
                        if self.first.swap(false, Ordering::Relaxed) {
                            Err("scripted paste failure".into())
                        } else {
                            Ok(())
                        }
                    }
                }
                let (mut h, rx) = ready();
                enable_transcripts(&mut h);
                h.controller.paster = Box::new(FirstPasteFails {
                    state: Arc::clone(&h.pastes),
                    first: AtomicBool::new(true),
                });
                edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
                finish(&mut h, &rx, EngineKind::Local, 1, "failed segment");
                assert_eq!(h.controller.session.state(), State::Recording);
                assert_eq!(h.controller.active, Some(1));
                assert_eq!(
                    h.controller.status.message(),
                    Some("文字を貼り付けられません")
                );
                assert_eq!(h.controller.status.detail(), Some(PASTE_FAILED_DETAIL));
                assert_eq!(rows(&h)[0]["paste_failed"], true);
                finish(&mut h, &rx, EngineKind::Local, 1, "later segment");
                assert_eq!(transcripts_with_count(&h, 1)[0]["text"], "later segment");
                edges(&mut h, [HotkeyEdge::Released(HotkeyKey::Standard)]);
                finish(&mut h, &rx, EngineKind::Local, 1, "end");
                assert_eq!(
                    *h.pastes.attempts.lock().unwrap(),
                    ["failed segment", "later segment", "end"]
                );
                let metrics = rows(&h);
                assert_eq!(
                    metrics
                        .iter()
                        .map(|row| row["paste_failed"].as_bool().unwrap())
                        .collect::<Vec<_>>(),
                    [true, false, false]
                );
                assert!(metrics.iter().all(|row| row["engine"] == "local"));
                assert_eq!(transcripts_with_count(&h, 2).len(), 2);
                assert_eq!(h.controller.history.list().len(), 2);
                assert_eq!(h.controller.session.state(), State::Idle);
                assert!(h.controller.owners.is_empty());
            }

            #[test]
            fn repress_in_finalizing_after_mid_utterance_segments_cancels_and_begins_new() {
                let (mut h, rx) = ready();
                edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
                for text in ["first", "second"] {
                    finish(&mut h, &rx, EngineKind::Local, 1, text);
                }
                edges(
                    &mut h,
                    [
                        HotkeyEdge::Released(HotkeyKey::Standard),
                        HotkeyEdge::Pressed(HotkeyKey::Standard),
                    ],
                );
                assert_eq!(*h.engine(EngineKind::Local).canceled.lock().unwrap(), [1]);
                assert_eq!(h.controller.active, Some(2));
                assert!(!h.controller.owners.contains(&1));
                edges(&mut h, [HotkeyEdge::Released(HotkeyKey::Standard)]);
                finish(&mut h, &rx, EngineKind::Local, 2, "new utterance");
                assert_eq!(
                    *h.pastes.attempts.lock().unwrap(),
                    ["first", "second", "new utterance"]
                );
                h.engine(EngineKind::Local)
                    .send(final_message(1, "canceled end"));
                h.turn();
                assert_no_submission(&mut h);
            }
        }

        #[test]
        fn standard_key_routes_begin_audio_end_to_standard() {
            let (mut h, rx) = ready();
            utterance(&mut h, HotkeyKey::Standard);
            assert_eq!(
                commands(&h, EngineKind::Local),
                [
                    EngineCmd::Begin { gen: 1 },
                    EngineCmd::Audio(vec![0.25]),
                    EngineCmd::End {
                        gen: 1,
                        pad_ms: 500
                    }
                ]
            );
            finish(&mut h, &rx, EngineKind::Local, 1, "one transcription");
            assert_eq!(*h.pastes.attempts.lock().unwrap(), ["one transcription"]);
            assert_eq!(h.controller.session.state(), State::Idle);
            assert_eq!(
                h.engine(EngineKind::Local).builds.load(Ordering::Relaxed),
                1
            );
        }

        #[test]
        fn rejected_press_leaves_no_route() {
            let mut h = LifecycleHarness::new();
            h.boot(true);
            edges(
                &mut h,
                [
                    HotkeyEdge::Pressed(HotkeyKey::Standard),
                    HotkeyEdge::Released(HotkeyKey::Standard),
                ],
            );
            assert_eq!(h.controller.session.state(), State::Disconnected);
            h.engine(EngineKind::Local).send(EngineMsg::Ready);
            h.turn();
            h.engine(EngineKind::Local)
                .send(final_message(1, "rejected"));
            h.turn();
            assert_no_submission(&mut h);
        }

        #[test]
        fn repress_in_finalizing_cancels_old_owner_and_begins_new() {
            let (mut h, _) = ready();
            utterance(&mut h, HotkeyKey::Standard);
            commands(&h, EngineKind::Local);
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
            assert_eq!(*h.engine(EngineKind::Local).canceled.lock().unwrap(), [1]);
            assert_eq!(
                commands(&h, EngineKind::Local),
                [
                    EngineCmd::Cancel { gen: 1 },
                    EngineCmd::Begin { gen: 2 },
                    EngineCmd::Audio(vec![0.25])
                ]
            );
        }

        #[test]
        fn batched_press_release_repress_routes_each_gen_to_its_owner() {
            let (mut h, _) = ready();
            edges(
                &mut h,
                [
                    HotkeyEdge::Pressed(HotkeyKey::Standard),
                    HotkeyEdge::Released(HotkeyKey::Standard),
                    HotkeyEdge::Pressed(HotkeyKey::Standard),
                ],
            );
            assert_eq!(
                commands(&h, EngineKind::Local),
                [
                    EngineCmd::Begin { gen: 1 },
                    EngineCmd::Audio(vec![0.25]),
                    EngineCmd::End {
                        gen: 1,
                        pad_ms: 500
                    },
                    EngineCmd::Cancel { gen: 1 },
                    EngineCmd::Begin { gen: 2 },
                    EngineCmd::Audio(vec![0.25])
                ]
            );
        }

        #[test]
        fn record_and_transcript_read_owner_before_prune() {
            let (mut h, rx) = ready();
            h.controller
                .apply_settings(SettingsPatch {
                    transcript_log: Some(true),
                    ..SettingsPatch::default()
                })
                .unwrap();
            utterance(&mut h, HotkeyKey::Standard);
            finish(&mut h, &rx, EngineKind::Local, 1, "owner");
            assert_eq!(rows(&h)[0]["engine"], "local");
            assert_eq!(transcripts(&h)[0]["engine"], "local");
            assert!(h.controller.owners.is_empty());
            assert!(h.controller.active.is_none());
            h.engine(EngineKind::Local).send(final_message(1, "late"));
            h.turn();
            assert_no_submission(&mut h);
        }

        #[test]
        fn standard_disconnect_in_recording_prunes_owner_and_clears_active() {
            let (mut h, _) = ready();
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
            h.engine(EngineKind::Local).send(EngineMsg::Disconnected {
                reason: "standard failed".into(),
                failed_model: None,
            });
            h.turn();
            assert_eq!(h.controller.session.state(), State::Disconnected);
            assert!(h.controller.owners.is_empty());
            assert!(h.controller.active.is_none());
            assert!(rows(&h).is_empty());
            h.engine(EngineKind::Local).send(final_message(1, "late"));
            h.turn();
            assert_no_submission(&mut h);
        }

        #[test]
        fn final_without_owner_is_dropped() {
            let (mut h, _) = ready();
            utterance(&mut h, HotkeyKey::Standard);
            h.controller
                .submit_final(final_message(999, "missing owner"));
            assert_no_submission(&mut h);
            assert_eq!(h.controller.session.state(), State::Finalizing);
        }

        #[test]
        fn late_final_after_cancel_and_timeout_is_dropped() {
            let (mut h, _) = ready();
            utterance(&mut h, HotkeyKey::Standard);
            utterance(&mut h, HotkeyKey::Standard);
            h.engine(EngineKind::Local)
                .send(final_message(1, "canceled"));
            h.turn();
            assert_no_submission(&mut h);
            let outputs = h
                .controller
                .session
                .handle(Input::Tick, Instant::now() + Duration::from_secs(10));
            h.controller.execute_outputs(outputs);
            h.engine(EngineKind::Local)
                .send(final_message(2, "timeout"));
            h.turn();
            assert_no_submission(&mut h);
        }

        #[test]
        fn saving_haas_keeps_running_local_postprocess_and_punct_flags() {
            use kikigaki_core::config::AsrModel;
            let (mut h, rx) = ready();
            h.controller
                .apply_settings(SettingsPatch {
                    punctuation: Some(crate::settings::PunctSetting::On),
                    ..SettingsPatch::default()
                })
                .unwrap();
            let before = h.controller.settings_snapshot();
            assert!(before.local_punct);
            let saved = h
                .controller
                .apply_settings(SettingsPatch {
                    asr_model: Some(AsrModel::Haas),
                    ..SettingsPatch::default()
                })
                .unwrap();
            assert_eq!(saved.asr_model, AsrModel::Haas);
            assert_eq!(saved.asr_model_applied, AsrModel::ReazonSpeech);
            assert_eq!(saved.local_punct, before.local_punct);
            assert_eq!(saved.punct_choice, before.punct_choice);
            assert_eq!(saved.strip_trailing_period, before.strip_trailing_period);
            let mut outputs = Vec::new();
            h.controller
                .handle_hotkey_edge(HotkeyEdge::Pressed(HotkeyKey::Standard), &mut outputs);
            h.engine(EngineKind::Local).send(final_message(1, "甲、乙"));
            h.controller.handle_engine_messages(&mut outputs);
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
            h.controller
                .handle_hotkey_edge(HotkeyEdge::Released(HotkeyKey::Standard), &mut outputs);
            h.controller.handle_postprocess_events(&mut outputs);
            h.controller.execute_outputs(outputs);
            assert_eq!(*h.pastes.attempts.lock().unwrap(), ["甲、乙。"]);
        }

        #[test]
        fn finals_use_their_engines_postprocess_settings() {
            use crate::settings::PunctSetting;
            for (choice, expected) in [
                (PunctSetting::Off, "甲、乙"),
                (PunctSetting::On, "甲、乙。"),
                (PunctSetting::OnStrip, "甲、乙"),
            ] {
                let (mut h, rx) = ready();
                h.controller
                    .apply_settings(SettingsPatch {
                        punctuation: Some(choice),
                        ..SettingsPatch::default()
                    })
                    .unwrap();
                // Keep all generations' accepted messages in the FIFO before draining results.
                let mut outputs = Vec::new();
                for (gen, key, kind, raw) in [
                    (1, HotkeyKey::Standard, EngineKind::Local, "甲、乙"),
                    (2, HotkeyKey::Standard, EngineKind::Local, "甲、乙"),
                    (3, HotkeyKey::Standard, EngineKind::Local, "甲、乙"),
                ] {
                    h.controller
                        .handle_hotkey_edge(HotkeyEdge::Pressed(key), &mut outputs);
                    h.engine(kind).send(final_message(gen, raw));
                    h.controller.handle_engine_messages(&mut outputs);
                    // Session accepts the processed final and becomes Idle, but outputs stay queued.
                    rx.recv_timeout(Duration::from_secs(1)).unwrap();
                    h.controller
                        .handle_hotkey_edge(HotkeyEdge::Released(key), &mut outputs);
                    h.controller.handle_postprocess_events(&mut outputs);
                }
                h.controller.execute_outputs(outputs);
                assert_eq!(
                    *h.pastes.attempts.lock().unwrap(),
                    [expected, expected, expected]
                );
                assert_eq!(
                    rows(&h)
                        .iter()
                        .map(|row| row["engine"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["local", "local", "local"]
                );
            }
        }

        #[test]
        fn standard_failure_precedes_mic_failure_on_batched_start() {
            struct FailingMic;
            impl MicPort for FailingMic {
                fn start(&mut self, _sink: AudioSink) -> anyhow::Result<()> {
                    anyhow::bail!("mic failed")
                }
                fn stop(&mut self) {}
            }
            let (mut h, _) = ready();
            h.controller.mic = Box::new(FailingMic);
            let mut outputs = Vec::new();
            h.controller
                .handle_hotkey_edge(HotkeyEdge::Pressed(HotkeyKey::Standard), &mut outputs);
            h.engine(EngineKind::Local).send(EngineMsg::Disconnected {
                reason: "standard failed".into(),
                failed_model: None,
            });
            h.controller.handle_engine_messages(&mut outputs);
            h.controller.execute_outputs(outputs);
            assert_eq!(
                h.controller.status,
                Status::engine_failed("standard failed")
            );
            assert_eq!(h.controller.session.state(), State::Disconnected);
        }

        #[test]
        fn extra_multiple_processed_finals_in_one_turn_keep_recording_owner() {
            let (mut h, rx) = ready();
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
            for text in ["first", "second"] {
                h.engine(EngineKind::Local).send(final_message(1, text));
            }
            let mut outputs = Vec::new();
            h.controller.handle_engine_messages(&mut outputs);
            for _ in 0..2 {
                rx.recv_timeout(Duration::from_secs(1)).unwrap();
            }
            h.controller.handle_postprocess_events(&mut outputs);
            h.controller.execute_outputs(outputs);
            assert_eq!(*h.pastes.attempts.lock().unwrap(), ["first", "second"]);
            assert_eq!(rows(&h).len(), 2);
            assert_eq!(h.controller.active, Some(1));
            assert_eq!(h.controller.session.state(), State::Recording);
            assert!(!h.controller.retired.contains(&1));
        }

        #[test]
        fn extra_early_final_keeps_owner_and_starting_key_stops_the_mic() {
            let (mut h, rx) = ready();
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
            finish(&mut h, &rx, EngineKind::Local, 1, "early");
            assert_eq!(h.controller.active, Some(1));
            finish(&mut h, &rx, EngineKind::Local, 1, "another segment");
            edges(&mut h, [HotkeyEdge::Released(HotkeyKey::Standard)]);
            assert_eq!(h.controller.session.state(), State::Finalizing);
            finish(&mut h, &rx, EngineKind::Local, 1, "");
            assert_eq!(
                *h.pastes.attempts.lock().unwrap(),
                ["early", "another segment"]
            );
            assert_eq!(h.controller.session.state(), State::Idle);
            assert!(h.controller.active.is_none());
            assert!(h.controller.owners.is_empty());
            assert!(commands(&h, EngineKind::Local)
                .iter()
                .any(|cmd| matches!(cmd, EngineCmd::End { gen: 1, .. })));
        }

        #[test]
        fn extra_processed_final_after_abort_is_dropped() {
            let (mut h, rx) = ready();
            utterance(&mut h, HotkeyKey::Standard);
            h.engine(EngineKind::Local)
                .send(final_message(1, "queued before abort"));
            // Queue postprocessing without draining its completion in the same turn:
            // a fast worker could otherwise paste before the simulated disconnect.
            let mut outputs = Vec::new();
            h.controller.handle_engine_messages(&mut outputs);
            h.controller.execute_outputs(outputs);
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
            h.disconnected(None);
            h.turn();
            assert!(h.pastes.attempts.lock().unwrap().is_empty());
            assert!(rows(&h).is_empty());
        }

        #[test]
        fn standard_key_obeys_focus_and_capture_suppression() {
            for focused in [false, true] {
                let (mut h, _) = ready();
                h.controller.window_focused = focused;
                h.controller.capturing_hotkey = !focused;
                edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
                assert!(h.controller.owners.is_empty());
                assert_eq!(h.controller.session.state(), State::Idle);
            }
        }

        #[test]
        fn paste_failure_warning_is_published() {
            let (mut h, rx) = ready();
            h.controller.paster = Box::new(FakePaster(Arc::new(PasteState {
                reject_late: true,
                ..PasteState::default()
            })));
            h.controller.clock =
                Box::new(FakeClock(Some(Instant::now() - Duration::from_secs(10))));
            utterance(&mut h, HotkeyKey::Standard);
            finish(&mut h, &rx, EngineKind::Local, 1, "failed paste");
            assert_eq!(
                h.controller.snapshot().status.message.as_deref(),
                Some(PASTE_FAILED_NOTICE)
            );
            assert!(h.controller.owners.is_empty());
            assert_eq!(rows(&h)[0]["paste_failed"], true);
        }

        #[test]
        fn standard_warnings_and_install_progress_are_published() {
            let (mut h, _) = ready();
            for status in [
                Status::Warning(MICROPHONE_PERMISSION_DENIED.into()),
                Status::Warning("ホットキーが無効 — 再設定してください".into()),
                Status::Installing("downloading".into()),
            ] {
                h.controller.status = status.clone();
                assert_eq!(
                    h.controller.snapshot().status.message.as_deref(),
                    status.message()
                );
            }
        }

        #[test]
        fn postprocess_fifo_preserves_standard_settings_back_to_back() {
            use crate::settings::PunctSetting;
            for (choice, expected) in [
                (PunctSetting::Off, "甲、乙"),
                (PunctSetting::On, "甲、乙。"),
                (PunctSetting::OnStrip, "甲、乙"),
            ] {
                let (mut h, _) = ready();
                h.controller
                    .apply_settings(SettingsPatch {
                        punctuation: Some(choice),
                        ..SettingsPatch::default()
                    })
                    .unwrap();
                for (gen, text) in [(10, "甲、乙"), (11, "甲、乙"), (12, "甲、乙")] {
                    h.controller.owners.insert(gen);
                    h.controller.submit_final(final_message(gen, text));
                }
                let deadline = Instant::now() + Duration::from_secs(1);
                let mut texts = Vec::new();
                while texts.len() < 3 {
                    if let Some(result) = h.controller.postprocess.as_ref().unwrap().try_recv() {
                        texts.push(result.text);
                    }
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
                assert_eq!(texts, [expected, expected, expected]);
            }
        }

        #[test]
        fn transcript_and_metrics_record_owner_engine() {
            let (mut h, rx) = ready();
            h.controller
                .apply_settings(SettingsPatch {
                    transcript_log: Some(true),
                    ..SettingsPatch::default()
                })
                .unwrap();
            utterance(&mut h, HotkeyKey::Standard);
            finish(&mut h, &rx, EngineKind::Local, 1, "metadata");
            assert_eq!(rows(&h)[0]["engine"], "local");
            assert_eq!(transcripts(&h)[0]["engine"], "local");
        }
    }

    #[test]
    fn hotword_reconcile_restarts_only_standard() {
        let mut h = LifecycleHarness::new();
        h.boot(false);
        let (reply, rx) = mpsc::sync_channel(1);
        h.controller.handle_command(ControllerCmd::ApplySettings {
            patch: SettingsPatch {
                builtin_replace_dict: Some(true),
                ..SettingsPatch::default()
            },
            reply,
        });
        rx.try_recv().unwrap().unwrap();
        h.turn();
        assert_eq!(
            h.engine(EngineKind::Local).builds.load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            *h.engine(EngineKind::Local).observed_enabled.lock().unwrap(),
            [false, true]
        );
    }

    #[test]
    fn shutdown_with_standard_engine_delaying_meets_deadline() {
        let mut h = LifecycleHarness::new();
        h.boot(false);
        *h.engine(EngineKind::Local).shutdown_delay.lock().unwrap() = Some(Duration::from_secs(2));
        let started = Instant::now();
        h.controller.begin_shutdown();
        assert!(started.elapsed() < Duration::from_secs(5));
        let budgets = h.startup.join_budgets.lock().unwrap();
        assert_eq!(budgets.len(), 1);
        assert!(budgets[0] <= Duration::from_millis(2600));
        assert_eq!(
            h.engine(EngineKind::Local)
                .shutdown_threads
                .lock()
                .unwrap()
                .len(),
            1
        );
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
        // Fake startup and engine progress never wakes the controller, so drive
        // the loop with explicit wakes: a slow poll tick would outlast any wait.
        for _ in 0..300 {
            if latest(&harness).is_some_and(|snapshot| snapshot.status.state == "idle") {
                return harness;
            }
            harness.client.wake();
            thread::sleep(Duration::from_millis(10));
        }
        panic!("harness never became idle; latest={:?}", latest(&harness));
    }

    fn ready_harness_with_builtin_enabled() -> Harness {
        let harness = harness_with_initial_builtin_and_poll_interval(
            false,
            true,
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
            POLL_INTERVAL,
        );
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        harness
    }

    #[test]
    fn snapshot_serializes_the_compiled_platform() {
        let harness = ready_harness();
        let snapshot = latest(&harness).unwrap();
        assert_eq!(snapshot.platform, crate::platform::NAME);
        assert_eq!(
            serde_json::to_value(snapshot).unwrap()["platform"],
            crate::platform::NAME
        );
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

    /// Polls `condition` every 10ms for up to 3 seconds, returning whether it became true.
    fn poll(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..300 {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn transcribe(harness: &Harness, gen: u64, text: &str) {
        transcribe_with(harness, EngineKind::Local, gen, text);
    }

    fn transcribe_with(harness: &Harness, kind: EngineKind, gen: u64, text: &str) {
        let attempts = harness.pastes.attempts.lock().unwrap().len();
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(harness, |snapshot| snapshot.status.state == "recording");
        harness
            .hotkey
            .send(HotkeyEdge::Released(HotkeyKey::Standard));
        wait_snapshot(harness, |snapshot| snapshot.status.state == "finalizing");
        harness.engine(kind).send(final_message(gen, text));
        assert!(
            poll(|| harness.pastes.attempts.lock().unwrap().len() > attempts),
            "paste was not attempted"
        );
    }

    #[test]
    fn repress_while_finalizing_aborts_and_enqueues_cancel_before_the_next_begin() {
        let harness = ready_harness();
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "recording");
        harness
            .hotkey
            .send(HotkeyEdge::Released(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "finalizing");

        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "recording");

        assert_eq!(
            *harness.engine(EngineKind::Local).canceled.lock().unwrap(),
            vec![1]
        );
        let engine = harness.engine(EngineKind::Local);
        let receivers = engine.command_receivers.lock().unwrap();
        let commands: Vec<_> = receivers[0].lock().unwrap().try_iter().collect();
        assert_eq!(
            commands,
            vec![
                EngineCmd::Begin { gen: 1 },
                EngineCmd::End {
                    gen: 1,
                    pad_ms: 500,
                },
                EngineCmd::Cancel { gen: 1 },
                EngineCmd::Begin { gen: 2 },
            ]
        );
    }

    fn apply_builtin(harness: &Harness, enabled: bool) -> SettingsSnapshot {
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    builtin_replace_dict: Some(enabled),
                    ..SettingsPatch::default()
                },
                reply,
            })
            .unwrap()
    }

    fn wait_for_builds(harness: &Harness, expected: usize) {
        assert!(
            poll(|| harness
                .engines
                .iter()
                .map(|(_, state)| state.builds.load(Ordering::Relaxed))
                .sum::<usize>()
                == expected),
            "engine build count did not reach {expected}; actual={}",
            harness
                .engines
                .iter()
                .map(|(_, state)| state.builds.load(Ordering::Relaxed))
                .sum::<usize>()
        );
    }

    #[test]
    fn settings_preserve_standard_dictionary_during_recording() {
        let harness = ready_harness();
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(&harness, |s| s.status.state == "recording");
        apply_builtin(&harness, true);
        harness
            .hotkey
            .send(HotkeyEdge::Released(HotkeyKey::Standard));
        harness
            .engine(EngineKind::Local)
            .send(final_message(1, "クバネティス"));
        wait_snapshot(&harness, |s| s.status.state == "idle");
        assert_eq!(*harness.pastes.attempts.lock().unwrap(), ["Kubernetes"]);
    }

    fn startup_metric_count(harness: &Harness) -> usize {
        std::fs::read_to_string(&harness.metrics_path)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(r#""kind":"startup""#))
            .count()
    }

    fn crash_postprocess_worker(harness: &Harness) {
        harness.punctuator.panic.store(true, Ordering::Relaxed);
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(harness, |snapshot| snapshot.status.state == "recording");
        harness
            .engine(EngineKind::Local)
            .send(final_message(1, "crash"));
        harness.client.wake();
        for _ in 0..300 {
            if harness.punctuator.panicked.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(10));
                harness
                    .engine(EngineKind::Local)
                    .send(EngineMsg::Disconnected {
                        reason: "test worker cleanup".into(),
                        failed_model: None,
                    });
                harness.engine(EngineKind::Local).send(EngineMsg::Ready);
                harness.client.wake();
                wait_snapshot(harness, |snapshot| snapshot.status.state == "idle");
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("post-processing worker did not execute the scripted panic");
    }

    #[test]
    fn hotword_toggle_reconciles_once_on_next_idle_tick() {
        let harness = ready_harness();
        harness
            .engine(EngineKind::Local)
            .plans
            .lock()
            .unwrap()
            .push_back(FakeEngineBuild::Pending);

        assert!(apply_builtin(&harness, true).builtin_replace_dict);
        wait_for_builds(&harness, 2);
        thread::sleep(Duration::from_millis(60));

        assert_eq!(
            harness
                .engine(EngineKind::Local)
                .builds
                .load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            *harness
                .engine(EngineKind::Local)
                .observed_enabled
                .lock()
                .unwrap(),
            [false, true]
        );
    }

    #[test]
    fn hotword_toggle_waits_through_recording_and_finalizing() {
        let harness = ready_harness();
        harness
            .engine(EngineKind::Local)
            .plans
            .lock()
            .unwrap()
            .push_back(FakeEngineBuild::Pending);
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "recording");

        apply_builtin(&harness, true);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            harness
                .engine(EngineKind::Local)
                .builds
                .load(Ordering::Relaxed),
            1
        );

        harness
            .hotkey
            .send(HotkeyEdge::Released(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "finalizing");
        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            harness
                .engine(EngineKind::Local)
                .builds
                .load(Ordering::Relaxed),
            1
        );

        harness
            .engine(EngineKind::Local)
            .send(final_message(1, "done"));
        harness.client.wake();
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        wait_for_builds(&harness, 2);
        assert_eq!(
            *harness
                .engine(EngineKind::Local)
                .observed_enabled
                .lock()
                .unwrap(),
            [false, true]
        );
    }

    #[test]
    fn hotword_toggle_while_finalizing_waits_for_idle() {
        let harness = ready_harness();
        harness
            .engine(EngineKind::Local)
            .plans
            .lock()
            .unwrap()
            .push_back(FakeEngineBuild::Pending);
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "recording");
        harness
            .hotkey
            .send(HotkeyEdge::Released(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "finalizing");

        apply_builtin(&harness, true);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            harness
                .engine(EngineKind::Local)
                .builds
                .load(Ordering::Relaxed),
            1
        );

        harness
            .engine(EngineKind::Local)
            .send(final_message(1, "done"));
        harness.client.wake();
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        wait_for_builds(&harness, 2);
        assert_eq!(
            *harness
                .engine(EngineKind::Local)
                .observed_enabled
                .lock()
                .unwrap(),
            [false, true]
        );
    }

    #[test]
    fn hotword_flips_coalesce_while_engine_is_starting() {
        let harness = harness_with_engine_options(
            false,
            false,
            EngineKind::Local,
            VecDeque::from([FakeEngineBuild::Pending]),
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
            POLL_INTERVAL,
        );
        wait_for_builds(&harness, 1);

        apply_builtin(&harness, true);
        apply_builtin(&harness, false);
        apply_builtin(&harness, true);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            harness
                .engine(EngineKind::Local)
                .builds
                .load(Ordering::Relaxed),
            1
        );

        harness.engine(EngineKind::Local).send(EngineMsg::Ready);
        harness.client.wake();
        wait_for_builds(&harness, 2);
        assert_eq!(
            *harness
                .engine(EngineKind::Local)
                .observed_enabled
                .lock()
                .unwrap(),
            [false, true]
        );
    }

    #[test]
    fn hotword_flip_back_during_starting_cancels_reconcile() {
        let harness = harness_with_engine_options(
            false,
            false,
            EngineKind::Local,
            VecDeque::from([FakeEngineBuild::Pending]),
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
            POLL_INTERVAL,
        );
        wait_for_builds(&harness, 1);

        apply_builtin(&harness, true);
        apply_builtin(&harness, false);
        harness.engine(EngineKind::Local).send(EngineMsg::Ready);
        harness.client.wake();
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        thread::sleep(Duration::from_millis(60));

        assert_eq!(
            harness
                .engine(EngineKind::Local)
                .builds
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            *harness
                .engine(EngineKind::Local)
                .observed_enabled
                .lock()
                .unwrap(),
            [false]
        );
    }

    #[test]
    fn hotword_reconcile_failure_disconnects_stops_mic_and_retries() {
        let harness = ready_harness();
        harness
            .engine(EngineKind::Local)
            .plans
            .lock()
            .unwrap()
            .push_back(FakeEngineBuild::Fail("scripted build failure".into()));
        harness.mic.running.store(true, Ordering::Relaxed);
        let stops_before = harness.mic.stops.load(Ordering::Relaxed);

        apply_builtin(&harness, true);
        let failed = wait_snapshot(&harness, |snapshot| {
            snapshot.status.state == "disconnected"
                && snapshot
                    .status
                    .detail
                    .as_deref()
                    .is_some_and(|message| message.contains("scripted build failure"))
        });
        assert_eq!(failed.status.phase, "failed");
        assert!(!harness.mic.running.load(Ordering::Relaxed));
        assert!(harness.mic.stops.load(Ordering::Relaxed) > stops_before);

        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_for_builds(&harness, 4);
        assert_eq!(
            *harness
                .engine(EngineKind::Local)
                .observed_enabled
                .lock()
                .unwrap(),
            [false, true, false, true]
        );
    }

    #[test]
    fn postprocess_failure_survives_successful_engine_reconcile() {
        let harness = harness(
            true,
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
        );
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        crash_postprocess_worker(&harness);

        apply_builtin(&harness, true);
        wait_for_builds(&harness, 2);
        let snapshot = wait_snapshot(&harness, |snapshot| {
            snapshot
                .status
                .message
                .as_deref()
                .is_some_and(|message| message.contains("内蔵置換辞書"))
        });

        assert_eq!(snapshot.status.phase, "ready");
        assert_eq!(
            *harness
                .engine(EngineKind::Local)
                .observed_enabled
                .lock()
                .unwrap(),
            [false, true]
        );

        apply_builtin(&harness, true);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            harness
                .engine(EngineKind::Local)
                .builds
                .load(Ordering::Relaxed),
            2
        );
        assert!(latest(&harness)
            .unwrap()
            .status
            .message
            .is_some_and(|message| message.contains("内蔵置換辞書")));
    }

    #[test]
    fn engine_reconcile_failure_does_not_mark_postprocess_degraded() {
        let harness = ready_harness();
        harness
            .engine(EngineKind::Local)
            .plans
            .lock()
            .unwrap()
            .push_back(FakeEngineBuild::Fail("engine tier failed".into()));

        apply_builtin(&harness, true);
        let snapshot = wait_snapshot(&harness, |snapshot| {
            snapshot
                .status
                .detail
                .as_deref()
                .is_some_and(|message| message.contains("engine tier failed"))
        });

        assert_eq!(snapshot.status.phase, "failed");
        assert!(!snapshot.status.message.unwrap().contains("内蔵置換辞書"));
    }

    #[test]
    fn combined_status_names_postprocess_and_engine_failures() {
        let harness = harness(
            true,
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
        );
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        crash_postprocess_worker(&harness);
        harness
            .engine(EngineKind::Local)
            .plans
            .lock()
            .unwrap()
            .push_back(FakeEngineBuild::Fail("engine tier failed".into()));

        apply_builtin(&harness, true);
        let snapshot = wait_snapshot(&harness, |snapshot| {
            snapshot.status.detail.as_deref().is_some_and(|message| {
                message.contains("engine tier failed") && message.contains("内蔵置換辞書")
            })
        });

        assert_eq!(snapshot.status.phase, "failed");
    }

    #[test]
    fn remote_hotword_toggle_only_updates_postprocess() {
        let harness = harness_with_engine_options(
            false,
            false,
            EngineKind::Remote,
            VecDeque::new(),
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
            POLL_INTERVAL,
        );
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");

        apply_builtin(&harness, true);
        thread::sleep(Duration::from_millis(100));

        assert_eq!(
            harness
                .engine(EngineKind::Remote)
                .builds
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            *harness
                .engine(EngineKind::Remote)
                .observed_enabled
                .lock()
                .unwrap(),
            [false]
        );
        transcribe_with(&harness, EngineKind::Remote, 1, "クバネティス");
        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "Kubernetes"
        );
    }

    #[test]
    fn standard_postprocess_keeps_punctuation_enabled() {
        let harness = harness_with_engine_options(
            true,
            false,
            EngineKind::Local,
            VecDeque::new(),
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
            POLL_INTERVAL,
        );
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        transcribe_with(&harness, EngineKind::Local, 1, "こんにちは");

        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "こんにちは。"
        );
    }

    #[test]
    fn local_applies_builtin_replacement_dictionary() {
        let harness = harness_with_engine_options(
            false,
            false,
            EngineKind::Local,
            VecDeque::new(),
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
            POLL_INTERVAL,
        );
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
        apply_builtin(&harness, true);
        transcribe_with(&harness, EngineKind::Local, 1, "クバネティス");

        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "Kubernetes"
        );
    }

    #[test]
    fn stale_ready_does_not_clear_warning_or_consume_startup_metric() {
        let harness = harness_with_engine_options(
            false,
            false,
            EngineKind::Local,
            VecDeque::from([FakeEngineBuild::Pending, FakeEngineBuild::Pending]),
            BootstrapOutcome::ReadyToStart,
            vec![Err("new failed".into()), Err("old failed".into())],
            Vec::new(),
            false,
            None,
            None,
            POLL_INTERVAL,
        );
        wait_for_builds(&harness, 1);
        let error = harness
            .client
            .send_and_wait(|reply| ControllerCmd::EndHotkeyCapture {
                key: HotkeyKey::Standard,
                new_chord: Some("Cmd+Z".into()),
                reply,
            })
            .unwrap_err();
        assert_eq!(error.code, "register_failed");
        apply_builtin(&harness, true);

        harness.engine(EngineKind::Local).send(EngineMsg::Ready);
        harness.client.wake();
        wait_for_builds(&harness, 2);
        wait_snapshot(&harness, |snapshot| {
            snapshot.status.phase == "starting"
                && snapshot.status.message.as_deref()
                    == Some("ホットキーが無効 — 再設定してください")
        });
        assert_eq!(startup_metric_count(&harness), 0);

        harness.engine(EngineKind::Local).send(EngineMsg::Ready);
        harness.client.wake();
        wait_snapshot(&harness, |snapshot| {
            snapshot.status.state == "idle" && snapshot.status.message.is_none()
        });
        assert_eq!(startup_metric_count(&harness), 1);
    }

    #[test]
    fn reconnect_restart_preserves_pending_hotword_reconcile() {
        let harness = ready_harness();
        harness
            .engine(EngineKind::Local)
            .send(EngineMsg::Disconnected {
                reason: "connection lost".into(),
                failed_model: None,
            });
        harness.client.wake();
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "disconnected");
        harness
            .engine(EngineKind::Local)
            .plans
            .lock()
            .unwrap()
            .extend([FakeEngineBuild::Pending, FakeEngineBuild::Pending]);

        apply_builtin(&harness, true);
        assert_eq!(
            harness
                .engine(EngineKind::Local)
                .builds
                .load(Ordering::Relaxed),
            1
        );
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_for_builds(&harness, 2);
        assert_eq!(
            *harness
                .engine(EngineKind::Local)
                .observed_enabled
                .lock()
                .unwrap(),
            [false, false]
        );

        harness.engine(EngineKind::Local).send(EngineMsg::Ready);
        harness.client.wake();
        wait_for_builds(&harness, 3);
        let interim = wait_snapshot(&harness, |snapshot| snapshot.status.phase == "starting");
        assert_eq!(interim.status.state, "idle");
        assert_eq!(interim.status.detail.as_deref(), Some("connection lost"));
        assert_eq!(
            *harness
                .engine(EngineKind::Local)
                .observed_enabled
                .lock()
                .unwrap(),
            [false, false, true]
        );

        harness.engine(EngineKind::Local).send(EngineMsg::Ready);
        harness.client.wake();
        wait_snapshot(&harness, |snapshot| snapshot.status.message.is_none());
    }

    #[test]
    fn release_while_window_focused_is_honored() {
        let harness = ready_harness();
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "recording");
        harness.client.send(ControllerCmd::SetWindowFocused(true));
        thread::sleep(Duration::from_millis(30));

        harness
            .hotkey
            .send(HotkeyEdge::Released(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "finalizing");
        harness
            .engine(EngineKind::Local)
            .send(final_message(1, "done"));
        harness.client.wake();
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "idle");
    }

    #[test]
    fn release_during_hotkey_capture_is_honored() {
        let mut session = Session::new(SessionConfig {
            silence_pad_ms: 500,
            final_timeout: Duration::from_secs(3),
        });
        let now = Instant::now();
        session.handle(Input::Engine(Event::Ready { sr: SAMPLE_RATE }), now);
        let pressed = hotkey_input(HotkeyEdge::Pressed(HotkeyKey::Standard), false, false).unwrap();
        session.handle(pressed, now);
        assert_eq!(session.state(), State::Recording);

        let released =
            hotkey_input(HotkeyEdge::Released(HotkeyKey::Standard), false, true).unwrap();
        session.handle(released, now + Duration::from_millis(1));
        assert_eq!(session.state(), State::Finalizing);
        session.handle(
            Input::Engine(Event::Final(kikigaki_core::protocol::Final {
                gen: 1,
                text: String::new(),
                lang: String::new(),
                engine_latency_ms: None,
                dropped_chunks: 0,
                vad_close_to_asr_end_ms: None,
                postprocess_ms: None,
            })),
            now + Duration::from_millis(2),
        );
        assert_eq!(session.state(), State::Idle);
    }

    // The two tests below prove the controller handles an input on its own wake,
    // not on the next poll tick. The tick is far beyond the bound, so a missed wake still
    // fails, while the wide bound tolerates a heavily loaded test host.
    const SLOW_POLL_INTERVAL: Duration = Duration::from_secs(5);
    const WAKE_HANDLING_BOUND: Duration = Duration::from_secs(1);

    #[test]
    fn engine_final_is_processed_without_waiting_for_the_poll_tick() {
        let harness = ready_harness_with_poll_interval(SLOW_POLL_INTERVAL);
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "recording");
        harness
            .hotkey
            .send(HotkeyEdge::Released(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "finalizing");

        let started = Instant::now();
        harness
            .engine(EngineKind::Local)
            .send(final_message(1, "awake"));
        harness.client.wake();
        while harness.pastes.attempts.lock().unwrap().is_empty() {
            assert!(
                started.elapsed() < WAKE_HANDLING_BOUND,
                "engine final waited for the poll tick"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn hotkey_edge_is_handled_without_waiting_for_the_poll_tick() {
        let harness = ready_harness_with_poll_interval(SLOW_POLL_INTERVAL);
        let started = Instant::now();
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        while latest(&harness).unwrap().status.state != "recording" {
            assert!(
                started.elapsed() < WAKE_HANDLING_BOUND,
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

    #[test]
    fn generic_chord_patches_are_rejected_without_side_effects() {
        let mut h = capture_harness();
        let before = h.controller.settings_snapshot();
        let path = h._temp.path().join("config.toml");
        let raw = std::fs::read_to_string(&path).unwrap();
        let (reply, rx) = mpsc::sync_channel(1);
        let patch = SettingsPatch {
            hotkey: Some("Cmd+Space".into()),
            transcript_log: Some(true),
            ..SettingsPatch::default()
        };
        h.controller
            .handle_command(ControllerCmd::ApplySettings { patch, reply });
        let error = rx.try_recv().unwrap().unwrap_err();
        assert_eq!(error.code, "invalid_patch");
        assert_eq!(
            error.message,
            "キーの組み合わせはキー入力で変更してください"
        );
        assert_eq!(h.controller.settings_snapshot(), before);
        assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
        assert!(h.hotkeys.registered.lock().unwrap().is_empty());
        assert!(h.hotkeys.unregistered.lock().unwrap().is_empty());
    }

    fn apply_transcript_log(
        client: &ControllerClient,
        enabled: bool,
    ) -> Result<SettingsSnapshot, UiError> {
        client.send_and_wait(|reply| ControllerCmd::ApplySettings {
            patch: SettingsPatch {
                transcript_log: Some(enabled),
                ..SettingsPatch::default()
            },
            reply,
        })
    }

    #[test]
    fn apply_settings_request_reply_correlates_to_the_right_call() {
        let harness = ready_harness();
        let a = harness.client.clone();
        let b = harness.client.clone();
        let first = thread::spawn(move || apply_transcript_log(&a, true).unwrap().transcript_log);
        let second = thread::spawn(move || apply_transcript_log(&b, false).unwrap().transcript_log);
        assert!(first.join().unwrap());
        assert!(!second.join().unwrap());
    }

    #[test]
    fn concurrent_patches_apply_in_order() {
        let harness = ready_harness();
        let initial = wait_snapshot(&harness, |_| true).settings;
        assert!(!initial.transcript_log);
        let flipped = !initial.strip_trailing_period;
        let (a_tx, a_rx) = mpsc::sync_channel(1);
        let (b_tx, b_rx) = mpsc::sync_channel(1);
        harness
            .client
            .tx
            .send(ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    transcript_log: Some(true),
                    ..SettingsPatch::default()
                },
                reply: a_tx,
            })
            .unwrap();
        harness
            .client
            .tx
            .send(ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    strip_trailing_period: Some(flipped),
                    ..SettingsPatch::default()
                },
                reply: b_tx,
            })
            .unwrap();
        let a = a_rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        assert!(a.transcript_log);
        assert_ne!(
            a.strip_trailing_period, flipped,
            "the second patch ran first"
        );
        // The second reply already carries the first patch: they applied in send order.
        let b = b_rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        assert!(b.transcript_log);
        assert_eq!(b.strip_trailing_period, flipped);
        wait_snapshot(&harness, |snapshot| {
            snapshot.settings.transcript_log && snapshot.settings.strip_trailing_period == flipped
        });
    }

    #[test]
    fn standard_engine_start_failure_enters_failed_and_retries() {
        let harness = harness_with_engine_options(
            false,
            false,
            EngineKind::Local,
            VecDeque::from([FakeEngineBuild::Fail("initial start failed".into())]),
            BootstrapOutcome::ReadyToStart,
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
            POLL_INTERVAL,
        );
        let failed = wait_snapshot(&harness, |s| s.status.phase == "failed");
        assert_eq!(failed.status.state, "disconnected");
        assert!(failed
            .status
            .detail
            .as_deref()
            .unwrap()
            .contains("initial start failed"));
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(&harness, |s| {
            s.status.phase == "ready" && s.status.state == "idle"
        });
        transcribe(&harness, 1, "retried");
        assert_eq!(*harness.pastes.attempts.lock().unwrap(), ["retried"]);
    }

    #[test]
    fn capture_standard_accepts_retired_chord() {
        let mut h = capture_harness();
        let path = h._temp.path().join("config.toml");
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.insert_str(0, "long_hotkey = \"Alt+Shift+Space\"\n");
        std::fs::write(&path, raw).unwrap();
        let capabilities = h.controller.capabilities;
        h.controller.settings = SettingsCoordinator::load(path.clone(), capabilities).unwrap();
        begin_capture(&mut h, HotkeyKey::Standard).unwrap();
        let snapshot = end_capture(&mut h, HotkeyKey::Standard, Some("Alt+Shift+Space")).unwrap();
        assert_eq!(snapshot.hotkey, "Alt+Shift+Space");
        // The file still carries the retired key with the same chord; the next launch validates it.
        let reloaded = Config::load(Some(&path)).unwrap();
        assert_eq!(reloaded.engine, EngineKind::Local);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("long_hotkey = \"Alt+Shift+Space\""));
        reloaded.validate(capabilities).unwrap();
    }

    #[test]
    fn capture_standard_round_trip_persists_and_registers_chord() {
        let mut h = capture_harness();
        begin_capture(&mut h, HotkeyKey::Standard).unwrap();
        let snapshot = end_capture(&mut h, HotkeyKey::Standard, Some("Cmd+Z")).unwrap();
        assert_eq!(snapshot.hotkey, "Cmd+Z");
        let reloaded = Config::load(Some(&h._temp.path().join("config.toml"))).unwrap();
        assert_eq!(reloaded.hotkey, "Cmd+Z");
        assert_eq!(
            *h.hotkeys.registered.lock().unwrap(),
            [(HotkeyKey::Standard, "Cmd+Z".into())]
        );
        assert!(!h.controller.capturing_hotkey);
    }

    #[test]
    fn hotkey_wire_accepts_standard_and_rejects_retired_variant() {
        assert_eq!(
            serde_json::from_str::<HotkeyKey>(r#""standard""#).unwrap(),
            HotkeyKey::Standard
        );
        let error = serde_json::from_str::<HotkeyKey>(r#""long""#).unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }

    fn capture_harness() -> LifecycleHarness {
        let mut h = LifecycleHarness::new();
        h.controller.session.handle(
            Input::Engine(Event::Ready { sr: SAMPLE_RATE }),
            Instant::now(),
        );
        h
    }

    fn begin_capture(h: &mut LifecycleHarness, key: HotkeyKey) -> Result<(), UiError> {
        let (reply, rx) = mpsc::sync_channel(1);
        h.controller
            .handle_command(ControllerCmd::BeginHotkeyCapture { key, reply });
        rx.try_recv().unwrap()
    }

    fn end_capture(
        h: &mut LifecycleHarness,
        key: HotkeyKey,
        new_chord: Option<&str>,
    ) -> Result<SettingsSnapshot, UiError> {
        let (reply, rx) = mpsc::sync_channel(1);
        h.controller
            .handle_command(ControllerCmd::EndHotkeyCapture {
                key,
                new_chord: new_chord.map(str::to_owned),
                reply,
            });
        rx.try_recv().unwrap()
    }

    fn assert_standard_restored(h: &LifecycleHarness) {
        assert_eq!(
            *h.hotkeys.registered.lock().unwrap(),
            [(HotkeyKey::Standard, "Alt+Space".into())]
        );
        assert!(!h.controller.capturing_hotkey);
    }

    #[test]
    fn capture_unregisters_and_restores_standard_chord() {
        for chord in [None, Some("Alt+Space")] {
            let mut h = capture_harness();
            begin_capture(&mut h, HotkeyKey::Standard).unwrap();
            assert_eq!(
                *h.hotkeys.unregistered.lock().unwrap(),
                [HotkeyKey::Standard]
            );
            assert!(h.controller.capturing_hotkey);
            end_capture(&mut h, HotkeyKey::Standard, chord).unwrap();
            assert_standard_restored(&h);
        }
    }

    #[test]
    fn capture_persist_failure_restores_standard() {
        let mut h = capture_harness();
        begin_capture(&mut h, HotkeyKey::Standard).unwrap();
        std::fs::write(h._temp.path().join("config.toml"), "# external edit\n").unwrap();
        let error = end_capture(&mut h, HotkeyKey::Standard, Some("Cmd+Z")).unwrap_err();
        assert_eq!(error.code, "external_change");
        assert_eq!(
            *h.hotkeys.registered.lock().unwrap(),
            [
                (HotkeyKey::Standard, "Cmd+Z".into()),
                (HotkeyKey::Standard, "Alt+Space".into())
            ]
        );
        assert_eq!(h.controller.settings.config().hotkey, "Alt+Space");
        assert!(!h.controller.capturing_hotkey);
    }

    #[test]
    fn capture_registration_failure_restores_standard_and_warns_on_restore_failure() {
        for fail_restore in [false, true] {
            let mut h = capture_harness();
            begin_capture(&mut h, HotkeyKey::Standard).unwrap();
            h.hotkeys.plan.lock().unwrap().extend([
                Err("new failed".into()),
                if fail_restore {
                    Err("restore failed".into())
                } else {
                    Ok(())
                },
            ]);
            let error = end_capture(&mut h, HotkeyKey::Standard, Some("Cmd+Z")).unwrap_err();
            assert_eq!(error.code, "register_failed");
            assert_eq!(error.message, "new failed");
            assert_eq!(
                *h.hotkeys.registered.lock().unwrap(),
                [
                    (HotkeyKey::Standard, "Cmd+Z".into()),
                    (HotkeyKey::Standard, "Alt+Space".into())
                ]
            );
            if fail_restore {
                assert_eq!(
                    h.controller.status.message(),
                    Some("ホットキーが無効 — 再設定してください")
                );
            }
        }
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
            .send_and_wait(|reply| ControllerCmd::BeginHotkeyCapture {
                key: HotkeyKey::Standard,
                reply,
            })
            .unwrap();
        let error = harness
            .client
            .send_and_wait(|reply| ControllerCmd::EndHotkeyCapture {
                key: HotkeyKey::Standard,
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
                key: HotkeyKey::Standard,
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
            .send_and_wait(|reply| ControllerCmd::BeginHotkeyCapture {
                key: HotkeyKey::Standard,
                reply,
            })
            .unwrap();
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::EndHotkeyCapture {
                key: HotkeyKey::Standard,
                new_chord: Some("Alt+Space".into()),
                reply,
            })
            .unwrap();

        assert_eq!(
            *harness.hotkeys.unregistered.lock().unwrap(),
            [HotkeyKey::Standard]
        );
        assert_eq!(
            *harness.hotkeys.registered.lock().unwrap(),
            [(HotkeyKey::Standard, "Alt+Space".into())]
        );

        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
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
            .engine(EngineKind::Local)
            .send(EngineMsg::Disconnected {
                reason: "bad model".into(),
                failed_model: Some("asr"),
            });
        let snapshot = wait_snapshot(&harness, |snapshot| {
            snapshot.status.detail.as_deref() == Some("bad model")
        });
        assert_eq!(
            snapshot.status.message.as_deref(),
            Some("音声エンジンを起動できませんでした")
        );
        assert_eq!(snapshot.status.phase, "failed");
        assert_eq!(*harness.startup.invalidated.lock().unwrap(), ["asr"]);

        let harness = ready_harness();
        harness
            .engine(EngineKind::Local)
            .send(EngineMsg::Disconnected {
                reason: "engine stopped".into(),
                failed_model: None,
            });
        let snapshot = wait_snapshot(&harness, |snapshot| {
            snapshot.status.detail.as_deref() == Some("engine stopped")
        });
        assert_eq!(
            snapshot.status.message.as_deref(),
            Some("音声エンジンが停止しました")
        );
        assert_eq!(snapshot.status.phase, "failed");
        assert!(harness.startup.invalidated.lock().unwrap().is_empty());
    }

    #[test]
    fn first_ready_records_startup_metric_once() {
        let harness = ready_harness();
        harness.engine(EngineKind::Local).send(EngineMsg::Ready);

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
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        wait_snapshot(&harness, |snapshot| snapshot.status.state == "recording");
        let error = harness
            .client
            .send_and_wait(|reply| ControllerCmd::BeginHotkeyCapture {
                key: HotkeyKey::Standard,
                reply,
            })
            .unwrap_err();
        assert_eq!(error.code, "not_idle");
    }

    #[test]
    fn builtin_dictionary_toggle_changes_live_transcription_output() {
        let harness = ready_harness();
        transcribe(&harness, 1, "クバネティス");
        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "クバネティス"
        );

        harness
            .client
            .send_and_wait(|reply| ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    builtin_replace_dict: Some(true),
                    ..SettingsPatch::default()
                },
                reply,
            })
            .unwrap();

        transcribe(&harness, 2, "クバネティス");
        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "Kubernetes"
        );
    }

    #[test]
    fn builtin_dictionary_enabled_before_worker_creation_applies_to_first_transcription() {
        let harness = ready_harness_with_builtin_enabled();

        transcribe(&harness, 1, "クバネティス");

        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "Kubernetes"
        );
    }

    #[test]
    fn externally_rejected_builtin_patch_leaves_live_worker_behavior_unchanged() {
        let harness = ready_harness();
        transcribe(&harness, 1, "クバネティス");
        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "クバネティス"
        );

        let config_path = harness._temp.path().join("config.toml");
        let mut externally_edited = std::fs::read_to_string(&config_path).unwrap();
        externally_edited.push_str("# external edit\n");
        std::fs::write(config_path, externally_edited).unwrap();
        let error = harness
            .client
            .send_and_wait(|reply| ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    builtin_replace_dict: Some(true),
                    ..SettingsPatch::default()
                },
                reply,
            })
            .unwrap_err();
        assert_eq!(error.code, "external_change");

        transcribe(&harness, 2, "クバネティス");
        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "クバネティス"
        );
    }

    #[test]
    fn window_focus_suppresses_the_hotkey() {
        let harness = ready_harness();
        harness.client.send(ControllerCmd::SetWindowFocused(true));
        thread::sleep(Duration::from_millis(50));
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(latest(&harness).unwrap().status.state, "idle");
        assert_eq!(harness.mic.starts.load(Ordering::Relaxed), 0);
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
                        ..SettingsPatch::default()
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
        assert_eq!(snapshot.status.detail.as_deref(), Some("offline"));
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

#[cfg(test)]
mod mic_warning_policy_tests {
    use super::*;
    use crate::permissions::MicPermission::*;

    #[test]
    fn only_denied_claims_permission_denial() {
        assert_eq!(
            microphone_warning(Denied, true).unwrap().0,
            MICROPHONE_PERMISSION_DENIED
        );
        for permission in [Authorized, NotDetermined, Unknown, Restricted] {
            assert!(microphone_warning(permission, true).is_none());
        }
        assert_eq!(
            microphone_warning(Busy, true).unwrap().1,
            "ほかのアプリがマイクを使っています。閉じてから、もう一度確認してください。"
        );
        assert_eq!(
            microphone_warning(Unavailable, true).unwrap().1,
            "マイクが見つかりません。接続してから、もう一度確認してください。"
        );
    }

    #[test]
    fn captured_audio_clears_only_the_controllers_own_mic_warning() {
        let mic = crate::status::StatusText::new("マイクを使えません", "device error");
        let own = Status::Warning(mic.clone());
        assert!(!clear_captured_mic_warning(&own, Some(&mic), false));
        assert!(clear_captured_mic_warning(&own, Some(&mic), true));
        assert!(!clear_captured_mic_warning(&own, None, true));
        assert!(!clear_captured_mic_warning(
            &Status::Warning("other warning".into()),
            Some(&mic),
            true
        ));
        assert!(!clear_captured_mic_warning(
            &Status::engine_failed("engine error"),
            Some(&mic),
            true
        ));
    }

    #[test]
    fn macos_warning_policy_is_unchanged() {
        for permission in [
            NotDetermined,
            Restricted,
            Denied,
            Busy,
            Unavailable,
            Unknown,
        ] {
            assert_eq!(
                microphone_warning(permission, false).unwrap().0,
                MICROPHONE_PERMISSION_DENIED
            );
        }
        assert!(microphone_warning(Authorized, false).is_none());
    }
}
