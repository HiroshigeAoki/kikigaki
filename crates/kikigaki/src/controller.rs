//! Tauri-free push-to-talk controller and its injected platform ports.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
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
use kikigaki_core::startup::{StartupEvent, StartupPlan};

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
    Long,
}

pub enum HotkeyEdge {
    Pressed(HotkeyKey),
    Released(HotkeyKey),
}

pub enum ControllerCmd {
    /// Delivered by event producers so the run loop drains their queues without waiting for the
    /// poll tick.
    Wake,
    ApplySettings {
        patch: SettingsPatch,
        reply: ReplyTx<crate::settings::SettingsSnapshot>,
    },
    ClearHistory {
        reply: ReplyTx<()>,
    },
    RetryLongEngine {
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
    fn start(&mut self, sink: kikigaki_core::engine::AudioSink) -> anyhow::Result<()>;
    fn stop(&mut self);
}

pub trait PostprocessFactory: Send {
    fn build(
        &self,
        learned: std::sync::Arc<kikigaki_core::replace::Rules>,
        builtin_enabled: bool,
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

pub type StartupPortFactory = Box<dyn Fn(StartupPlan) -> Box<dyn StartupPort> + Send>;

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

fn long_key_off() -> UiError {
    UiError {
        code: "long_key_off",
        message: "長文キーが無効です".into(),
    }
}

pub struct ControllerConfig {
    pub settings: SettingsCoordinator,
    pub capabilities: Capabilities,
    pub models_dir: PathBuf,
    pub bootstrap: BootstrapOutcome,
    pub process_started_at: Instant,
    pub long_hotkey_registered: Option<bool>,
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
    long_install_factory: StartupPortFactory,
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
        long_install_factory,
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
    long_install_factory: StartupPortFactory,
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
                long_install_factory,
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
    long_install_factory: StartupPortFactory,
    clock: Box<dyn Clock>,
    controller_client: ControllerClient,
    poll_interval: Duration,
) {
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
        long_install_factory,
        clock,
        waker,
    );

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
        controller.reconcile_hotwords(&mut outputs);
        controller.handle_startup_events(&mut outputs);
        controller.handle_engine_messages(&mut outputs);
        controller.handle_long_events(&mut outputs);
        controller.handle_postprocess_events(&mut outputs);
        outputs.extend(
            controller
                .session
                .handle(Input::Tick, controller.clock.now()),
        );
        controller.execute_outputs(std::mem::take(&mut outputs));
        controller.apply_standard_disconnect();
        controller.reconcile_long();
        controller.publish_if_changed();
    }
}

#[derive(Debug, Clone, Copy)]
struct Owner {
    key: HotkeyKey,
    engine: EngineKind,
}

const LONG_HOTKEY_INVALID_NOTICE: &str = "長文キーのホットキーが無効 — 再設定してください";
const LONG_PREPARING_NOTICE: &str = "長文エンジン準備中のため標準で認識しました";
const LONG_STOPPED_NOTICE: &str = "長文エンジンが停止中のため標準で認識しました";
const LONG_INSTALL_FAILED_NOTICE: &str =
    "長文キーのモデルを準備できませんでした — もう一度お試しください";
const LONG_FAILED_NOTICE: &str = "長文エンジンが停止しました — 設定から再試行できます";
const PASTE_FAILED_NOTICE: &str = "Accessibility permission denied — System Settings › Privacy";

#[derive(Debug)]
enum LongState {
    Installing { percent: u8 },
    Loading,
    Ready,
    Failed { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LongOrigin {
    Boot,
    Settings,
}

struct LongEngine {
    supervisor: Option<EngineSupervisor>,
    install: Option<Box<dyn StartupPort>>,
    state: LongState,
    origin: LongOrigin,
    installed: bool,
    install_terminal: bool,
}

struct RetiringInstall {
    port: Box<dyn StartupPort>,
    terminal: bool,
}

struct Controller {
    settings: SettingsCoordinator,
    session: Session,
    mic: Box<dyn MicPort>,
    supervisor: Option<EngineSupervisor>,
    engine_factory: Arc<dyn EngineFactory>,
    startup: Box<dyn StartupPort>,
    long_install_factory: StartupPortFactory,
    long: Option<LongEngine>,
    long_desired: bool,
    long_origin: LongOrigin,
    long_start_latched: bool,
    long_disable_pending: bool,
    long_hotkey_registered: bool,
    long_notice: Option<String>,
    // Preserve a failure while a completed fallback is shown until the next press.
    long_notice_under_fallback: Option<String>,
    retiring: Vec<std::thread::JoinHandle<()>>,
    retiring_installs: Vec<RetiringInstall>,
    standard_engine: EngineKind,
    capabilities: Capabilities,
    models_dir: PathBuf,
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
    owners: HashMap<u64, Owner>,
    pending_notices: HashMap<u64, String>,
    // Tombstones protect this turn; after pruning, a missing owner rejects later finals.
    retired: HashSet<u64>,
    active: Option<u64>,
    standard_disconnect_pending: bool,
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
        long_install_factory: StartupPortFactory,
        clock: Box<dyn Clock>,
        waker: Waker,
    ) -> (Self, bool) {
        let ControllerConfig {
            settings,
            capabilities,
            models_dir,
            bootstrap,
            process_started_at,
            long_hotkey_registered,
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
        let long_desired = settings.config().long_key_effective(capabilities);
        let long_registered = long_desired && long_hotkey_registered != Some(false);
        let controller = Self {
            settings,
            session,
            mic,
            supervisor: Some(supervisor),
            engine_factory,
            startup,
            long_install_factory,
            long: None,
            long_desired,
            long_origin: LongOrigin::Boot,
            long_start_latched: false,
            long_disable_pending: false,
            long_hotkey_registered: long_registered,
            long_notice_under_fallback: None,
            long_notice: (long_desired && long_hotkey_registered == Some(false))
                .then(|| LONG_HOTKEY_INVALID_NOTICE.into()),
            retiring: Vec::new(),
            retiring_installs: Vec::new(),
            standard_engine,
            capabilities,
            models_dir,
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
            owners: HashMap::new(),
            pending_notices: HashMap::new(),
            retired: HashSet::new(),
            active: None,
            standard_disconnect_pending: false,
            hotwords_desired: hotwords_enabled,
            hotwords_applied: hotwords_enabled,
            reconcile_pending: false,
            engine_build_generation: 0,
            postprocess_degraded: None,
            shutting_down: false,
        };

        (controller, auto_start)
    }

    fn unregister_long_hotkey(&mut self) {
        if let Err(message) = self.hotkeys.unregister(HotkeyKey::Long) {
            tracing::warn!(%message, "unregister long hotkey");
            self.long_notice = Some(LONG_HOTKEY_INVALID_NOTICE.into());
        }
    }

    fn set_long_notice(&mut self, notice: Option<String>) {
        self.long_notice = notice;
        self.long_notice_under_fallback = None;
        self.dirty = true;
    }

    fn cancel_generation(&mut self, gen: u64, engine: EngineKind) {
        if let Some(supervisor) = self.supervisor_for(engine) {
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
            ControllerCmd::ApplySettings { patch, reply } => {
                let result = self.apply_settings(patch);
                let _ = reply.send(result);
            }
            ControllerCmd::ClearHistory { reply } => {
                self.history.clear();
                let _ = reply.send(Ok(()));
            }
            ControllerCmd::RetryLongEngine { reply } => {
                let result = self.retry_long();
                let _ = reply.send(result);
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
            ControllerCmd::BeginHotkeyCapture { key, reply } => {
                if key == HotkeyKey::Long
                    && !self.settings.config().long_key_effective(self.capabilities)
                {
                    let _ = reply.send(Err(long_key_off()));
                    return;
                }
                if self.session.state() != State::Idle {
                    let _ = reply.send(Err(UiError {
                        code: "not_idle",
                        message: "聞き取り中はホットキーを変更できません".into(),
                    }));
                    return;
                }
                let result = self
                    .hotkeys
                    .unregister(HotkeyKey::Standard)
                    .and_then(|()| self.hotkeys.unregister(HotkeyKey::Long));
                match result {
                    Ok(()) => {
                        self.long_hotkey_registered = false;
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
                            self.status =
                                Status::StartupFailed(format!("Startup failed — {error}"));
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
        let result = self.hotkeys.register(key, chord);
        if key == HotkeyKey::Long {
            self.long_hotkey_registered = result.is_ok();
            if result.is_ok() {
                self.clear_long_hotkey_notice();
            }
        }
        result.map_err(|message| {
            match key {
                HotkeyKey::Standard => {
                    self.status = Status::Warning("ホットキーが無効 — 再設定してください".into());
                }
                HotkeyKey::Long => {
                    self.set_long_notice(Some(LONG_HOTKEY_INVALID_NOTICE.into()));
                }
            }
            register_failed(message)
        })
    }

    fn clear_long_hotkey_notice(&mut self) {
        if self.long_notice.as_deref() == Some(LONG_HOTKEY_INVALID_NOTICE) {
            self.long_notice = None;
        }
        if self.long_notice_under_fallback.as_deref() == Some(LONG_HOTKEY_INVALID_NOTICE) {
            self.long_notice_under_fallback = None;
        }
    }

    fn capture_restores_long(&self) -> bool {
        self.settings.config().long_key_effective(self.capabilities) && !self.long_disable_pending
    }

    fn restore_capture_hotkeys(&mut self) -> Result<(), UiError> {
        let standard = self.settings.config().hotkey.clone();
        let long = self.settings.config().long_hotkey.clone();
        let standard_result = self.reregister_or_warn(HotkeyKey::Standard, &standard);
        // Attempt both restores even if Standard failed, so each key gets its own warning.
        let long_result = if self.capture_restores_long() {
            self.reregister_or_warn(HotkeyKey::Long, &long)
        } else {
            self.long_hotkey_registered = false;
            Ok(())
        };
        standard_result.and(long_result)
    }

    fn end_hotkey_capture(
        &mut self,
        key: HotkeyKey,
        new_chord: Option<String>,
        reply: ReplyTx<crate::settings::SettingsSnapshot>,
    ) {
        let was_capturing = self.capturing_hotkey;
        self.capturing_hotkey = false;
        if key == HotkeyKey::Long && !self.settings.config().long_key_effective(self.capabilities) {
            if was_capturing {
                let _ = self.restore_capture_hotkeys();
            }
            let _ = reply.send(Err(long_key_off()));
            return;
        }
        let (old, other_key, other_chord) = match key {
            HotkeyKey::Standard => (
                self.settings.config().hotkey.clone(),
                HotkeyKey::Long,
                self.settings.config().long_hotkey.clone(),
            ),
            HotkeyKey::Long => (
                self.settings.config().long_hotkey.clone(),
                HotkeyKey::Standard,
                self.settings.config().hotkey.clone(),
            ),
        };
        let Some(chord) = new_chord else {
            let result = self.restore_capture_hotkeys();
            let _ = reply.send(result.map(|()| self.settings_snapshot()));
            return;
        };
        // Match Config::validate, even when the other key is disabled.
        if chord.trim().eq_ignore_ascii_case(other_chord.trim()) {
            let _ = self.restore_capture_hotkeys();
            let _ = reply.send(Err(UiError {
                code: "hotkey_conflict",
                message: "もう一方のキーと同じ組み合わせは使えません".into(),
            }));
            return;
        }
        if chord == old {
            let result = if was_capturing {
                self.restore_capture_hotkeys()
            } else {
                Ok(())
            };
            let _ = reply.send(result.map(|()| self.settings_snapshot()));
            return;
        }
        let registration = if key == HotkeyKey::Standard || self.capture_restores_long() {
            self.hotkeys.register(key, &chord).map_err(register_failed)
        } else {
            Ok(())
        };
        if let Err(error) = registration {
            let _ = self.restore_capture_hotkeys();
            let _ = reply.send(Err(error));
            return;
        }
        if key == HotkeyKey::Long {
            self.long_hotkey_registered = self.capture_restores_long();
            if self.long_hotkey_registered {
                self.clear_long_hotkey_notice();
            }
        }
        if other_key == HotkeyKey::Standard || self.capture_restores_long() {
            if let Err(error) = self.reregister_or_warn(other_key, &other_chord) {
                let _ = self.restore_capture_hotkeys();
                let _ = reply.send(Err(error));
                return;
            }
        }
        let patch = match key {
            HotkeyKey::Standard => SettingsPatch {
                hotkey: Some(chord.clone()),
                ..SettingsPatch::default()
            },
            HotkeyKey::Long => SettingsPatch {
                long_hotkey: Some(chord.clone()),
                ..SettingsPatch::default()
            },
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
        let (key, pressed) = match edge {
            HotkeyEdge::Pressed(key) => (key, true),
            HotkeyEdge::Released(key) => (key, false),
        };
        let active_owner = self.active.and_then(|gen| self.owners.get(&gen)).copied();
        if !pressed && active_owner.is_none_or(|owner| owner.key != key) {
            return;
        }
        let mut notice = None;
        let engine = if !pressed {
            active_owner.unwrap().engine
        } else if self.session.state() == State::Disconnected
            || key == HotkeyKey::Standard
            || self.long_disable_pending
        {
            self.standard_engine
        } else {
            match self.long.as_ref().map(|long| &long.state) {
                Some(LongState::Ready) => EngineKind::Whisper,
                Some(LongState::Installing { .. } | LongState::Loading) => {
                    notice = Some(LONG_PREPARING_NOTICE.to_owned());
                    self.standard_engine
                }
                Some(LongState::Failed { .. }) => {
                    notice = Some(LONG_STOPPED_NOTICE.to_owned());
                    self.standard_engine
                }
                None => return,
            }
        };
        let Some(input) = hotkey_input(edge, self.window_focused, self.capturing_hotkey, engine)
        else {
            return;
        };
        self.dirty = true;
        if pressed && self.long_notice.as_deref().is_some_and(is_fallback_notice) {
            self.long_notice = self.long_notice_under_fallback.take();
        }
        let edge_outputs = self.session.handle(input, self.clock.now());
        // Bind each edge's Begin before another edge or engine message can be handled.
        for output in &edge_outputs {
            match output {
                Output::Cancel { gen } => self.retire_generation(*gen),
                Output::Begin { gen } => {
                    self.owners.insert(*gen, Owner { key, engine });
                    if let Some(notice) = notice.take() {
                        self.pending_notices.insert(*gen, notice);
                    }
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

    fn active_engine_is_whisper(&self) -> bool {
        self.active
            .and_then(|gen| self.owners.get(&gen))
            .is_some_and(|owner| owner.engine == EngineKind::Whisper)
    }

    fn owner_engine(&self, gen: u64) -> EngineKind {
        self.owners.get(&gen).map_or_else(
            || {
                tracing::warn!(gen, "missing utterance owner; using standard engine");
                self.standard_engine
            },
            |owner| owner.engine,
        )
    }

    fn supervisor_for(&self, engine: EngineKind) -> Option<&EngineSupervisor> {
        if engine == EngineKind::Whisper {
            self.long.as_ref().and_then(|long| long.supervisor.as_ref())
        } else {
            self.supervisor.as_ref()
        }
    }

    fn submit_final(&self, engine: EngineKind, message: EngineMsg) {
        let EngineMsg::Final { gen, .. } = &message else {
            return;
        };
        let gen = *gen;
        let Some(owner) = self
            .owners
            .get(&gen)
            .filter(|owner| owner.engine == engine && !self.retired.contains(&gen))
        else {
            tracing::debug!(gen, ?engine, "dropping final without a live matching owner");
            return;
        };
        if let Some(worker) = &self.postprocess {
            let result = worker.submit(
                gen,
                message,
                postprocess_settings(self.settings.config(), self.capabilities, owner.engine),
            );
            if let Err(error) = result {
                tracing::error!(%error, "submit configured final for post-processing");
            }
        } else {
            tracing::error!(gen, "post-processing is unavailable");
        }
    }

    fn standard_disconnected(&mut self, outputs: &mut Vec<Output>) {
        if self.active_engine_is_whisper() {
            self.standard_disconnect_pending = true;
            return;
        }
        if let Some(gen) = self.active {
            self.retire_generation(gen);
        }
        outputs.extend(
            self.session
                .handle(Input::EngineDisconnected, self.clock.now()),
        );
    }

    fn apply_standard_disconnect(&mut self) {
        if self.standard_disconnect_pending && !self.active_engine_is_whisper() {
            self.standard_disconnect_pending = false;
            let mut outputs = Vec::new();
            self.standard_disconnected(&mut outputs);
            self.execute_outputs(outputs);
        }
    }

    fn complete_notice(&mut self, gen: u64) {
        if let Some(notice) = self.pending_notices.remove(&gen) {
            if self
                .long_notice
                .as_deref()
                .is_some_and(|notice| !is_fallback_notice(notice))
            {
                self.long_notice_under_fallback = self.long_notice.take();
            }
            self.long_notice = Some(notice);
        }
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
                self.status = Status::EngineFailed(format!("Engine restart failed — {error}"));
                self.standard_disconnected(outputs);
            }
        }
        self.dirty = true;
    }

    fn apply_settings(
        &mut self,
        patch: SettingsPatch,
    ) -> Result<crate::settings::SettingsSnapshot, UiError> {
        let requested = patch.long_key;
        if requested == Some(true) && !self.capabilities.long_key {
            return Err(UiError {
                code: "unsupported_engine",
                message: "この機能はこのビルドでは利用できません".into(),
            });
        }
        if requested == Some(true) && self.status.is_startup_failed() && self.postprocess.is_none()
        {
            return Err(UiError {
                code: "engine_not_started",
                message: "起動が完了していないため、長文キーを有効にできません".into(),
            });
        }
        let newly_registered = requested == Some(true) && !self.long_hotkey_registered;
        if newly_registered {
            let chord = patch
                .long_hotkey
                .as_deref()
                .unwrap_or(&self.settings.config().long_hotkey);
            self.hotkeys
                .register(HotkeyKey::Long, chord)
                .map_err(register_failed)?;
        }
        let snapshot = match self.settings.apply(patch) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if newly_registered {
                    self.unregister_long_hotkey();
                }
                return Err(error);
            }
        };
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
        if let Some(enabled) = requested {
            let previously_desired = self.long_desired;
            self.long_desired = enabled;
            self.long_disable_pending = !self.long_desired;
            if self.long_desired {
                if newly_registered {
                    self.long_hotkey_registered = true;
                    self.long_notice = None;
                }
                if !previously_desired {
                    self.long_origin = LongOrigin::Settings;
                }
                if self.long_start_latched && self.long.is_none() {
                    self.start_long(self.long_origin);
                }
            }
        }
        Ok(self.settings_snapshot())
    }

    fn start_long(&mut self, origin: LongOrigin) {
        let plan = StartupPlan::for_target(
            EngineKind::Whisper,
            self.models_dir.clone(),
            self.capabilities,
            kikigaki_core::config::punct_effective(
                self.settings.config(),
                self.capabilities,
                EngineKind::Whisper,
            ),
        );
        let factory = Arc::clone(&self.engine_factory);
        let supervisor =
            EngineSupervisor::new(Box::new(move || factory.build(EngineKind::Whisper)))
                .with_waker(Arc::clone(&self.waker))
                .with_deferred_teardown();
        // Reuse the live download: starting another worker would contend for its model lock.
        // Leave queued events for the new instance, including its request origin.
        let (install, adopted) = match self
            .retiring_installs
            .iter()
            .rposition(|install| !install.terminal)
        {
            Some(index) => (self.retiring_installs.swap_remove(index).port, true),
            None => ((self.long_install_factory)(plan), false),
        };
        let mut long = LongEngine {
            supervisor: Some(supervisor),
            install: Some(install),
            state: if self.retiring.is_empty() {
                LongState::Installing { percent: 0 }
            } else {
                LongState::Loading
            },
            origin,
            installed: false,
            install_terminal: false,
        };
        let start_result = if adopted {
            Ok(true)
        } else {
            long.install
                .as_mut()
                .expect("new long installer exists")
                .start()
        };
        self.long = Some(long);
        if self.long_hotkey_registered {
            self.set_long_notice(None);
        }
        self.dirty = true;
        if adopted {
            return;
        }
        match start_result {
            Ok(true) => {}
            Ok(false) => {
                self.fail_long_install("Long engine model installation is already running".into())
            }
            Err(error) => {
                self.fail_long_install(format!("Long engine model installation failed — {error}"))
            }
        }
    }

    fn retire_long(&mut self, mut supervisor: EngineSupervisor) {
        // A closed event channel has already removed current; both forms must shut down here.
        let closed = supervisor.take_closed_engine();
        self.retiring.push(std::thread::spawn(move || {
            if let Some(engine) = closed {
                engine.shutdown();
            }
            drop(supervisor);
        }));
    }

    fn retire_long_instance(&mut self) {
        if let Some(mut long) = self.long.take() {
            if let Some(supervisor) = long.supervisor.take() {
                self.retire_long(supervisor);
            }
            if let Some(install) = long.install.take() {
                self.retiring_installs.push(RetiringInstall {
                    port: install,
                    terminal: long.install_terminal,
                });
            }
        }
    }

    fn fail_long_install(&mut self, reason: String) {
        tracing::warn!(%reason, "long engine install failed");
        let long = self.long.as_mut().expect("long installer exists");
        long.install_terminal = true;
        let origin = long.origin;
        self.set_long_notice(Some(LONG_INSTALL_FAILED_NOTICE.into()));
        if origin == LongOrigin::Settings {
            if let Err(error) = self.settings.apply(SettingsPatch {
                long_key: Some(false),
                ..SettingsPatch::default()
            }) {
                tracing::error!(code = error.code, message = %error.message, "failed to compensate long key installation failure");
                self.long_notice =
                    Some(format!("{LONG_INSTALL_FAILED_NOTICE} / {}", error.message));
            }
            self.long_desired = false;
            self.long_disable_pending = true;
            self.disable_long();
        } else {
            self.fail_long_load(reason, None, Some(LONG_INSTALL_FAILED_NOTICE.into()));
        }
        self.dirty = true;
    }

    fn fail_long_load(
        &mut self,
        reason: String,
        failed_model: Option<&'static str>,
        notice: Option<String>,
    ) {
        tracing::warn!(%reason, failed_model = ?failed_model, "long engine failed");
        let supervisor = if let Some(long) = self.long.as_mut() {
            if failed_model.is_some() {
                if let Some(install) = long.install.as_mut() {
                    install.invalidate_model_load_failure(failed_model);
                }
            }
            long.state = LongState::Failed {
                reason: reason.clone(),
            };
            long.supervisor.take()
        } else {
            None
        };
        if let Some(supervisor) = supervisor {
            self.retire_long(supervisor);
        }
        self.set_long_notice(Some(notice.unwrap_or_else(|| LONG_FAILED_NOTICE.into())));
    }

    fn disable_long(&mut self) {
        if self.long_hotkey_registered {
            self.unregister_long_hotkey();
            self.long_hotkey_registered = false;
        }
        self.retire_long_instance();
        self.long_disable_pending = false;
        self.dirty = true;
    }

    fn retry_long(&mut self) -> Result<(), UiError> {
        let Some(origin) = self
            .long
            .as_ref()
            .and_then(|long| matches!(long.state, LongState::Failed { .. }).then_some(long.origin))
        else {
            return Err(UiError {
                code: "not_failed",
                message: "長文エンジンは停止していません".into(),
            });
        };
        self.retire_long_instance();
        self.start_long(origin);
        Ok(())
    }

    fn reconcile_long(&mut self) {
        let mut pending = Vec::new();
        for handle in self.retiring.drain(..) {
            if handle.is_finished() {
                if handle.join().is_err() {
                    tracing::warn!("long engine retirement panicked");
                }
            } else {
                pending.push(handle);
            }
        }
        self.retiring = pending;
        if self.long_disable_pending && !self.active_engine_is_whisper() {
            self.disable_long();
        }
        self.retiring_installs.retain_mut(|install| {
            while !install.terminal {
                match install.port.try_recv() {
                    Some(StartupEvent::Installed(_) | StartupEvent::Failed(_)) => {
                        install.terminal = true;
                    }
                    Some(StartupEvent::Installing { .. }) => {}
                    None => break,
                }
            }
            // Keep unfinished workers for adoption and shutdown, without blocking this turn.
            !install.terminal || !install.port.join(Duration::ZERO)
        });
        if !self.long_desired || !self.long_start_latched {
            return;
        }
        if self.long.is_none() {
            self.start_long(self.long_origin);
        }
        self.start_installed_long();
    }

    fn start_installed_long(&mut self) {
        let can_start = self.retiring.is_empty()
            && self.long.as_ref().is_some_and(|long| {
                long.installed
                    && matches!(long.state, LongState::Loading)
                    && long
                        .supervisor
                        .as_ref()
                        .is_some_and(|supervisor| supervisor.sink().is_none())
            });
        if can_start {
            let Some(long) = self.long.as_mut() else {
                return;
            };
            let Some(supervisor) = long.supervisor.as_mut() else {
                return;
            };
            let result = supervisor.start();
            if let Err(error) = result {
                self.fail_long_load(format!("Long engine start failed — {error}"), None, None);
            }
        }
    }

    fn handle_long_events(&mut self, outputs: &mut Vec<Output>) {
        if !self.long_desired && !self.long_disable_pending {
            return;
        }
        loop {
            let event = self
                .long
                .as_mut()
                .filter(|long| !long.installed && !matches!(long.state, LongState::Failed { .. }))
                .and_then(|long| long.install.as_mut())
                .and_then(|install| install.try_recv());
            let Some(event) = event else {
                break;
            };
            let Some(long) = self.long.as_mut() else {
                break;
            };
            self.dirty = true;
            match event {
                StartupEvent::Installing {
                    done_bytes,
                    total_bytes,
                    ..
                } => {
                    let percent = percent(done_bytes, total_bytes);
                    long.state = if self.retiring.is_empty() {
                        LongState::Installing { percent }
                    } else {
                        LongState::Loading
                    };
                }
                StartupEvent::Installed(_) => {
                    long.installed = true;
                    long.install_terminal = true;
                    long.state = LongState::Loading;
                    break;
                }
                StartupEvent::Failed(reason) => {
                    self.fail_long_install(reason);
                    return;
                }
            }
        }
        self.start_installed_long();
        loop {
            let message = self
                .long
                .as_mut()
                .and_then(|long| long.supervisor.as_mut())
                .and_then(EngineSupervisor::try_recv);
            let Some(message) = message else {
                break;
            };
            let Some(long) = self.long.as_mut() else {
                break;
            };
            self.dirty = true;
            match message {
                EngineMsg::Ready => {
                    long.state = LongState::Ready;
                    // Registration warnings remain actionable even after the engine is ready.
                    if self.long_hotkey_registered {
                        self.long_notice_under_fallback = None;
                    }
                    if self.long_hotkey_registered
                        && self
                            .long_notice
                            .as_deref()
                            .is_some_and(|notice| !is_fallback_notice(notice))
                    {
                        self.long_notice = None;
                    }
                }
                EngineMsg::Disconnected {
                    reason,
                    failed_model,
                } => {
                    if self.active_engine_is_whisper() {
                        let gen = self.active.unwrap();
                        outputs.extend(self.session.handle(Input::Abort { gen }, self.clock.now()));
                        // Retirement removes the supervisor; deliver cancellation before moving it.
                        self.cancel_generation(gen, EngineKind::Whisper);
                        self.fail_long_load(reason, failed_model, Some(LONG_FAILED_NOTICE.into()));
                    } else {
                        self.fail_long_load(reason, failed_model, None);
                    }
                    // Retire the entire supervisor before another poll can shut it down inline.
                    break;
                }
                message @ EngineMsg::Final { .. } => {
                    self.submit_final(EngineKind::Whisper, message)
                }
            }
        }
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
                        self.standard_disconnected(outputs);
                        continue;
                    }
                    if self.postprocess.is_none() {
                        let settings = self.settings.snapshot();
                        match self.postprocess_factory.build(
                            self.learned.as_core_rules(),
                            settings.builtin_replace_dict,
                            Some(Arc::clone(&self.waker)),
                        ) {
                            Ok(worker) => {
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
                                self.standard_disconnected(outputs);
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
            if self.retired.contains(&processed.gen) || !self.owners.contains_key(&processed.gen) {
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
                        self.standard_disconnect_pending = false;
                        if !self.long_start_latched {
                            self.long_start_latched = true;
                            if self.long_desired {
                                self.start_long(self.long_origin);
                            }
                        }
                    }
                    Input::Engine(Event::Ready { sr: SAMPLE_RATE })
                }
                message @ EngineMsg::Final { .. } => {
                    self.submit_final(self.standard_engine, message);
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
                    if let Some(supervisor) = self.supervisor_for(self.owner_engine(gen)) {
                        send_command(supervisor, EngineCmd::Begin { gen });
                    }
                }
                Output::StartMic => {
                    let sink = mic_gen
                        .and_then(|gen| self.supervisor_for(self.owner_engine(gen)))
                        .and_then(EngineSupervisor::sink);
                    if let Some(sink) = sink {
                        let permission_denied = microphone_permission_denied();
                        let standard_failed = matches!(
                            self.status,
                            Status::StartupFailed(_) | Status::EngineFailed(_)
                        );
                        if permission_denied && !standard_failed {
                            self.status = Status::Warning(MICROPHONE_PERMISSION_DENIED.into());
                        }
                        if let Err(error) = self.mic.start(sink) {
                            tracing::warn!(%error, "microphone unavailable");
                            if !standard_failed {
                                self.status =
                                    Status::Warning(format!("Microphone unavailable — {error}"));
                            }
                        } else if !permission_denied
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
                    if let Some(supervisor) = self.supervisor_for(self.owner_engine(gen)) {
                        send_command(supervisor, EngineCmd::End { gen, pad_ms });
                    }
                }
                Output::Cancel { gen } => {
                    self.cancel_generation(gen, self.owner_engine(gen));
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
                                if !self.generation_is_in_progress(gen) {
                                    self.complete_notice(gen);
                                }
                                let at = Utc::now();
                                crate::transcript::enqueue(
                                    self.settings.config().transcript_log,
                                    self.settings.config().transcript_dir.clone(),
                                    self.owner_engine(gen).as_str(),
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
                                self.status = Status::Warning(PASTE_FAILED_NOTICE.into());
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
                    row.engine = Some(self.owner_engine(row.gen).as_str().into());
                    if !self.generation_is_in_progress(row.gen) {
                        if row.final_to_paste_ms.is_none() && !row.timeout && !row.paste_failed {
                            self.complete_notice(row.gen);
                        }
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
                                self.status =
                                    Status::StartupFailed(format!("Startup failed — {error}"));
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
                                self.status = Status::EngineFailed(format!(
                                    "Engine restart failed — {error}"
                                ));
                            }
                        }
                    }
                }
            }
        }
        // Record and transcript enqueue above must still see owners retired during this drain.
        self.owners.retain(|gen, _| !self.retired.contains(gen));
        self.pending_notices
            .retain(|gen, _| !self.retired.contains(gen));
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
                hotkey: settings.hotkey.clone(),
                punct_enabled: settings.punct_enabled,
                strip_trailing_period: settings.strip_trailing_period,
                long_hotkey: self
                    .long_hotkey_registered
                    .then(|| settings.long_hotkey.clone()),
            },
            settings,
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
        let mut settings = self.settings.snapshot();
        settings.long_state = self.long.as_ref().map(|long| match &long.state {
            LongState::Installing { percent } => {
                crate::status::LongStateView::Installing { percent: *percent }
            }
            LongState::Loading => crate::status::LongStateView::Loading,
            LongState::Ready => crate::status::LongStateView::Ready,
            LongState::Failed { reason } => crate::status::LongStateView::Failed {
                message: reason.clone(),
            },
        });
        settings
    }

    fn effective_status(&self) -> Status {
        if matches!(
            self.status,
            Status::StartupFailed(_) | Status::EngineFailed(_)
        ) {
            return match (&self.status, &self.postprocess_degraded) {
                (Status::EngineFailed(engine), Some(postprocess)) => {
                    Status::EngineFailed(format!("{engine} / {postprocess}"))
                }
                _ => self.status.clone(),
            };
        }
        if self.status.message() == Some(PASTE_FAILED_NOTICE) {
            return self.status.clone();
        }
        // Standard-side warnings (microphone, hotkey) and install progress affect both keys,
        // so a long-engine notice only fills an otherwise clear status.
        if let (Some(notice), Status::Ok) = (&self.long_notice, &self.status) {
            return Status::Warning(notice.clone());
        }
        self.postprocess_degraded.as_ref().map_or_else(
            || self.status.clone(),
            |message| Status::Warning(message.clone()),
        )
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
        let deadline = Instant::now() + Duration::from_millis(4500);
        self.retire_long_instance();
        self.postprocess.take();
        self.supervisor.take();
        for install in &mut self.retiring_installs {
            if !install
                .port
                .join(deadline.saturating_duration_since(Instant::now()))
            {
                tracing::warn!("long install worker did not join before shutdown deadline");
            }
        }
        self.retiring_installs.clear();
        if !self
            .startup
            .join(deadline.saturating_duration_since(Instant::now()))
        {
            tracing::warn!("startup worker did not join before shutdown deadline");
        }
        for handle in self.retiring.drain(..) {
            let (done, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = done.send(handle.join());
            });
            if !matches!(
                rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                Ok(Ok(()))
            ) {
                tracing::warn!("long engine retirement did not join before shutdown deadline");
            }
        }
        self.publish_if_changed();
    }
}

fn hotkey_input(
    edge: HotkeyEdge,
    window_focused: bool,
    capturing_hotkey: bool,
    engine: EngineKind,
) -> Option<Input> {
    match edge {
        HotkeyEdge::Pressed(_) if window_focused || capturing_hotkey => None,
        HotkeyEdge::Pressed(_) => Some(Input::Pressed),
        HotkeyEdge::Released(_) => Some(Input::Released { engine }),
    }
}

fn is_fallback_notice(notice: &str) -> bool {
    matches!(notice, LONG_PREPARING_NOTICE | LONG_STOPPED_NOTICE)
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

fn progress_message(id: &str, done_bytes: u64, total_bytes: u64) -> String {
    let percent = percent(done_bytes, total_bytes);
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
    use crate::settings::SettingsSnapshot;
    use crate::status::{BootstrapError, OnboardingState};

    #[test]
    fn hotkey_input_handles_both_keys_with_shared_suppression() {
        for edge in [
            HotkeyEdge::Pressed(HotkeyKey::Long),
            HotkeyEdge::Released(HotkeyKey::Long),
        ] {
            assert!(hotkey_input(edge, false, false, EngineKind::Local).is_some());
        }
    }

    #[test]
    fn factory_builds_the_requested_kind() {
        let factory = FakeEngineFactory::new(Arc::new(FakeEngineState::default()));
        for kind in [EngineKind::Local, EngineKind::Remote, EngineKind::Whisper] {
            let mut engine = factory.build(kind).unwrap();
            assert!(matches!(engine.events().try_recv(), Ok(EngineMsg::Ready)));
            assert_eq!(factory.engine(kind).builds.load(Ordering::Relaxed), 1);
            assert_eq!(*factory.engine(kind).observed_kinds.lock().unwrap(), [kind]);
        }
    }

    #[test]
    fn fake_harness_isolates_engine_kinds() {
        let harness = ready_harness();
        let factory = FakeEngineFactory::from_states(Arc::clone(&harness.engines));
        harness
            .engine(EngineKind::Whisper)
            .plans
            .lock()
            .unwrap()
            .push_back(FakeEngineBuild::Pending);
        let mut standard = factory.build(EngineKind::Local).unwrap();
        assert!(matches!(standard.events().try_recv(), Ok(EngineMsg::Ready)));
        let mut long = factory.build(EngineKind::Whisper).unwrap();
        harness
            .engine(EngineKind::Whisper)
            .send(final_message(99, "long only"));
        assert!(matches!(
            long.events().try_recv(),
            Ok(EngineMsg::Final { gen: 99, .. })
        ));
        assert!(matches!(
            standard.events().try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        controller_barrier(&harness);
        assert!(harness.pastes.attempts.lock().unwrap().is_empty());
        assert!(harness
            .engine(EngineKind::Local)
            .canceled
            .lock()
            .unwrap()
            .is_empty());
        assert_eq!(
            harness
                .engine(EngineKind::Local)
                .event_senders
                .lock()
                .unwrap()
                .len(),
            2
        );
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
            (EngineKind::Whisper, Arc::new(FakeEngineState::default())),
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
        starts: AtomicUsize,
        stops: AtomicUsize,
        running: AtomicBool,
    }

    struct FakeMic(Arc<MicState>);

    impl MicPort for FakeMic {
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
    }

    impl FakePostprocessFactory {
        fn new(replace_file: PathBuf, gate: Option<BuildGate>) -> Self {
            Self {
                replace_file,
                builtin: Arc::new(kikigaki_core::replace::builtin_rules().unwrap()),
                gate: Mutex::new(gate),
                punctuator: Arc::new(PunctuatorState::default()),
            }
        }
    }

    fn fake_long_install_factory(state: Arc<StartupState>) -> StartupPortFactory {
        Box::new(move |plan| {
            state.plans.lock().unwrap().push(plan.clone());
            Box::new(FakeStartup {
                rescans: VecDeque::new(),
                required: plan.required,
                state: Arc::clone(&state),
                owns_install_lock: false,
            })
        })
    }

    impl PostprocessFactory for FakePostprocessFactory {
        fn build(
            &self,
            learned: Arc<kikigaki_core::replace::Rules>,
            builtin_enabled: bool,
            waker: Option<Waker>,
        ) -> anyhow::Result<PostprocessWorker> {
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
        plans: Mutex<Vec<StartupPlan>>,
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
                    anyhow::bail!("another install in progress for whisper");
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
        long_startup: Arc<StartupState>,
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
        harness_with_boot_long(
            punct,
            builtin_replace_dict,
            engine_kind,
            engine_plans,
            bootstrap,
            hotkey_plan,
            rescans,
            reject_late_paste,
            clock,
            gate,
            poll_interval,
            BootLong::default(),
        )
    }

    #[derive(Default)]
    struct BootLong {
        enabled: bool,
        registration: Option<bool>,
    }

    #[allow(clippy::too_many_arguments)]
    fn harness_with_boot_long(
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
        boot_long: BootLong,
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
                "hotkey = \"Alt+Space\"\nengine = \"{engine_name}\"\nlong_key = {}\nstrip_trailing_period = false\nbuiltin_replace_dict = {builtin_replace_dict}\nreplace_file = {:?}\nlearned_file = {:?}\nmetrics_path = {:?}\nmodels_dir = {:?}\n[punct]\nenabled = {}\n",
                boot_long.enabled, replace_file, learned_file, metrics_path, models_dir, punct,
            ),
        )
        .unwrap();
        let capabilities = Capabilities {
            punct: true,
            remote_engine: true,
            long_key: true,
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
        let long_startup = Arc::new(StartupState::default());
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
                models_dir,
                bootstrap,
                process_started_at: Instant::now(),
                long_hotkey_registered: boot_long.registration,
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
            fake_long_install_factory(Arc::clone(&long_startup)),
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
            long_startup,
        }
    }

    // Direct turns reuse the injected ports above; gates make teardown assertions deterministic.
    struct LifecycleHarness {
        controller: Controller,
        engines: Arc<Vec<(EngineKind, Arc<FakeEngineState>)>>,
        hotkeys: Arc<HotkeyState>,
        startup: Arc<StartupState>,
        long_startup: Arc<StartupState>,
        pastes: Arc<PasteState>,
        _temp: tempfile::TempDir,
    }

    impl Drop for LifecycleHarness {
        fn drop(&mut self) {
            self.controller.begin_shutdown();
        }
    }

    impl LifecycleHarness {
        fn new(enabled: bool, capable: bool) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("config.toml");
            let replace_file = temp.path().join("replace.toml");
            std::fs::write(&replace_file, "").unwrap();
            std::fs::write(&path, format!("long_key = {enabled}\nreplace_file = {:?}\nmetrics_path = {:?}\ntranscript_dir = {:?}\nlearned_file = {:?}\n[punct]\nenabled = false\n", replace_file, temp.path().join("metrics.jsonl"), temp.path().join("transcripts"), temp.path().join("learned.toml"))).unwrap();
            let capabilities = Capabilities {
                punct: true,
                remote_engine: true,
                long_key: capable,
            };
            let settings = SettingsCoordinator::load(path, capabilities).unwrap();
            let engines = engine_states();
            let factory: Arc<dyn EngineFactory> =
                Arc::new(FakeEngineFactory::from_states(Arc::clone(&engines)));
            let hotkeys = Arc::new(HotkeyState::default());
            let startup = Arc::new(StartupState::default());
            let long_startup = Arc::new(StartupState::default());
            let pastes = Arc::new(PasteState::default());
            let (controller, _) = Controller::new(
                ControllerConfig {
                    settings,
                    capabilities,
                    models_dir: temp.path().join("models"),
                    bootstrap: BootstrapOutcome::ReadyToStart,
                    process_started_at: Instant::now(),
                    long_hotkey_registered: None,
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
                fake_long_install_factory(Arc::clone(&long_startup)),
                Box::new(RealClock),
                Arc::new(|| {}),
            );
            Self {
                controller,
                engines,
                hotkeys,
                startup,
                long_startup,
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
            c.handle_long_events(&mut outputs);
            c.handle_postprocess_events(&mut outputs);
            c.execute_outputs(outputs);
            c.apply_standard_disconnect();
            c.reconcile_long();
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

        fn apply(&mut self, enabled: bool) -> Result<SettingsSnapshot, UiError> {
            let (reply, rx) = mpsc::sync_channel(1);
            self.controller
                .handle_command(ControllerCmd::ApplySettings {
                    patch: SettingsPatch {
                        long_key: Some(enabled),
                        ..SettingsPatch::default()
                    },
                    reply,
                });
            rx.try_recv().unwrap()
        }

        fn retry(&mut self) -> Result<(), UiError> {
            let (reply, rx) = mpsc::sync_channel(1);
            self.controller
                .handle_command(ControllerCmd::RetryLongEngine { reply });
            rx.try_recv().unwrap()
        }

        fn ready_long(&mut self) {
            self.boot(false);
            self.apply(true).unwrap();
            self.turn();
            assert!(matches!(
                self.controller.long.as_ref().unwrap().state,
                LongState::Ready
            ));
        }

        fn disconnected(&mut self, model: Option<&'static str>) {
            self.engine(EngineKind::Whisper)
                .send(EngineMsg::Disconnected {
                    reason: "long failed".into(),
                    failed_model: model,
                });
            self.turn();
        }

        fn await_retirement(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !self.controller.retiring.is_empty() {
                assert!(Instant::now() < deadline, "retirement did not finish");
                self.turn();
                thread::yield_now();
            }
        }

        fn shutdown_gate(&self) -> (Receiver<()>, SyncSender<()>) {
            let (entered, entered_rx) = mpsc::sync_channel(1);
            let (release, release_rx) = mpsc::sync_channel(1);
            *self
                .engine(EngineKind::Whisper)
                .shutdown_gate
                .lock()
                .unwrap() = Some(BuildGate {
                entered,
                release: release_rx,
            });
            (entered_rx, release)
        }

        fn dictate(&mut self) {
            let c = &mut self.controller;
            let mut outputs = Vec::new();
            c.handle_hotkey_edge(HotkeyEdge::Pressed(HotkeyKey::Standard), &mut outputs);
            c.handle_hotkey_edge(HotkeyEdge::Released(HotkeyKey::Standard), &mut outputs);
            c.execute_outputs(outputs);
            assert_eq!(c.session.state(), State::Finalizing);
            self.engine(EngineKind::Local)
                .send(final_message(1, "standard works"));
            let deadline = Instant::now() + Duration::from_secs(1);
            while self.pastes.attempts.lock().unwrap().is_empty() {
                assert!(Instant::now() < deadline, "standard dictation stalled");
                self.turn();
                thread::yield_now();
            }
            assert_eq!(self.controller.session.state(), State::Idle);
        }
    }

    mod routing_tests {
        use super::*;

        const PREPARING: &str = "長文エンジン準備中のため標準で認識しました";
        const STOPPED: &str = "長文エンジンが停止中のため標準で認識しました";
        const FAILED: &str = "長文エンジンが停止しました — 設定から再試行できます";

        struct AudioMic;
        impl MicPort for AudioMic {
            fn start(&mut self, sink: AudioSink) -> anyhow::Result<()> {
                sink.send(EngineCmd::Audio(vec![0.25]))?;
                Ok(())
            }
            fn stop(&mut self) {}
        }

        fn ready() -> (LifecycleHarness, Receiver<()>) {
            let mut h = LifecycleHarness::new(false, true);
            let (tx, rx) = mpsc::channel();
            h.controller.waker = Arc::new(move || {
                let _ = tx.send(());
            });
            h.controller.mic = Box::new(AudioMic);
            h.ready_long();
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
                    assert_eq!(h.controller.owners[&1].engine, EngineKind::Local);
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
                assert_eq!(h.controller.status.message(), Some(PASTE_FAILED_NOTICE));
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
            fn fallback_long_press_with_mid_utterance_segments_pastes_all_and_moves_notice_at_end()
            {
                for end in ["end", ""] {
                    let (mut h, rx) = ready();
                    h.controller.long.as_mut().unwrap().state = LongState::Loading;
                    edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Long)]);
                    for text in ["first", "", "second"] {
                        finish(&mut h, &rx, EngineKind::Local, 1, text);
                        assert_eq!(h.controller.session.state(), State::Recording);
                        assert!(
                            h.controller.long_notice.is_none(),
                            "fallback notice must wait for the end"
                        );
                        assert_eq!(h.controller.pending_notices[&1], PREPARING);
                        assert_eq!(h.controller.active, Some(1));
                    }
                    edges(&mut h, [HotkeyEdge::Released(HotkeyKey::Long)]);
                    assert_eq!(h.controller.session.state(), State::Finalizing);
                    assert!(h.controller.long_notice.is_none());
                    finish(&mut h, &rx, EngineKind::Local, 1, end);
                    let mut expected = vec!["first", "second"];
                    if !end.is_empty() {
                        expected.push(end);
                    }
                    assert_eq!(*h.pastes.attempts.lock().unwrap(), expected);
                    assert_eq!(rows(&h).len(), 4);
                    assert!(rows(&h).iter().all(|row| row["engine"] == "local"));
                    assert!(commands(&h, EngineKind::Whisper).is_empty());
                    assert_eq!(h.controller.long_notice.as_deref(), Some(PREPARING));
                    assert!(h.controller.pending_notices.is_empty());
                    assert!(h.controller.active.is_none());
                    assert!(h.controller.owners.is_empty());
                    assert_eq!(h.controller.session.state(), State::Idle);
                }
            }

            #[test]
            fn repress_in_finalizing_after_mid_utterance_segments_cancels_and_begins_new() {
                let (mut h, rx) = ready();
                edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
                for text in ["first", "second"] {
                    finish(&mut h, &rx, EngineKind::Local, 1, text);
                }
                edges(&mut h, [HotkeyEdge::Released(HotkeyKey::Standard)]);
                assert_eq!(h.controller.session.state(), State::Finalizing);
                edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Long)]);
                assert_eq!(*h.engine(EngineKind::Local).canceled.lock().unwrap(), [1]);
                assert_eq!(h.controller.active, Some(2));
                assert_eq!(h.controller.owners[&2].engine, EngineKind::Whisper);
                assert!(!h.controller.owners.contains_key(&1));
                assert_eq!(
                    commands(&h, EngineKind::Local),
                    [
                        EngineCmd::Begin { gen: 1 },
                        EngineCmd::Audio(vec![0.25]),
                        EngineCmd::End {
                            gen: 1,
                            pad_ms: 500
                        },
                        EngineCmd::Cancel { gen: 1 }
                    ]
                );
                assert_eq!(
                    commands(&h, EngineKind::Whisper),
                    [EngineCmd::Begin { gen: 2 }, EngineCmd::Audio(vec![0.25])]
                );
                edges(&mut h, [HotkeyEdge::Released(HotkeyKey::Long)]);
                finish(&mut h, &rx, EngineKind::Whisper, 2, "new utterance");
                assert_eq!(
                    *h.pastes.attempts.lock().unwrap(),
                    ["first", "second", "new utterance"]
                );
                assert_eq!(
                    rows(&h)
                        .iter()
                        .map(|row| row["engine"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["local", "local", "whisper"]
                );
                h.engine(EngineKind::Local)
                    .send(final_message(1, "canceled end"));
                h.turn();
                assert_no_submission(&mut h);
            }
        }

        #[test]
        fn standard_key_routes_begin_audio_end_to_standard() {
            let (mut h, _) = ready();
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
            assert!(commands(&h, EngineKind::Whisper).is_empty());
        }

        #[test]
        fn long_key_routes_to_whisper_when_ready() {
            let (mut h, _) = ready();
            utterance(&mut h, HotkeyKey::Long);
            assert_eq!(
                commands(&h, EngineKind::Whisper),
                [
                    EngineCmd::Begin { gen: 1 },
                    EngineCmd::Audio(vec![0.25]),
                    EngineCmd::End {
                        gen: 1,
                        pad_ms: 500
                    }
                ]
            );
            assert!(commands(&h, EngineKind::Local).is_empty());
        }

        fn fallback(state: LongState, notice: &str, text: &str) {
            let (mut h, rx) = ready();
            h.controller.long.as_mut().unwrap().state = state;
            utterance(&mut h, HotkeyKey::Long);
            assert_ne!(
                h.controller.snapshot().status.message.as_deref(),
                Some(notice)
            );
            finish(&mut h, &rx, EngineKind::Local, 1, text);
            assert_eq!(
                h.controller.snapshot().status.message.as_deref(),
                Some(notice)
            );
            assert!(commands(&h, EngineKind::Whisper).is_empty());
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
            assert_ne!(
                h.controller.snapshot().status.message.as_deref(),
                Some(notice)
            );
        }

        #[test]
        fn long_key_falls_back_while_installing() {
            fallback(LongState::Installing { percent: 20 }, PREPARING, "fallback");
        }
        #[test]
        fn long_key_falls_back_while_loading() {
            fallback(LongState::Loading, PREPARING, "fallback");
        }
        #[test]
        fn long_key_falls_back_while_failed() {
            fallback(
                LongState::Failed {
                    reason: "failed".into(),
                },
                STOPPED,
                "fallback",
            );
        }
        #[test]
        fn fallback_notice_survives_empty_result() {
            fallback(LongState::Loading, PREPARING, "");
        }

        #[test]
        fn rejected_press_leaves_no_route() {
            let mut h = LifecycleHarness::new(true, true);
            h.boot(true);
            edges(
                &mut h,
                [
                    HotkeyEdge::Pressed(HotkeyKey::Long),
                    HotkeyEdge::Released(HotkeyKey::Long),
                ],
            );
            assert_eq!(h.controller.session.state(), State::Disconnected);
            assert!(commands(&h, EngineKind::Local).is_empty());
            h.engine(EngineKind::Local).send(EngineMsg::Ready);
            h.turn();
            h.engine(EngineKind::Local)
                .send(final_message(1, "rejected"));
            h.turn();
            assert_no_submission(&mut h);
        }

        #[test]
        fn other_key_press_during_recording_is_ignored() {
            let (mut h, _) = ready();
            edges(
                &mut h,
                [
                    HotkeyEdge::Pressed(HotkeyKey::Long),
                    HotkeyEdge::Pressed(HotkeyKey::Standard),
                ],
            );
            assert_eq!(h.controller.session.state(), State::Recording);
            assert!(commands(&h, EngineKind::Local).is_empty());
            assert_eq!(commands(&h, EngineKind::Whisper).len(), 2);
        }

        #[test]
        fn only_the_starting_keys_release_ends_the_utterance() {
            for key in [HotkeyKey::Standard, HotkeyKey::Long] {
                let (mut h, _) = ready();
                let other = if key == HotkeyKey::Standard {
                    HotkeyKey::Long
                } else {
                    HotkeyKey::Standard
                };
                edges(
                    &mut h,
                    [HotkeyEdge::Pressed(key), HotkeyEdge::Released(other)],
                );
                assert_eq!(h.controller.session.state(), State::Recording);
                edges(&mut h, [HotkeyEdge::Released(key)]);
                assert_eq!(h.controller.session.state(), State::Finalizing);
            }
        }

        #[test]
        fn repress_in_finalizing_on_other_key_cancels_old_owner_and_begins_new() {
            let (mut h, _) = ready();
            utterance(&mut h, HotkeyKey::Long);
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
            assert_eq!(*h.engine(EngineKind::Whisper).canceled.lock().unwrap(), [1]);
            assert_eq!(
                commands(&h, EngineKind::Local),
                [EngineCmd::Begin { gen: 2 }, EngineCmd::Audio(vec![0.25])]
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
                    HotkeyEdge::Pressed(HotkeyKey::Long),
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
                    EngineCmd::Cancel { gen: 1 }
                ]
            );
            assert_eq!(
                commands(&h, EngineKind::Whisper),
                [EngineCmd::Begin { gen: 2 }, EngineCmd::Audio(vec![0.25])]
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
            utterance(&mut h, HotkeyKey::Long);
            finish(&mut h, &rx, EngineKind::Whisper, 1, "owner");
            assert_eq!(rows(&h)[0]["engine"], "whisper");
            assert_eq!(transcripts(&h)[0]["engine"], "whisper");
            assert!(h.controller.owners.is_empty());
            assert!(h.controller.active.is_none());
            h.engine(EngineKind::Whisper).send(final_message(1, "late"));
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
        fn long_press_while_disable_pending_falls_back() {
            let (mut h, _) = ready();
            utterance(&mut h, HotkeyKey::Long);
            h.apply(false).unwrap();
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Long)]);
            h.turn();
            assert_eq!(
                commands(&h, EngineKind::Local),
                [EngineCmd::Begin { gen: 2 }, EngineCmd::Audio(vec![0.25])]
            );
            assert!(h.controller.long_notice.is_none());
            assert!(h.controller.long.is_none());
        }

        #[test]
        fn final_from_non_owner_supervisor_is_dropped() {
            for key in [HotkeyKey::Standard, HotkeyKey::Long] {
                let (mut h, _) = ready();
                utterance(&mut h, key);
                let other = if key == HotkeyKey::Standard {
                    EngineKind::Whisper
                } else {
                    EngineKind::Local
                };
                h.engine(other).send(final_message(1, "wrong engine"));
                h.turn();
                assert_no_submission(&mut h);
                assert_eq!(h.controller.session.state(), State::Finalizing);
            }
        }

        #[test]
        fn late_final_after_cancel_timeout_disable_retry_is_dropped() {
            let (mut h, _) = ready();
            utterance(&mut h, HotkeyKey::Long);
            edges(
                &mut h,
                [
                    HotkeyEdge::Pressed(HotkeyKey::Standard),
                    HotkeyEdge::Released(HotkeyKey::Standard),
                ],
            );
            h.engine(EngineKind::Whisper)
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
            h.apply(false).unwrap();
            h.turn();
            h.await_retirement();
            h.apply(true).unwrap();
            h.turn();
            h.disconnected(None);
            h.retry().unwrap();
            h.await_retirement();
            h.turn();
            h.engine(EngineKind::Whisper)
                .send(final_message(1, "old instance"));
            h.turn();
            assert_no_submission(&mut h);
            assert!(h.pastes.attempts.lock().unwrap().is_empty());
        }

        #[test]
        fn finals_use_their_engines_postprocess_settings() {
            use crate::settings::PunctSetting;
            for (choice, local, long) in [
                (PunctSetting::Off, "甲、乙", "甲乙"),
                (PunctSetting::On, "甲、乙。", "甲乙。"),
                (PunctSetting::OnStrip, "甲、乙", "甲乙"),
            ] {
                let (mut h, rx) = ready();
                h.controller
                    .apply_settings(SettingsPatch {
                        punctuation: Some(choice),
                        ..SettingsPatch::default()
                    })
                    .unwrap();
                // Keep both generations' accepted messages in the FIFO before draining results.
                let mut outputs = Vec::new();
                for (gen, key, kind, raw) in [
                    (1, HotkeyKey::Standard, EngineKind::Local, "甲、乙"),
                    (2, HotkeyKey::Long, EngineKind::Whisper, "甲、乙。"),
                    (3, HotkeyKey::Standard, EngineKind::Local, "甲、乙"),
                ] {
                    h.controller
                        .handle_hotkey_edge(HotkeyEdge::Pressed(key), &mut outputs);
                    h.engine(kind).send(final_message(gen, raw));
                    h.controller.handle_engine_messages(&mut outputs);
                    h.controller.handle_long_events(&mut outputs);
                    // Session accepts the processed final and becomes Idle, but outputs stay queued.
                    rx.recv_timeout(Duration::from_secs(1)).unwrap();
                    h.controller
                        .handle_hotkey_edge(HotkeyEdge::Released(key), &mut outputs);
                    h.controller.handle_postprocess_events(&mut outputs);
                }
                h.controller.execute_outputs(outputs);
                assert_eq!(*h.pastes.attempts.lock().unwrap(), [local, long, local]);
                assert_eq!(
                    rows(&h)
                        .iter()
                        .map(|row| row["engine"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["local", "whisper", "local"]
                );
            }
        }

        #[test]
        fn long_disconnect_in_recording_aborts_and_standard_still_works() {
            for closed in [false, true] {
                let (mut h, rx) = ready();
                edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Long)]);
                if closed {
                    h.engine(EngineKind::Whisper)
                        .event_senders
                        .lock()
                        .unwrap()
                        .clear();
                    h.turn();
                } else {
                    h.disconnected(None);
                }
                assert_eq!(h.controller.session.state(), State::Idle);
                assert_eq!(h.controller.long_notice.as_deref(), Some(FAILED));
                assert!(rows(&h).is_empty());
                utterance(&mut h, HotkeyKey::Standard);
                finish(&mut h, &rx, EngineKind::Local, 2, "works");
                assert_eq!(*h.pastes.attempts.lock().unwrap(), ["works"]);
                assert_eq!(h.controller.long_notice.as_deref(), Some(FAILED));
            }
        }

        #[test]
        fn long_disconnect_in_finalizing_aborts() {
            let (mut h, _) = ready();
            utterance(&mut h, HotkeyKey::Long);
            h.disconnected(None);
            assert_eq!(h.controller.session.state(), State::Idle);
            assert_eq!(*h.engine(EngineKind::Whisper).canceled.lock().unwrap(), [1]);
            assert!(rows(&h).is_empty());
        }

        #[test]
        fn standard_disconnect_during_long_utterance_defers_session_disconnect() {
            let (mut h, rx) = ready();
            utterance(&mut h, HotkeyKey::Long);
            h.engine(EngineKind::Local).send(EngineMsg::Disconnected {
                reason: "standard failed".into(),
                failed_model: None,
            });
            h.turn();
            assert_eq!(h.controller.session.state(), State::Finalizing);
            assert_eq!(
                h.controller.status,
                Status::EngineFailed("standard failed".into())
            );
            finish(&mut h, &rx, EngineKind::Whisper, 1, "long completes");
            assert_eq!(*h.pastes.attempts.lock().unwrap(), ["long completes"]);
            assert_eq!(h.controller.session.state(), State::Disconnected);
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Long)]);
            h.turn();
            assert_eq!(h.controller.session.state(), State::Idle);
        }

        #[test]
        fn disable_during_long_utterance_defers_until_retired() {
            let (mut h, rx) = ready();
            utterance(&mut h, HotkeyKey::Long);
            h.apply(false).unwrap();
            h.turn();
            assert!(!h.controller.settings.config().long_key);
            assert!(h.controller.long.is_some());
            assert!(h.hotkeys.unregistered.lock().unwrap().is_empty());
            finish(
                &mut h,
                &rx,
                EngineKind::Whisper,
                1,
                "complete before disable",
            );
            assert!(h.controller.long.is_none());
            assert_eq!(*h.hotkeys.unregistered.lock().unwrap(), [HotkeyKey::Long]);
            assert_eq!(
                *h.pastes.attempts.lock().unwrap(),
                ["complete before disable"]
            );
        }

        #[test]
        fn released_carries_whisper_engine_for_long_utterances() {
            let (mut h, _) = ready();
            let press = Instant::now();
            h.controller.clock = Box::new(FakeClock(Some(press)));
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Long)]);
            h.controller.clock = Box::new(FakeClock(Some(press + Duration::from_secs(24))));
            edges(&mut h, [HotkeyEdge::Released(HotkeyKey::Long)]);
            let outputs = h
                .controller
                .session
                .handle(Input::Tick, press + Duration::from_millis(27_500));
            assert!(
                outputs.is_empty(),
                "Whisper timeout must include half the audio duration"
            );
            assert_eq!(h.controller.session.state(), State::Finalizing);
        }

        #[test]
        fn extra_standard_failure_precedes_mic_failure_on_batched_long_start() {
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
                .handle_hotkey_edge(HotkeyEdge::Pressed(HotkeyKey::Long), &mut outputs);
            h.engine(EngineKind::Local).send(EngineMsg::Disconnected {
                reason: "standard failed".into(),
                failed_model: None,
            });
            h.controller.handle_engine_messages(&mut outputs);
            h.controller.execute_outputs(outputs);
            assert_eq!(
                h.controller.status,
                Status::EngineFailed("standard failed".into())
            );
            assert!(h.controller.standard_disconnect_pending);
            assert_eq!(h.controller.session.state(), State::Recording);
        }

        #[test]
        fn extra_multiple_processed_finals_in_one_turn_keep_recording_owner() {
            let (mut h, rx) = ready();
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Long)]);
            for text in ["first", "second"] {
                h.engine(EngineKind::Whisper).send(final_message(1, text));
            }
            let mut outputs = Vec::new();
            h.controller.handle_long_events(&mut outputs);
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
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Long)]);
            finish(&mut h, &rx, EngineKind::Whisper, 1, "early");
            assert_eq!(h.controller.active, Some(1));
            finish(&mut h, &rx, EngineKind::Whisper, 1, "another segment");
            edges(&mut h, [HotkeyEdge::Released(HotkeyKey::Long)]);
            assert_eq!(h.controller.session.state(), State::Finalizing);
            finish(&mut h, &rx, EngineKind::Whisper, 1, "");
            assert_eq!(
                *h.pastes.attempts.lock().unwrap(),
                ["early", "another segment"]
            );
            assert_eq!(h.controller.session.state(), State::Idle);
            assert!(h.controller.active.is_none());
            assert!(h.controller.owners.is_empty());
            assert!(commands(&h, EngineKind::Whisper)
                .iter()
                .any(|cmd| matches!(cmd, EngineCmd::End { gen: 1, .. })));
        }

        #[test]
        fn extra_processed_final_after_abort_is_dropped() {
            let (mut h, rx) = ready();
            utterance(&mut h, HotkeyKey::Long);
            h.engine(EngineKind::Whisper)
                .send(final_message(1, "queued before abort"));
            h.turn();
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
            h.disconnected(None);
            h.turn();
            assert!(h.pastes.attempts.lock().unwrap().is_empty());
            assert!(rows(&h).is_empty());
        }

        #[test]
        fn extra_fallback_notice_temporarily_covers_failure_then_restores_it() {
            let (mut h, rx) = ready();
            h.disconnected(None);
            utterance(&mut h, HotkeyKey::Long);
            finish(&mut h, &rx, EngineKind::Local, 1, "fallback after failure");
            assert_eq!(h.controller.long_notice.as_deref(), Some(STOPPED));
            edges(&mut h, [HotkeyEdge::Pressed(HotkeyKey::Standard)]);
            assert_eq!(
                h.controller.long_notice.as_deref(),
                Some(LONG_FAILED_NOTICE)
            );
        }

        #[test]
        fn extra_long_failure_during_fallback_does_not_abort_standard() {
            let (mut h, rx) = ready();
            h.controller.long.as_mut().unwrap().state = LongState::Loading;
            utterance(&mut h, HotkeyKey::Long);
            h.disconnected(None);
            assert_eq!(h.controller.session.state(), State::Finalizing);
            finish(&mut h, &rx, EngineKind::Local, 1, "fallback survives");
            assert_eq!(*h.pastes.attempts.lock().unwrap(), ["fallback survives"]);
        }

        #[test]
        fn extra_both_keys_obey_focus_and_capture_suppression() {
            for key in [HotkeyKey::Standard, HotkeyKey::Long] {
                for focused in [false, true] {
                    let (mut h, _) = ready();
                    h.controller.window_focused = focused;
                    h.controller.capturing_hotkey = !focused;
                    edges(&mut h, [HotkeyEdge::Pressed(key)]);
                    assert!(h.controller.owners.is_empty());
                    assert_eq!(h.controller.session.state(), State::Idle);
                }
            }
        }

        #[test]
        fn extra_paste_failure_warning_precedes_fallback_and_engine_notices() {
            let (mut h, rx) = ready();
            h.controller.long.as_mut().unwrap().state = LongState::Loading;
            h.controller.paster = Box::new(FakePaster(Arc::new(PasteState {
                reject_late: true,
                ..PasteState::default()
            })));
            h.controller.clock =
                Box::new(FakeClock(Some(Instant::now() - Duration::from_secs(10))));
            utterance(&mut h, HotkeyKey::Long);
            finish(&mut h, &rx, EngineKind::Local, 1, "failed paste");
            h.controller.long_notice = Some("long failed".into());
            assert_eq!(
                h.controller.snapshot().status.message.as_deref(),
                Some(PASTE_FAILED_NOTICE)
            );
            assert!(h.controller.owners.is_empty());
            assert_eq!(rows(&h)[0]["paste_failed"], true);
        }

        #[test]
        fn standard_warnings_and_install_progress_outrank_long_notices() {
            let (mut h, _) = ready();
            h.disconnected(None);
            assert_eq!(
                h.controller.snapshot().status.message.as_deref(),
                Some(LONG_FAILED_NOTICE)
            );
            for status in [
                Status::Warning(MICROPHONE_PERMISSION_DENIED.into()),
                Status::Warning("ホットキーが無効 — 再設定してください".into()),
                Status::Installing("downloading".into()),
            ] {
                h.controller.status = status.clone();
                assert_eq!(
                    h.controller.snapshot().status.message.as_deref(),
                    status.message(),
                    "{status:?}"
                );
            }
            h.controller.status = Status::Ok;
            assert_eq!(
                h.controller.snapshot().status.message.as_deref(),
                Some(LONG_FAILED_NOTICE)
            );
        }

        #[test]
        fn extra_postprocess_fifo_alternates_engine_settings_back_to_back() {
            use crate::settings::PunctSetting;
            for (choice, local, long) in [
                (PunctSetting::Off, "甲、乙", "甲乙"),
                (PunctSetting::On, "甲、乙。", "甲乙。"),
                (PunctSetting::OnStrip, "甲、乙", "甲乙"),
            ] {
                let (mut h, _) = ready();
                h.controller
                    .apply_settings(SettingsPatch {
                        punctuation: Some(choice),
                        ..SettingsPatch::default()
                    })
                    .unwrap();
                for (gen, engine, text) in [
                    (10, EngineKind::Local, "甲、乙"),
                    (11, EngineKind::Whisper, "甲、乙。"),
                    (12, EngineKind::Local, "甲、乙"),
                ] {
                    h.controller.owners.insert(
                        gen,
                        Owner {
                            key: HotkeyKey::Standard,
                            engine,
                        },
                    );
                    h.controller.submit_final(engine, final_message(gen, text));
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
                assert_eq!(texts, [local, long, local]);
            }
        }

        #[test]
        fn transcript_and_metrics_record_owner_engine() {
            for (state, key, kind, expected) in [
                (
                    LongState::Ready,
                    HotkeyKey::Long,
                    EngineKind::Whisper,
                    "whisper",
                ),
                (
                    LongState::Loading,
                    HotkeyKey::Long,
                    EngineKind::Local,
                    "local",
                ),
                (
                    LongState::Ready,
                    HotkeyKey::Standard,
                    EngineKind::Local,
                    "local",
                ),
            ] {
                let (mut h, rx) = ready();
                h.controller.long.as_mut().unwrap().state = state;
                h.controller
                    .apply_settings(SettingsPatch {
                        transcript_log: Some(true),
                        ..SettingsPatch::default()
                    })
                    .unwrap();
                utterance(&mut h, key);
                finish(&mut h, &rx, kind, 1, "metadata");
                assert_eq!(rows(&h)[0]["engine"], expected);
                assert_eq!(transcripts(&h)[0]["engine"], expected);
            }
        }
    }

    #[test]
    fn long_engine_starts_only_after_standard_first_ready() {
        let mut h = LifecycleHarness::new(true, true);
        h.boot(true);
        assert!(h.controller.long.is_none());
        assert_eq!(h.long_startup.starts.load(Ordering::Relaxed), 0);
        h.controller.hotwords_desired = true;
        h.engine(EngineKind::Local).send(EngineMsg::Ready);
        h.turn();
        assert!(
            h.controller.long.is_none(),
            "stale Ready must not latch long startup"
        );
        h.turn();
        assert!(h.controller.long.is_some());
        assert_eq!(h.long_startup.starts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn standard_restart_ready_does_not_recreate_long() {
        let mut h = LifecycleHarness::new(true, true);
        h.boot(false);
        h.controller.first_ready = true; // Metrics and long-start latches are independent.
        h.controller.supervisor.as_mut().unwrap().restart().unwrap();
        h.turn();
        assert_eq!(h.long_startup.starts.load(Ordering::Relaxed), 1);
        assert_eq!(
            h.engine(EngineKind::Whisper).builds.load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn long_engine_reaches_ready_through_install_and_load() {
        let mut h = LifecycleHarness::new(true, true);
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.engine(EngineKind::Whisper)
            .plans
            .lock()
            .unwrap()
            .push_back(FakeEngineBuild::Pending);
        h.boot(false);
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Installing {
                id: "whisper",
                done_bytes: 45,
                total_bytes: 100,
            });
        h.turn();
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Installing { percent: 45 }
        ));
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Installed(
                kikigaki_core::startup::InstallReport {
                    installed: Vec::new(),
                    reused: Vec::new(),
                },
            ));
        h.turn();
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Loading
        ));
        h.engine(EngineKind::Whisper).send(EngineMsg::Ready);
        h.turn();
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Ready
        ));
        let plans = h.long_startup.plans.lock().unwrap();
        assert_eq!(plans[0].engine, EngineKind::Whisper);
        assert!(!plans[0]
            .required
            .iter()
            .any(|model| model.id == "mojicast-punct"));
    }

    #[test]
    fn long_startup_requires_shared_punctuation_model_when_enabled() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.controller
            .apply_settings(SettingsPatch {
                punctuation: Some(crate::settings::PunctSetting::On),
                long_key: Some(true),
                ..SettingsPatch::default()
            })
            .unwrap();
        let plans = h.long_startup.plans.lock().unwrap();
        assert!(plans[0]
            .required
            .iter()
            .any(|model| model.id == "mojicast-punct"));
    }

    #[test]
    fn long_events_never_change_session_or_standard_status() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.controller.status = Status::Warning("paste warning".into());
        h.controller.startup_cold = true;
        h.apply(true).unwrap();
        h.turn();
        h.disconnected(None);
        assert_eq!(h.controller.session.state(), State::Idle);
        assert_eq!(h.controller.status, Status::Warning("paste warning".into()));
        assert!(h.controller.startup_cold);
        assert!(!h.controller.download_in_flight);
        assert!(h.controller.onboarding.is_none());
        assert_eq!(h.controller.snapshot().status.phase, "ready");
        h.engine(EngineKind::Local).send(EngineMsg::Ready);
        h.turn();
        assert_eq!(
            h.controller.snapshot().status.message.as_deref(),
            Some(LONG_FAILED_NOTICE)
        );
        h.controller.status = Status::StartupFailed("standard failed".into());
        assert_eq!(
            h.controller.snapshot().status.message.as_deref(),
            Some("standard failed")
        );
    }

    #[test]
    fn long_install_progress_does_not_clear_a_paste_warning() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.controller.status = Status::Warning("paste warning".into());
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.apply(true).unwrap();
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Installing {
                id: "whisper",
                done_bytes: 45,
                total_bytes: 100,
            });
        h.turn();
        assert_eq!(
            h.controller.snapshot().status.message.as_deref(),
            Some("paste warning")
        );
    }

    #[test]
    fn enable_registers_then_persists_then_starts() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(true);
        let result = h.apply(true).unwrap();
        assert!(result.long_key);
        assert_eq!(
            *h.hotkeys.registered.lock().unwrap(),
            [(HotkeyKey::Long, "Alt+Shift+Space".into())]
        );
        assert!(std::fs::read_to_string(h._temp.path().join("config.toml"))
            .unwrap()
            .contains("long_key = true"));
        assert_eq!(h.long_startup.starts.load(Ordering::Relaxed), 0);
        h.engine(EngineKind::Local).send(EngineMsg::Ready);
        h.turn();
        assert_eq!(h.long_startup.starts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn enable_registration_failure_does_not_persist() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.hotkeys
            .plan
            .lock()
            .unwrap()
            .push_back(Err("registration failed".into()));
        assert_eq!(h.apply(true).unwrap_err().code, "register_failed");
        assert!(!h.controller.settings.config().long_key);
        assert!(h.controller.long.is_none());
    }

    #[test]
    fn enable_persist_failure_unregisters() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        std::fs::write(h._temp.path().join("config.toml"), "# external change\n").unwrap();
        assert_eq!(h.apply(true).unwrap_err().code, "external_change");
        assert_eq!(*h.hotkeys.unregistered.lock().unwrap(), [HotkeyKey::Long]);
        assert!(!h.controller.long_desired);
        assert!(h.controller.long.is_none());
    }

    #[test]
    fn long_failure_card_is_friendly_and_serializes_effective_severity() {
        let mut h = LifecycleHarness::new(true, true);
        h.boot(false);
        h.controller
            .fail_long_load("raw model IO failure".into(), Some("whisper"), None);
        assert!(matches!(&h.controller.long.as_ref().unwrap().state,
            LongState::Failed { reason } if reason == "raw model IO failure"));
        let snapshot = h.controller.snapshot();
        assert_eq!(snapshot.status.message.as_deref(), Some(LONG_FAILED_NOTICE));
        assert_eq!(
            serde_json::to_value(&snapshot.status).unwrap()["level"],
            "warning"
        );
        for (status, level) in [
            (Status::Ok, "ok"),
            (Status::Installing("progress".into()), "ok"),
            (Status::Warning("warning".into()), "warning"),
            (Status::StartupFailed("startup".into()), "error"),
            (Status::EngineFailed("engine".into()), "error"),
        ] {
            h.controller.long_notice = None;
            h.controller.status = status;
            assert_eq!(
                serde_json::to_value(h.controller.snapshot().status).unwrap()["level"],
                level
            );
        }
    }

    #[test]
    fn enable_install_failure_reverts_long_key_and_unregisters() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.apply(true).unwrap();
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Failed("download failed".into()));
        h.turn();
        assert!(!h.controller.settings.config().long_key);
        assert_eq!(*h.hotkeys.unregistered.lock().unwrap(), [HotkeyKey::Long]);
        assert!(h.controller.long.is_none());
        assert_eq!(
            h.controller.long_notice.as_deref(),
            Some("長文キーのモデルを準備できませんでした — もう一度お試しください")
        );
        assert_eq!(h.controller.session.state(), State::Idle);
    }

    #[test]
    fn load_failure_keeps_long_key_and_reports_failed() {
        let mut h = LifecycleHarness::new(false, true);
        h.ready_long();
        h.disconnected(None);
        assert!(h.controller.settings.config().long_key);
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Failed { .. }
        ));
        assert!(h.hotkeys.unregistered.lock().unwrap().is_empty());
        assert_eq!(h.controller.session.state(), State::Idle);
    }

    #[test]
    fn start_error_goes_to_failed() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.engine(EngineKind::Whisper)
            .plans
            .lock()
            .unwrap()
            .push_back(FakeEngineBuild::Fail("start failed".into()));
        h.apply(true).unwrap();
        h.turn();
        assert!(h.controller.settings.config().long_key);
        assert!(
            matches!(&h.controller.long.as_ref().unwrap().state, LongState::Failed { reason } if reason.contains("start failed"))
        );
    }

    #[test]
    fn long_channel_closed_without_message_retires_off_thread() {
        let mut h = LifecycleHarness::new(false, true);
        h.ready_long();
        let (entered, release) = h.shutdown_gate();
        h.engine(EngineKind::Whisper)
            .event_senders
            .lock()
            .unwrap()
            .clear();
        h.turn();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Failed { .. }
        ));
        assert_eq!(h.controller.retiring.len(), 1);
        h.dictate();
        release.send(()).unwrap();
        h.await_retirement();
    }

    #[test]
    fn retry_from_failed_invalidates_failed_model_and_recreates() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        assert_eq!(h.retry().unwrap_err().code, "not_failed");
        h.apply(true).unwrap();
        h.turn();
        h.disconnected(Some(kikigaki_core::models::WHISPER_MODEL_ID));
        assert_eq!(
            *h.long_startup.invalidated.lock().unwrap(),
            [kikigaki_core::models::WHISPER_MODEL_ID]
        );
        assert!(h.startup.invalidated.lock().unwrap().is_empty());
        h.retry().unwrap();
        h.await_retirement();
        h.turn();
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Ready
        ));
        assert_eq!(
            h.engine(EngineKind::Whisper).builds.load(Ordering::Relaxed),
            2
        );
        assert_eq!(h.long_startup.starts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn disable_retires_off_thread_while_standard_keeps_dictating() {
        let mut h = LifecycleHarness::new(false, true);
        h.ready_long();
        *h.engine(EngineKind::Whisper).shutdown_delay.lock().unwrap() =
            Some(Duration::from_millis(1500));
        let (entered, release) = h.shutdown_gate();
        h.apply(false).unwrap();
        h.turn();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        h.dictate();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(h.controller.long.is_none());
        assert_eq!(*h.hotkeys.unregistered.lock().unwrap(), [HotkeyKey::Long]);
        release.send(()).unwrap();
        h.await_retirement();
    }

    #[test]
    fn enable_disable_enable_during_install_converges_to_enabled_once() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.apply(true).unwrap();
        h.apply(false).unwrap();
        h.turn();
        h.apply(true).unwrap();
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Installed(
                kikigaki_core::startup::InstallReport {
                    installed: Vec::new(),
                    reused: Vec::new(),
                },
            ));
        h.turn();
        h.await_retirement();
        h.turn();
        assert!(h.controller.settings.config().long_key);
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Ready
        ));
        assert_eq!(
            h.engine(EngineKind::Whisper).builds.load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            h.engine(EngineKind::Local).builds.load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn reenable_during_inflight_install_adopts_the_running_installer() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.long_startup
            .enforce_install_lock
            .store(true, Ordering::Relaxed);
        h.apply(true).unwrap();
        h.apply(false).unwrap();
        h.turn();
        h.apply(true).unwrap();
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Installed(
                kikigaki_core::startup::InstallReport {
                    installed: Vec::new(),
                    reused: Vec::new(),
                },
            ));
        h.turn();
        h.await_retirement();
        h.turn();
        assert!(h.controller.settings.config().long_key);
        assert_eq!(h.long_startup.plans.lock().unwrap().len(), 1);
        assert_eq!(h.long_startup.starts.load(Ordering::Relaxed), 1);
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Ready
        ));
        assert_eq!(
            h.engine(EngineKind::Whisper).builds.load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn reenable_after_install_finished_creates_a_fresh_installer() {
        let mut h = LifecycleHarness::new(false, true);
        h.long_startup
            .enforce_install_lock
            .store(true, Ordering::Relaxed);
        h.ready_long();
        h.apply(false).unwrap();
        h.turn();
        h.apply(true).unwrap();
        h.turn();
        h.await_retirement();
        h.turn();
        assert_eq!(h.long_startup.plans.lock().unwrap().len(), 2);
        assert_eq!(h.long_startup.starts.load(Ordering::Relaxed), 2);
        assert!(h.controller.settings.config().long_key);
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Ready
        ));
    }

    #[test]
    fn adopted_install_failure_uses_the_new_settings_origin() {
        let mut h = LifecycleHarness::new(true, true);
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.boot(false);
        assert_eq!(h.controller.long.as_ref().unwrap().origin, LongOrigin::Boot);
        h.apply(false).unwrap();
        h.turn();
        // A queued terminal event belongs to the new request when adopted.
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Failed("download failed".into()));
        h.apply(true).unwrap();
        h.turn();
        assert_eq!(h.long_startup.plans.lock().unwrap().len(), 1);
        assert!(!h.controller.settings.config().long_key);
        assert!(h.controller.long.is_none());
        assert_eq!(
            h.controller.long_notice.as_deref(),
            Some("長文キーのモデルを準備できませんでした — もう一度お試しください")
        );
    }

    #[test]
    fn retired_install_is_pruned_after_it_finishes_while_disabled() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.apply(true).unwrap();
        h.apply(false).unwrap();
        h.turn();
        assert_eq!(h.controller.retiring_installs.len(), 1);
        h.long_startup.events.lock().unwrap().extend([
            StartupEvent::Installing {
                id: "whisper",
                done_bytes: 100,
                total_bytes: 100,
            },
            StartupEvent::Installed(kikigaki_core::startup::InstallReport {
                installed: Vec::new(),
                reused: Vec::new(),
            }),
        ]);
        h.turn();
        assert!(h.controller.retiring_installs.is_empty());
        assert!(!h.controller.settings.config().long_key);
        h.long_startup.hold_starts.store(false, Ordering::Relaxed);
        h.apply(true).unwrap();
        h.turn();
        h.await_retirement();
        h.turn();
        assert_eq!(h.long_startup.plans.lock().unwrap().len(), 2);
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Ready
        ));
    }

    #[test]
    fn retired_failed_install_is_pruned_and_never_adopted() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.apply(true).unwrap();
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Failed("download failed".into()));
        // Retire the failed port without running reconciliation first.
        h.controller.handle_long_events(&mut Vec::new());
        h.long_startup.hold_starts.store(false, Ordering::Relaxed);
        h.apply(true).unwrap();
        h.turn();
        h.await_retirement();
        h.turn();
        assert_eq!(h.long_startup.plans.lock().unwrap().len(), 2);
        assert!(h.controller.retiring_installs.is_empty());
        assert!(h.controller.settings.config().long_key);
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Ready
        ));
    }

    #[test]
    fn reenable_waits_for_retirement_before_building() {
        let mut h = LifecycleHarness::new(false, true);
        h.ready_long();
        let (entered, release) = h.shutdown_gate();
        h.apply(false).unwrap();
        h.turn();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        h.apply(true).unwrap();
        h.turn();
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Loading
        ));
        assert_eq!(
            h.engine(EngineKind::Whisper).builds.load(Ordering::Relaxed),
            1
        );
        release.send(()).unwrap();
        h.await_retirement();
        h.turn();
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Ready
        ));
        assert_eq!(
            h.engine(EngineKind::Whisper).builds.load(Ordering::Relaxed),
            2
        );
    }

    #[test]
    fn reenable_progress_waits_in_loading_while_retiring() {
        let mut h = LifecycleHarness::new(false, true);
        h.ready_long();
        let (entered, release) = h.shutdown_gate();
        h.apply(false).unwrap();
        h.turn();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.apply(true).unwrap();
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Installing {
                id: "whisper",
                done_bytes: 45,
                total_bytes: 100,
            });
        h.turn();
        let waiting = matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Loading
        );
        release.send(()).unwrap();
        h.await_retirement();
        assert!(
            waiting,
            "retirement must keep the new long instance in Loading"
        );
    }

    #[test]
    fn boot_long_registration_failure_notice_survives_standard_ready() {
        let harness = harness_with_boot_long(
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
            BootLong {
                enabled: true,
                registration: Some(false),
            },
        );
        wait_snapshot(&harness, |snapshot| {
            matches!(
                snapshot.settings.long_state,
                Some(crate::status::LongStateView::Ready)
            )
        });
        harness.engine(EngineKind::Local).send(EngineMsg::Ready);
        harness.client.wake();
        controller_barrier(&harness);
        let snapshot = latest(&harness).unwrap();
        assert_eq!(
            snapshot.status.message.as_deref(),
            Some(LONG_HOTKEY_INVALID_NOTICE)
        );
        assert!(snapshot.status.long_hotkey.is_none());
        assert_eq!(harness.long_startup.plans.lock().unwrap().len(), 1);
    }

    #[test]
    fn hotword_reconcile_restarts_only_standard() {
        let mut h = LifecycleHarness::new(false, true);
        h.ready_long();
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
            h.engine(EngineKind::Whisper).builds.load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            *h.engine(EngineKind::Local).observed_enabled.lock().unwrap(),
            [false, true]
        );
    }

    #[test]
    fn long_key_without_capability_never_creates_long_engine() {
        let mut h = LifecycleHarness::new(true, false);
        h.boot(false);
        h.turn();
        assert!(h.controller.long.is_none());
        assert!(h.hotkeys.registered.lock().unwrap().is_empty());
        assert!(!h.controller.settings_snapshot().long_available);
        assert_eq!(
            h.engine(EngineKind::Whisper).builds.load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn settings_long_key_rejected_without_capability() {
        let mut h = LifecycleHarness::new(false, false);
        h.boot(false);
        assert_eq!(h.apply(true).unwrap_err().code, "unsupported_engine");
        assert!(!h.controller.settings.config().long_key);
        assert!(h.hotkeys.registered.lock().unwrap().is_empty());
    }

    #[test]
    fn shutdown_with_both_engines_delaying_meets_deadline() {
        let mut h = LifecycleHarness::new(false, true);
        h.ready_long();
        for kind in [EngineKind::Local, EngineKind::Whisper] {
            *h.engine(kind).shutdown_delay.lock().unwrap() = Some(Duration::from_secs(2));
        }
        let started = Instant::now();
        h.controller.begin_shutdown();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(h.controller.retiring.is_empty());
        for startup in [&h.startup, &h.long_startup] {
            let budgets = startup.join_budgets.lock().unwrap();
            assert_eq!(budgets.len(), 1);
            assert!(budgets[0] <= Duration::from_millis(2600));
        }
        for kind in [EngineKind::Local, EngineKind::Whisper] {
            assert_eq!(h.engine(kind).shutdown_threads.lock().unwrap().len(), 1);
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

    fn controller_barrier(harness: &Harness) {
        for _ in 0..2 {
            harness
                .client
                .send_and_wait(|reply| ControllerCmd::ApplySettings {
                    patch: SettingsPatch::default(),
                    reply,
                })
                .unwrap();
        }
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

    fn wait_for_engine_instances(harness: &Harness, expected: usize) {
        assert!(
            poll(|| {
                harness
                    .engines
                    .iter()
                    .map(|(_, state)| state.event_senders.lock().unwrap().len())
                    .sum::<usize>()
                    == expected
                    && harness
                        .engines
                        .iter()
                        .map(|(_, state)| state.command_receivers.lock().unwrap().len())
                        .sum::<usize>()
                        == expected
            }),
            "engine instance count did not reach {expected}"
        );
    }

    #[test]
    fn long_install_completion_does_not_clear_a_non_installing_status() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.controller.status = Status::Warning("paste warning".into());
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.engine(EngineKind::Whisper)
            .plans
            .lock()
            .unwrap()
            .push_back(FakeEngineBuild::Pending);
        h.apply(true).unwrap();
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Installed(
                kikigaki_core::startup::InstallReport {
                    installed: Vec::new(),
                    reused: Vec::new(),
                },
            ));
        h.turn();
        assert_eq!(
            h.controller.snapshot().status.message.as_deref(),
            Some("paste warning")
        );
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Loading
        ));
    }

    #[test]
    fn long_enable_is_rejected_after_boot_startup_failure_without_persisting() {
        let mut h = LifecycleHarness::new(false, true);
        h.startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Failed("boot failed".into()));
        h.turn();
        let path = h._temp.path().join("config.toml");
        let before = std::fs::read_to_string(&path).unwrap();
        assert_eq!(h.apply(true).unwrap_err().code, "engine_not_started");
        assert_eq!(std::fs::read_to_string(path).unwrap(), before);
        assert_eq!(
            h.controller.status,
            Status::StartupFailed("boot failed".into())
        );
        assert!(h.long_startup.plans.lock().unwrap().is_empty());
    }

    #[test]
    fn long_enable_preserves_standard_punctuation_and_dictionary_during_recording() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        let (reply, rx) = mpsc::sync_channel(1);
        h.controller.handle_command(ControllerCmd::ApplySettings {
            patch: SettingsPatch {
                punctuation: Some(crate::settings::PunctSetting::On),
                builtin_replace_dict: Some(true),
                ..SettingsPatch::default()
            },
            reply,
        });
        rx.try_recv().unwrap().unwrap();
        h.turn();
        let mut outputs = Vec::new();
        h.controller
            .handle_hotkey_edge(HotkeyEdge::Pressed(HotkeyKey::Standard), &mut outputs);
        h.controller.execute_outputs(outputs);
        h.apply(true).unwrap();
        h.turn();
        assert_eq!(h.controller.session.state(), State::Recording);
        assert!(h.controller.settings_snapshot().punct_enabled);
        let mut outputs = Vec::new();
        h.controller
            .handle_hotkey_edge(HotkeyEdge::Released(HotkeyKey::Standard), &mut outputs);
        h.controller.execute_outputs(outputs);
        h.engine(EngineKind::Local)
            .send(final_message(1, "クバネティス"));
        let deadline = Instant::now() + Duration::from_secs(1);
        while h.pastes.attempts.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline);
            h.turn();
            thread::yield_now();
        }
        assert_eq!(*h.pastes.attempts.lock().unwrap(), ["Kubernetes。"]);
        assert_eq!(
            h.engine(EngineKind::Whisper).builds.load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn long_install_failure_compensation_write_failure_keeps_runtime_disabled() {
        let mut h = LifecycleHarness::new(false, true);
        h.boot(false);
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.apply(true).unwrap();
        std::fs::write(h._temp.path().join("config.toml"), "# external change\n").unwrap();
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Failed("download failed".into()));
        h.turn();
        assert!(h.controller.settings.config().long_key);
        assert!(!h.controller.long_desired);
        assert!(h.controller.long.is_none());
        assert_eq!(*h.hotkeys.unregistered.lock().unwrap(), [HotkeyKey::Long]);
        assert!(h
            .controller
            .long_notice
            .as_deref()
            .unwrap()
            .contains("設定ファイルが外部"));
    }

    #[test]
    fn boot_long_install_failure_keeps_setting_and_can_retry() {
        let mut h = LifecycleHarness::new(true, true);
        h.long_startup.hold_starts.store(true, Ordering::Relaxed);
        h.boot(false);
        h.long_startup
            .events
            .lock()
            .unwrap()
            .push_back(StartupEvent::Failed("boot download failed".into()));
        h.turn();
        assert!(h.controller.settings.config().long_key);
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Failed { .. }
        ));
        h.long_startup.hold_starts.store(false, Ordering::Relaxed);
        h.retry().unwrap();
        h.await_retirement();
        h.turn();
        assert!(matches!(
            h.controller.long.as_ref().unwrap().state,
            LongState::Ready
        ));
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
                    .message
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
                .message
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
            snapshot.status.message.as_deref().is_some_and(|message| {
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
    fn long_enable_keeps_standard_postprocess_punctuation_enabled() {
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
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    long_key: Some(true),
                    ..SettingsPatch::default()
                },
                reply,
            })
            .unwrap();
        wait_snapshot(&harness, |snapshot| {
            matches!(
                snapshot.settings.long_state,
                Some(crate::status::LongStateView::Ready)
            )
        });

        wait_for_engine_instances(&harness, 2);
        assert_eq!(harness.long_startup.plans.lock().unwrap().len(), 1);
        transcribe_with(&harness, EngineKind::Local, 1, "こんにちは");

        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "こんにちは。"
        );
    }

    #[test]
    fn local_applies_builtin_replacement_dictionary_with_long_resident() {
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
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    long_key: Some(true),
                    ..SettingsPatch::default()
                },
                reply,
            })
            .unwrap();
        wait_snapshot(&harness, |snapshot| {
            matches!(
                snapshot.settings.long_state,
                Some(crate::status::LongStateView::Ready)
            )
        });

        apply_builtin(&harness, true);
        transcribe_with(&harness, EngineKind::Local, 1, "クバネティス");

        assert_eq!(
            harness.pastes.attempts.lock().unwrap().last().unwrap(),
            "Kubernetes"
        );
    }

    #[test]
    fn whisper_builtin_toggle_does_not_restart_engine() {
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
        harness
            .client
            .send_and_wait(|reply| ControllerCmd::ApplySettings {
                patch: SettingsPatch {
                    long_key: Some(true),
                    ..SettingsPatch::default()
                },
                reply,
            })
            .unwrap();
        wait_snapshot(&harness, |snapshot| {
            matches!(
                snapshot.settings.long_state,
                Some(crate::status::LongStateView::Ready)
            )
        });
        let builds_before = harness
            .engine(EngineKind::Whisper)
            .builds
            .load(Ordering::Relaxed);

        apply_builtin(&harness, true);
        thread::sleep(Duration::from_millis(60));

        assert_eq!(
            harness
                .engine(EngineKind::Whisper)
                .builds
                .load(Ordering::Relaxed),
            builds_before
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
        assert_eq!(interim.status.message.as_deref(), Some("connection lost"));
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
        let pressed = hotkey_input(
            HotkeyEdge::Pressed(HotkeyKey::Standard),
            false,
            false,
            EngineKind::Local,
        )
        .unwrap();
        session.handle(pressed, now);
        assert_eq!(session.state(), State::Recording);

        let released = hotkey_input(
            HotkeyEdge::Released(HotkeyKey::Standard),
            false,
            true,
            EngineKind::Local,
        )
        .unwrap();
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

    #[test]
    fn engine_final_is_processed_without_waiting_for_the_poll_tick() {
        let harness = ready_harness_with_poll_interval(Duration::from_millis(500));
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
        harness
            .hotkey
            .send(HotkeyEdge::Pressed(HotkeyKey::Standard));
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
                    hotkey: Some("Alt+B".into()),
                    ..SettingsPatch::default()
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

    fn capture_harness(enabled: bool, capable: bool) -> LifecycleHarness {
        let mut h = LifecycleHarness::new(enabled, capable);
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

    fn assert_both_restored(h: &LifecycleHarness) {
        assert_eq!(
            *h.hotkeys.registered.lock().unwrap(),
            [
                (HotkeyKey::Standard, "Alt+Space".into()),
                (HotkeyKey::Long, "Alt+Shift+Space".into()),
            ]
        );
        assert!(h.controller.long_hotkey_registered);
        assert!(!h.controller.capturing_hotkey);
    }

    #[test]
    fn capture_unregisters_and_restores_both_chords() {
        for key in [HotkeyKey::Standard, HotkeyKey::Long] {
            for chord in [
                None,
                Some(if key == HotkeyKey::Standard {
                    "Alt+Space"
                } else {
                    "Alt+Shift+Space"
                }),
            ] {
                let mut h = capture_harness(true, true);
                begin_capture(&mut h, key).unwrap();
                assert_eq!(
                    *h.hotkeys.unregistered.lock().unwrap(),
                    [HotkeyKey::Standard, HotkeyKey::Long]
                );
                assert!(h.controller.capturing_hotkey);
                assert!(!h.controller.long_hotkey_registered);
                end_capture(&mut h, key, chord).unwrap();
                assert_both_restored(&h);
            }
        }
    }

    #[test]
    fn capture_long_persists_long_hotkey_only() {
        let mut h = capture_harness(true, true);
        begin_capture(&mut h, HotkeyKey::Long).unwrap();
        let snapshot = end_capture(&mut h, HotkeyKey::Long, Some("Cmd+Z")).unwrap();
        assert_eq!(snapshot.hotkey, "Alt+Space");
        assert_eq!(snapshot.long_hotkey, "Cmd+Z");
        assert!(snapshot.long_key);
        let reloaded = Config::load(Some(&h._temp.path().join("config.toml"))).unwrap();
        assert_eq!(reloaded.hotkey, "Alt+Space");
        assert_eq!(reloaded.long_hotkey, "Cmd+Z");
        assert!(reloaded.long_key);
        assert_eq!(
            *h.hotkeys.registered.lock().unwrap(),
            [
                (HotkeyKey::Long, "Cmd+Z".into()),
                (HotkeyKey::Standard, "Alt+Space".into()),
            ]
        );
        assert!(h.controller.long_hotkey_registered);
    }

    #[test]
    fn capture_conflict_rejected_before_registration() {
        for (key, chord) in [
            (HotkeyKey::Long, "  alt+SPACE  "),
            (HotkeyKey::Standard, "  alt+SHIFT+space  "),
        ] {
            let mut h = capture_harness(true, true);
            begin_capture(&mut h, key).unwrap();
            let error = end_capture(&mut h, key, Some(chord)).unwrap_err();
            assert_eq!(error.code, "hotkey_conflict");
            assert_eq!(error.message, "もう一方のキーと同じ組み合わせは使えません");
            assert_both_restored(&h);
            assert_eq!(h.controller.settings.config().hotkey, "Alt+Space");
            assert_eq!(
                h.controller.settings.config().long_hotkey,
                "Alt+Shift+Space"
            );
        }
    }

    #[test]
    fn capture_long_rejected_when_long_key_off() {
        for (enabled, capable) in [(false, true), (true, false)] {
            let mut h = capture_harness(enabled, capable);
            let error = begin_capture(&mut h, HotkeyKey::Long).unwrap_err();
            assert_eq!(error.code, "long_key_off");
            assert!(!h.controller.capturing_hotkey);
            assert!(h.hotkeys.unregistered.lock().unwrap().is_empty());
            assert_eq!(
                end_capture(&mut h, HotkeyKey::Long, Some("Cmd+Z"))
                    .unwrap_err()
                    .code,
                "long_key_off"
            );
            assert!(h.hotkeys.registered.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn capture_persist_failure_restores_both() {
        for key in [HotkeyKey::Standard, HotkeyKey::Long] {
            let mut h = capture_harness(true, true);
            begin_capture(&mut h, key).unwrap();
            std::fs::write(h._temp.path().join("config.toml"), "# external edit\n").unwrap();
            let error = end_capture(&mut h, key, Some("Cmd+Z")).unwrap_err();
            assert_eq!(error.code, "external_change");
            let calls = h.hotkeys.registered.lock().unwrap().clone();
            assert!(calls.contains(&(key, "Cmd+Z".into())));
            assert_eq!(
                &calls[calls.len() - 2..],
                [
                    (HotkeyKey::Standard, "Alt+Space".into()),
                    (HotkeyKey::Long, "Alt+Shift+Space".into()),
                ]
            );
            assert!(h.controller.long_hotkey_registered);
            assert!(!h.controller.capturing_hotkey);
            assert_eq!(h.controller.settings.config().hotkey, "Alt+Space");
            assert_eq!(
                h.controller.settings.config().long_hotkey,
                "Alt+Shift+Space"
            );
        }
    }

    #[test]
    fn capture_standard_conflicting_with_long_hotkey_rejected_even_when_long_off() {
        let mut h = capture_harness(false, true);
        begin_capture(&mut h, HotkeyKey::Standard).unwrap();
        let error =
            end_capture(&mut h, HotkeyKey::Standard, Some("  alt+SHIFT+space  ")).unwrap_err();
        assert_eq!(error.code, "hotkey_conflict");
        assert_eq!(error.message, "もう一方のキーと同じ組み合わせは使えません");
        assert_eq!(
            *h.hotkeys.registered.lock().unwrap(),
            [(HotkeyKey::Standard, "Alt+Space".into())]
        );
        assert!(!h.controller.long_hotkey_registered);
    }

    #[test]
    fn capture_restore_obeys_current_long_enablement_and_pending_disable() {
        for (enabled, pending) in [(false, false), (true, true)] {
            let mut h = capture_harness(true, true);
            begin_capture(&mut h, HotkeyKey::Standard).unwrap();
            h.controller
                .settings
                .apply(SettingsPatch {
                    long_key: Some(enabled),
                    ..SettingsPatch::default()
                })
                .unwrap();
            h.controller.long_disable_pending = pending;
            end_capture(&mut h, HotkeyKey::Standard, None).unwrap();
            assert_eq!(
                *h.hotkeys.registered.lock().unwrap(),
                [(HotkeyKey::Standard, "Alt+Space".into())]
            );
            assert!(!h.controller.long_hotkey_registered);
        }
    }

    #[test]
    fn capture_registration_failure_restores_both_and_warns_on_each_restore_failure() {
        for key in [HotkeyKey::Standard, HotkeyKey::Long] {
            for fail_restores in [false, true] {
                let mut h = capture_harness(true, true);
                begin_capture(&mut h, key).unwrap();
                h.hotkeys.plan.lock().unwrap().extend([
                    Err("new failed".into()),
                    if fail_restores {
                        Err("standard restore failed".into())
                    } else {
                        Ok(())
                    },
                    if fail_restores {
                        Err("long restore failed".into())
                    } else {
                        Ok(())
                    },
                ]);
                let error = end_capture(&mut h, key, Some("Cmd+Z")).unwrap_err();
                assert_eq!(error.code, "register_failed");
                assert_eq!(error.message, "new failed");
                let calls = h.hotkeys.registered.lock().unwrap().clone();
                assert_eq!(calls[0], (key, "Cmd+Z".into()));
                assert_eq!(
                    &calls[1..],
                    [
                        (HotkeyKey::Standard, "Alt+Space".into()),
                        (HotkeyKey::Long, "Alt+Shift+Space".into()),
                    ]
                );
                assert_eq!(h.controller.long_hotkey_registered, !fail_restores);
                if fail_restores {
                    assert_eq!(
                        h.controller.status.message(),
                        Some("ホットキーが無効 — 再設定してください")
                    );
                    assert_eq!(
                        h.controller.long_notice.as_deref(),
                        Some(LONG_HOTKEY_INVALID_NOTICE)
                    );
                    // The broken standard key outranks the long-key notice.
                    assert_eq!(
                        h.controller.snapshot().status.message.as_deref(),
                        h.controller.status.message()
                    );
                } else {
                    assert!(h.controller.long_notice.is_none());
                }
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
            [HotkeyKey::Standard, HotkeyKey::Long]
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
            snapshot.status.message.as_deref() == Some("bad model")
        });
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
            snapshot.status.message.as_deref() == Some("engine stopped")
        });
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
