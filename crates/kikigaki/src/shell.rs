//! macOS Tauri shell and adapters for the platform-independent controller.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use kikigaki_core::engine::EnginePhase;
use kikigaki_core::session::State;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::controller::{ControllerEventSink as _, HotkeyPort as _};

pub fn run() -> anyhow::Result<()> {
    let process_started_at = Instant::now();
    init_logging()?;
    let (client, cmd_rx, hotkey, hotkey_rx) = crate::controller::client_channels();
    let setup_client = client;

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("settings") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |_app, _shortcut, event| {
                    let edge = match event.state() {
                        ShortcutState::Pressed => crate::controller::HotkeyEdge::Pressed,
                        ShortcutState::Released => crate::controller::HotkeyEdge::Released,
                    };
                    hotkey.send(edge);
                })
                .build(),
        )
        .setup(move |app| setup(app, setup_client, cmd_rx, hotkey_rx, process_started_at))
        .on_window_event(on_window_event)
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            apply_settings,
            begin_hotkey_capture,
            end_hotkey_capture,
            set_launch_at_login,
            retry_bootstrap,
            open_config,
            open_settings_pane,
            request_microphone_access,
            start_download,
            retry_download,
            list_history,
            preview_correction,
            remember_correction,
            delete_learned_rule,
            clear_history,
            quit,
        ])
        .build(tauri::generate_context!())?;
    app.run(handle_run_event);
    Ok(())
}

fn init_logging() -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let log_dir = home.join("Library/Logs/kikigaki");
    std::fs::create_dir_all(&log_dir)?;
    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("kikigaki")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&log_dir)
        .context("create the seven-file rotating log appender")?;
    // Blocking writes on purpose: the process ends with `app.exit()` → `process::exit`, which never
    // flushes a `non_blocking` worker, and the lines that would be lost are exactly the shutdown
    // sequence this log exists to prove. Log volume is a few lines per utterance, so this is cheap.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("kikigaki=info")),
        )
        .with_writer(appender)
        .init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        log_dir = %log_dir.display(),
        "app started"
    );
    Ok(())
}

fn setup(
    app: &mut tauri::App,
    client: crate::controller::ControllerClient,
    cmd_rx: std::sync::mpsc::Receiver<crate::controller::ControllerCmd>,
    hotkey_rx: std::sync::mpsc::Receiver<crate::controller::HotkeyEdge>,
    process_started_at: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    app.handle()
        .set_activation_policy(tauri::ActivationPolicy::Accessory)?;

    let handle = app.handle().clone();
    let window = app
        .get_webview_window("settings")
        .expect("settings window declared in tauri.conf.json");
    let (bootstrap, settings, capabilities) = resolve_bootstrap();
    let (onboarding_now, bootstrap_error) = match &bootstrap {
        crate::controller::BootstrapOutcome::ReadyToStart => {
            tracing::info!(outcome = "ready_to_start", "config load completed");
            (None, None)
        }
        crate::controller::BootstrapOutcome::NeedsOnboarding(state) => {
            tracing::info!(outcome = "needs_onboarding", "config load completed");
            (Some(state.clone()), None)
        }
        crate::controller::BootstrapOutcome::ConfigError(error) => {
            tracing::info!(
                outcome = "config_error",
                code = error.code,
                "config load completed"
            );
            (None, Some(error.clone()))
        }
    };

    assert!(
        app.manage(Arc::new(ShellState {
            latest: Mutex::new(default_snapshot(
                &handle,
                &settings,
                onboarding_now.clone(),
                bootstrap_error.clone(),
            )),
            revision: std::sync::atomic::AtomicU64::new(0),
            window_focused: std::sync::atomic::AtomicBool::new(false),
            client,
            controller: Mutex::new(None),
            controller_ports: Mutex::new(Some(ControllerPorts { cmd_rx, hotkey_rx })),
            process_started_at,
            shutdown: crate::shutdown::ShutdownState::new(),
            onboarding_poll: Mutex::new(None),
        })),
        "ShellState must only be managed once"
    );

    build_tray(app, &handle)?;
    tracing::info!("tray built");
    if onboarding_now.is_some() || bootstrap_error.is_some() {
        let _ = window.show();
        let _ = window.set_focus();
        tracing::info!(reason = "bootstrap", "window shown");
    }
    if matches!(
        &bootstrap,
        crate::controller::BootstrapOutcome::ConfigError(_)
    ) {
        return Ok(());
    }

    let state = handle.state::<Arc<ShellState>>();
    let mut controller = state.controller.lock().unwrap();
    start_controller(&handle, bootstrap, settings, capabilities, &mut controller)
        .map_err(std::io::Error::other)?;
    Ok(())
}

fn start_controller(
    handle: &AppHandle,
    bootstrap: crate::controller::BootstrapOutcome,
    settings: crate::settings::SettingsCoordinator,
    capabilities: kikigaki_core::config::Capabilities,
    controller_slot: &mut Option<(
        crate::controller::ControllerHandle,
        std::thread::JoinHandle<()>,
    )>,
) -> Result<(), String> {
    if controller_slot.is_some() {
        return Err("controller is already running".into());
    }
    let state = handle.state::<Arc<ShellState>>();
    if matches!(
        &bootstrap,
        crate::controller::BootstrapOutcome::NeedsOnboarding(_)
    ) {
        *state.onboarding_poll.lock().unwrap() = Some(spawn_onboarding_poll(handle.clone()));
    }

    let cfg = settings.config().clone();
    let hotkeys = TauriHotkeys(handle.clone());
    if let Err(error) = hotkeys.register(&cfg.hotkey) {
        tracing::error!(hotkey = %cfg.hotkey, %error, "failed to register the initial hotkey");
    } else {
        tracing::info!(chord = %cfg.hotkey, "initial hotkey registered");
    }
    let startup_plan = kikigaki_core::startup::StartupPlan::from_config(&cfg, capabilities);
    let engine_factory = Box::new(TauriEngineFactory {
        engine_kind: cfg.engine,
        local_models_dir: cfg.models_dir.clone(),
        local_asr_cfg: cfg.asr.clone(),
        local_vad_cfg: cfg.vad.clone(),
        remote_cfg: cfg.remote.clone(),
    });
    let builtin = match kikigaki_core::replace::builtin_rules() {
        Ok(rules) => Arc::new(rules),
        Err(error) => {
            tracing::error!(%error, "failed to parse builtin replacement dictionary");
            Arc::new(kikigaki_core::replace::Rules::default())
        }
    };
    let postprocess_factory = Box::new(TauriPostprocessFactory {
        models_dir: cfg.models_dir.clone(),
        replace_file: cfg.replace_file.clone(),
        punct_cfg: cfg.punct.clone(),
        num_threads: usize::try_from(cfg.asr.num_threads).unwrap_or(1),
        builtin,
    });
    let startup_port = Box::new(crate::startup::StartupCoordinatorPort::new(startup_plan));
    let models_dir = cfg.models_dir.clone();
    let ports = state
        .controller_ports
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| "controller ports are no longer available".to_owned())?;
    let (controller, exited_rx) = crate::controller::spawn(
        crate::controller::ControllerConfig {
            settings,
            capabilities,
            models_dir,
            bootstrap,
            process_started_at: state.process_started_at,
        },
        ports.cmd_rx,
        ports.hotkey_rx,
        state.client.sender_for_handle(),
        Box::new(TauriSink(handle.clone())),
        Box::new(hotkeys),
        Box::new(TauriPaster(handle.clone())),
        engine_factory,
        Box::new(crate::mic::Mic::new()),
        postprocess_factory,
        startup_port,
        Box::new(crate::controller::RealClock),
    );
    let controller_death = spawn_controller_death_monitor(handle.clone(), exited_rx);
    *controller_slot = Some((controller, controller_death));
    tracing::info!("controller spawned");
    Ok(())
}

fn resolve_bootstrap() -> (
    crate::controller::BootstrapOutcome,
    crate::settings::SettingsCoordinator,
    kikigaki_core::config::Capabilities,
) {
    let capabilities = crate::cli::capabilities();
    let path = kikigaki_core::config::default_path();
    match crate::settings::SettingsCoordinator::load(path, capabilities) {
        Err(error) => {
            let fallback = crate::settings::SettingsCoordinator::load(
                std::env::temp_dir().join(format!(
                    "kikigaki-bootstrap-defaults-{}-unreachable.toml",
                    std::process::id()
                )),
                capabilities,
            )
            .expect("defaults-only load never fails");
            (
                crate::controller::BootstrapOutcome::ConfigError(crate::status::BootstrapError {
                    code: "config_error",
                    message: format!("{error:#}"),
                }),
                fallback,
                capabilities,
            )
        }
        Ok(settings) => {
            if let Err(error) = settings.config().validate(capabilities) {
                return (
                    crate::controller::BootstrapOutcome::ConfigError(
                        crate::status::BootstrapError {
                            code: "invalid_config",
                            message: format!("{error:#}"),
                        },
                    ),
                    settings,
                    capabilities,
                );
            }
            let scan_startup = crate::startup::StartupCoordinatorPort::new(
                kikigaki_core::startup::StartupPlan::from_config(settings.config(), capabilities),
            );
            match crate::onboarding::scan(&settings, &scan_startup) {
                Some(state) => (
                    crate::controller::BootstrapOutcome::NeedsOnboarding(state),
                    settings,
                    capabilities,
                ),
                None => (
                    crate::controller::BootstrapOutcome::ReadyToStart,
                    settings,
                    capabilities,
                ),
            }
        }
    }
}

fn default_snapshot(
    app: &AppHandle,
    settings: &crate::settings::SettingsCoordinator,
    onboarding: Option<crate::status::OnboardingState>,
    bootstrap_error: Option<crate::status::BootstrapError>,
) -> crate::status::UiSnapshot {
    let settings_snapshot = settings.snapshot();
    let engine = match settings.config().engine {
        kikigaki_core::config::EngineKind::Local => "local",
        kikigaki_core::config::EngineKind::Remote => "remote",
    };
    let phase = if bootstrap_error.is_some() {
        "failed"
    } else {
        "starting"
    };
    crate::status::UiSnapshot {
        revision: 0,
        status: crate::status::UiStatus {
            state: "disconnected",
            phase,
            message: bootstrap_error.as_ref().map(|error| error.message.clone()),
            hotkey: settings_snapshot.hotkey.clone(),
            punct_enabled: settings_snapshot.punct_enabled,
            strip_trailing_period: settings_snapshot.strip_trailing_period,
            engine,
        },
        settings: settings_snapshot,
        autostart: autostart_state(app),
        onboarding,
        history: Vec::new(),
        learned_rules: Vec::new(),
        bootstrap_error,
    }
}

struct TauriEngineFactory {
    engine_kind: kikigaki_core::config::EngineKind,
    local_models_dir: std::path::PathBuf,
    local_asr_cfg: kikigaki_core::config::AsrConfig,
    local_vad_cfg: kikigaki_core::config::VadConfig,
    #[cfg_attr(not(feature = "remote-engine"), allow(dead_code))]
    remote_cfg: kikigaki_core::config::RemoteConfig,
}

impl crate::controller::EngineFactory for TauriEngineFactory {
    fn build(&self) -> anyhow::Result<Box<dyn kikigaki_core::engine::Engine>> {
        match self.engine_kind {
            kikigaki_core::config::EngineKind::Local => kikigaki_engine::local::LocalEngine::start(
                self.local_models_dir.clone(),
                self.local_asr_cfg.clone(),
                self.local_vad_cfg.clone(),
            )
            .map(|engine| Box::new(engine) as Box<dyn kikigaki_core::engine::Engine>),
            #[cfg(feature = "remote-engine")]
            kikigaki_core::config::EngineKind::Remote => {
                kikigaki_core::remote::RemoteEngine::start(
                    self.remote_cfg.clone(),
                    Box::new(crate::sidecar::MacSpawner),
                )
                .map(|engine| Box::new(engine) as Box<dyn kikigaki_core::engine::Engine>)
            }
            #[cfg(not(feature = "remote-engine"))]
            kikigaki_core::config::EngineKind::Remote => {
                anyhow::bail!("engine = \"remote\" requires a build with the remote-engine feature")
            }
        }
    }
}

struct TauriPostprocessFactory {
    models_dir: std::path::PathBuf,
    replace_file: std::path::PathBuf,
    punct_cfg: kikigaki_core::config::PunctConfig,
    num_threads: usize,
    builtin: Arc<kikigaki_core::replace::Rules>,
}

impl crate::controller::PostprocessFactory for TauriPostprocessFactory {
    fn build(
        &self,
        learned: Arc<kikigaki_core::replace::Rules>,
        builtin_enabled: bool,
        waker: Option<kikigaki_core::engine::Waker>,
    ) -> anyhow::Result<kikigaki_core::postprocess::PostprocessWorker> {
        let punctuator: Box<dyn kikigaki_core::postprocess::Punctuator> = {
            #[cfg(feature = "punct")]
            {
                Box::new(kikigaki_engine::punct::MojicastPunctuator::new(
                    self.models_dir.clone(),
                    kikigaki_core::punct::Thresholds {
                        comma: self.punct_cfg.comma_threshold,
                        period: self.punct_cfg.period_threshold,
                        force_final_period: true,
                    },
                    self.num_threads,
                )?)
            }
            #[cfg(not(feature = "punct"))]
            {
                Box::new(kikigaki_core::postprocess::NoopPunctuator)
            }
        };
        let replace = kikigaki_core::replace::ReplaceFile::new(self.replace_file.clone());
        Ok(
            kikigaki_core::postprocess::PostprocessWorker::spawn_with_waker(
                kikigaki_core::postprocess::Pipeline::new(
                    replace,
                    punctuator,
                    false,
                    false,
                    Arc::clone(&self.builtin),
                    builtin_enabled,
                    learned,
                ),
                waker,
            ),
        )
    }
}

impl crate::controller::MicPort for crate::mic::Mic {
    fn start(&mut self, sink: kikigaki_core::engine::AudioSink) -> anyhow::Result<()> {
        crate::mic::Mic::start(self, sink)
    }

    fn stop(&mut self) {
        crate::mic::Mic::stop(self);
    }
}

struct TrayViewModel {
    tooltip: String,
    icon_rgba: Vec<u8>,
}

fn build_tray(app: &tauri::App, handle: &AppHandle) -> tauri::Result<()> {
    let tray_handle = handle.clone();
    tauri::tray::TrayIconBuilder::with_id("main")
        .icon(
            app.default_window_icon()
                .expect("bundle icon missing")
                .clone(),
        )
        .tooltip("kikigaki — 起動中")
        .on_tray_icon_event(move |_tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                if let Some(window) = tray_handle.get_webview_window("settings") {
                    let visible = window.is_visible().unwrap_or(false);
                    let focused = window.is_focused().unwrap_or(false);
                    if visible && focused {
                        let _ = window.hide();
                        tracing::info!(source = "tray click", "window hidden");
                    } else {
                        let _ = window.show();
                        let _ = window.set_focus();
                        tracing::info!(source = "tray click", "window shown");
                    }
                }
            }
        })
        .build(app)
        .map(|_| ())
}

fn apply_tray_view_model(app: &AppHandle, model: TrayViewModel) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(&model.tooltip));
        let icon = tauri::image::Image::new_owned(model.icon_rgba, 22, 22);
        let _ = tray.set_icon(Some(icon));
    }
}

fn tray_view_model(snapshot: &crate::status::UiSnapshot) -> TrayViewModel {
    let state = match snapshot.status.state {
        "idle" => State::Idle,
        "recording" => State::Recording,
        "finalizing" => State::Finalizing,
        _ => State::Disconnected,
    };
    let phase = match snapshot.status.phase {
        "ready" => EnginePhase::Ready,
        "failed" => EnginePhase::Failed,
        _ => EnginePhase::Starting,
    };
    let tooltip = snapshot
        .status
        .message
        .as_deref()
        .map(|message| format!("kikigaki — {message}"))
        .unwrap_or_else(|| crate::tray::tooltip(state, phase).to_owned());
    TrayViewModel {
        tooltip,
        icon_rgba: crate::tray::icon_rgba(state, phase),
    }
}

pub(crate) struct ShellState {
    pub(crate) latest: Mutex<crate::status::UiSnapshot>,
    revision: std::sync::atomic::AtomicU64,
    window_focused: std::sync::atomic::AtomicBool,
    pub(crate) client: crate::controller::ControllerClient,
    pub(crate) controller: Mutex<
        Option<(
            crate::controller::ControllerHandle,
            std::thread::JoinHandle<()>,
        )>,
    >,
    controller_ports: Mutex<Option<ControllerPorts>>,
    process_started_at: Instant,
    pub(crate) shutdown: crate::shutdown::ShutdownState,
    onboarding_poll: Mutex<Option<OnboardingPoll>>,
}

impl ShellState {
    pub(crate) fn stop_onboarding_poll(&self) {
        drop(self.onboarding_poll.lock().unwrap().take());
    }
}

struct ControllerPorts {
    cmd_rx: std::sync::mpsc::Receiver<crate::controller::ControllerCmd>,
    hotkey_rx: std::sync::mpsc::Receiver<crate::controller::HotkeyEdge>,
}

struct TauriSink(AppHandle);

impl crate::controller::ControllerEventSink for TauriSink {
    fn publish(&self, snapshot: crate::status::UiSnapshot) {
        publish_snapshot(&self.0, snapshot);
    }
}

fn publish_snapshot(app: &AppHandle, mut snapshot: crate::status::UiSnapshot) {
    let Some(state) = app.try_state::<Arc<ShellState>>() else {
        return;
    };
    let revision = state
        .revision
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1;
    snapshot.revision = revision;
    snapshot.autostart = autostart_state(app);
    if snapshot.onboarding.is_none() {
        state.stop_onboarding_poll();
    }
    *state.latest.lock().unwrap() = snapshot.clone();
    let _ = app.emit(
        "kikigaki://event",
        &crate::status::UiEvent {
            revision,
            kind: crate::status::UiEventKind::Snapshot(Box::new(snapshot.clone())),
        },
    );
    let tray_app = app.clone();
    let model = tray_view_model(&snapshot);
    let _ = app.run_on_main_thread(move || apply_tray_view_model(&tray_app, model));
}

fn autostart_state(app: &AppHandle) -> crate::status::AutostartState {
    crate::status::AutostartState {
        available: is_relocatable_install(),
        enabled: app.autolaunch().is_enabled().unwrap_or(false),
    }
}

struct TauriHotkeys(AppHandle);

impl crate::controller::HotkeyPort for TauriHotkeys {
    fn register(&self, chord: &str) -> Result<(), String> {
        let shortcut: Shortcut = chord.parse().map_err(|error| format!("{error}"))?;
        self.0
            .global_shortcut()
            .register(shortcut)
            .map_err(|error| format!("{error}"))
    }

    fn unregister(&self, chord: &str) -> Result<(), String> {
        let shortcut: Shortcut = chord.parse().map_err(|error| format!("{error}"))?;
        self.0
            .global_shortcut()
            .unregister(shortcut)
            .map_err(|error| format!("{error}"))
    }
}

struct TauriPaster(AppHandle);

impl crate::controller::Paster for TauriPaster {
    fn paste(&self, text: String, deadline: Instant) -> Result<(), String> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let app = self.0.clone();
        let scheduled = self.0.run_on_main_thread(move || {
            if Instant::now() > deadline {
                let _ = reply_tx.send(Err(
                    "paste deadline exceeded before the main thread could run it".to_owned(),
                ));
                return;
            }
            let method = {
                let state = app.state::<Arc<ShellState>>();
                let method = state.latest.lock().unwrap().settings.paste_method;
                method
            };
            let _ = reply_tx
                .send(crate::paste::paste(&text, method).map_err(|error| format!("{error:#}")));
        });
        match scheduled {
            Ok(()) => reply_rx
                .recv_timeout(Duration::from_secs(3))
                .map_err(|error| format!("main-thread paste did not reply: {error}"))
                .and_then(|result| result),
            Err(error) => Err(format!("run_on_main_thread failed: {error}")),
        }
    }
}

fn on_window_event(window: &tauri::Window, event: &WindowEvent) {
    if window.label() != "settings" {
        return;
    }
    let Some(state) = window.try_state::<Arc<ShellState>>() else {
        return;
    };
    match event {
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = window.hide();
            tracing::info!(source = "window close", "window hidden");
            state
                .window_focused
                .store(false, std::sync::atomic::Ordering::SeqCst);
            state
                .client
                .send(crate::controller::ControllerCmd::SetWindowFocused(false));
        }
        WindowEvent::Focused(focused) => {
            if *focused {
                tracing::info!("window focused");
            } else {
                tracing::info!("window unfocused");
            }
            state
                .window_focused
                .store(*focused, std::sync::atomic::Ordering::SeqCst);
            state
                .client
                .send(crate::controller::ControllerCmd::SetWindowFocused(*focused));
        }
        _ => {}
    }
}

struct OnboardingPoll {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for OnboardingPoll {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn spawn_onboarding_poll(app: AppHandle) -> OnboardingPoll {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let _ = std::thread::Builder::new()
        .name("onboarding-poll".into())
        .spawn(move || {
            tracing::info!("onboarding poll started");
            while !stop_flag.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(2));
                let Some(window) = app.get_webview_window("settings") else {
                    break;
                };
                if !window.is_visible().unwrap_or(false) {
                    continue;
                }
                let Some(state) = app.try_state::<Arc<ShellState>>() else {
                    break;
                };
                if state.latest.lock().unwrap().onboarding.is_none() {
                    break;
                }
                state
                    .client
                    .send(crate::controller::ControllerCmd::RescanOnboarding);
            }
            tracing::info!("onboarding poll stopped");
        })
        .expect("spawn onboarding poll");
    OnboardingPoll { stop }
}

fn spawn_controller_death_monitor(
    app: AppHandle,
    exited_rx: std::sync::mpsc::Receiver<()>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("controller-death-monitor".into())
        .spawn(move || {
            if exited_rx.recv().is_err() {
                if let Some(state) = app.try_state::<Arc<ShellState>>() {
                    if state.shutdown.is_running() {
                        tracing::info!(source = "controller death", "quit requested");
                        let mut snapshot = state.latest.lock().unwrap().clone();
                        snapshot.status.state = "disconnected";
                        snapshot.status.phase = "failed";
                        snapshot.status.message = Some("controller stopped".into());
                        TauriSink(app.clone()).publish(snapshot);
                    }
                }
                crate::shutdown::begin_shutdown_with_code(&app, 1);
            }
        })
        .expect("spawn controller-death monitor")
}

fn handle_run_event(app: &AppHandle, event: RunEvent) {
    match event {
        RunEvent::ExitRequested { api, code, .. } if code.is_none() => {
            api.prevent_exit();
            tracing::info!(source = "close-requested", "quit requested");
            crate::shutdown::begin_shutdown(app);
        }
        // NSApp terminate (Apple event, logout, Activity Monitor) skips `ExitRequested`
        // entirely — see `shutdown::finish_synchronously`.
        RunEvent::Exit => crate::shutdown::finish_synchronously(app, "system terminate"),
        _ => {}
    }
}

#[tauri::command]
fn quit(app: AppHandle) {
    // Both UI paths invoke this command; Tauri does not preserve which DOM event initiated it.
    tracing::info!(source = "window button / Cmd+Q", "quit requested");
    crate::shutdown::begin_shutdown(&app);
}

#[tauri::command]
fn get_snapshot(state: tauri::State<'_, Arc<ShellState>>) -> crate::status::UiSnapshot {
    state.latest.lock().unwrap().clone()
}

#[tauri::command]
async fn apply_settings(
    state: tauri::State<'_, Arc<ShellState>>,
    patch: crate::settings::SettingsPatch,
) -> Result<crate::settings::SettingsSnapshot, crate::status::UiError> {
    state
        .client
        .send_and_wait(|reply| crate::controller::ControllerCmd::ApplySettings { patch, reply })
}

#[tauri::command]
async fn begin_hotkey_capture(
    state: tauri::State<'_, Arc<ShellState>>,
) -> Result<(), crate::status::UiError> {
    state
        .client
        .send_and_wait(|reply| crate::controller::ControllerCmd::BeginHotkeyCapture { reply })
}

#[tauri::command]
async fn end_hotkey_capture(
    state: tauri::State<'_, Arc<ShellState>>,
    new_chord: Option<String>,
) -> Result<crate::settings::SettingsSnapshot, crate::status::UiError> {
    state
        .client
        .send_and_wait(|reply| crate::controller::ControllerCmd::EndHotkeyCapture {
            new_chord,
            reply,
        })
}

#[tauri::command]
fn set_launch_at_login(app: AppHandle, enabled: bool) -> Result<(), crate::status::UiError> {
    if !is_relocatable_install() {
        return Err(crate::status::UiError {
            code: "not_relocatable",
            message: "/Applications 以外にインストールされているため設定できません".into(),
        });
    }
    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    result.map_err(|error| crate::status::UiError {
        code: "autostart_failed",
        message: format!("{error}"),
    })?;
    let snapshot = {
        let state = app.state::<Arc<ShellState>>();
        let snapshot = state.latest.lock().unwrap().clone();
        snapshot
    };
    publish_snapshot(&app, snapshot);
    Ok(())
}

#[tauri::command]
fn retry_bootstrap(app: AppHandle) -> Result<(), crate::status::UiError> {
    let state = app.state::<Arc<ShellState>>();
    let mut controller = state.controller.lock().unwrap();
    if controller.is_some() {
        return Err(crate::status::UiError {
            code: "already_running",
            message: "controller is already running".into(),
        });
    }

    let (bootstrap, settings, capabilities) = resolve_bootstrap();
    let outcome = bootstrap.label();
    tracing::info!(outcome, "bootstrap retried");
    if let crate::controller::BootstrapOutcome::ConfigError(error) = &bootstrap {
        let snapshot = default_snapshot(&app, &settings, None, Some(error.clone()));
        publish_snapshot(&app, snapshot);
        return Err(crate::status::UiError {
            code: error.code,
            message: error.message.clone(),
        });
    }

    start_controller(&app, bootstrap, settings, capabilities, &mut controller).map_err(|message| {
        crate::status::UiError {
            code: "internal_error",
            message,
        }
    })
}

fn is_relocatable_install() -> bool {
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

#[tauri::command]
fn open_config(state: tauri::State<'_, Arc<ShellState>>) -> Result<(), crate::status::UiError> {
    let path = kikigaki_core::config::default_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if !path.exists() {
        let _ = std::fs::write(&path, "");
    }
    let _ = state;
    std::process::Command::new("open")
        .arg(&path)
        .status()
        .map_err(|error| crate::status::UiError {
            code: "open_failed",
            message: format!("{error}"),
        })?;
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum OnboardingPane {
    Microphone,
    Accessibility,
}

#[tauri::command]
fn open_settings_pane(pane: OnboardingPane) -> Result<(), crate::status::UiError> {
    let url = match pane {
        OnboardingPane::Microphone => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        }
        OnboardingPane::Accessibility => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
    };
    std::process::Command::new("open")
        .arg(url)
        .status()
        .map_err(|error| crate::status::UiError {
            code: "open_failed",
            message: format!("{error}"),
        })?;
    Ok(())
}

#[tauri::command]
async fn request_microphone_access() -> Result<bool, crate::status::UiError> {
    tokio::task::spawn_blocking(|| {
        crate::permissions::request_microphone_access(Duration::from_secs(120))
    })
    .await
    .map_err(|error| crate::status::UiError {
        code: "internal_error",
        message: format!("{error}"),
    })?
    .map_err(|message| crate::status::UiError {
        code: "mic_request_failed",
        message,
    })
}

#[tauri::command]
async fn start_download(
    state: tauri::State<'_, Arc<ShellState>>,
) -> Result<(), crate::status::UiError> {
    retry_download(state).await
}

#[tauri::command]
async fn retry_download(
    state: tauri::State<'_, Arc<ShellState>>,
) -> Result<(), crate::status::UiError> {
    state
        .client
        .send_and_wait(|reply| crate::controller::ControllerCmd::RetryDownload { reply })
}

#[tauri::command]
fn list_history(state: tauri::State<'_, Arc<ShellState>>) -> Vec<crate::history::HistoryEntry> {
    state.latest.lock().unwrap().history.clone()
}

#[tauri::command]
fn preview_correction(
    state: tauri::State<'_, Arc<ShellState>>,
    entry_id: u64,
    corrected: String,
) -> Result<crate::correction::Correction, crate::status::UiError> {
    let snapshot = state.latest.lock().unwrap();
    let entry = snapshot
        .history
        .iter()
        .find(|entry| entry.id == entry_id)
        .ok_or_else(|| crate::status::UiError {
            code: "not_found",
            message: "履歴が見つかりません".into(),
        })?;
    Ok(crate::correction::diff(&entry.raw, &corrected))
}

#[tauri::command]
async fn remember_correction(
    state: tauri::State<'_, Arc<ShellState>>,
    entry_id: u64,
    corrected: String,
) -> Result<(), crate::status::UiError> {
    state.client.send_and_wait(
        |reply| crate::controller::ControllerCmd::RememberCorrection {
            entry_id,
            corrected,
            reply,
        },
    )
}

#[tauri::command]
async fn delete_learned_rule(
    state: tauri::State<'_, Arc<ShellState>>,
    id: u64,
) -> Result<(), crate::status::UiError> {
    state
        .client
        .send_and_wait(|reply| crate::controller::ControllerCmd::DeleteLearnedRule { id, reply })
}

#[tauri::command]
async fn clear_history(
    state: tauri::State<'_, Arc<ShellState>>,
) -> Result<(), crate::status::UiError> {
    state
        .client
        .send_and_wait(|reply| crate::controller::ControllerCmd::ClearHistory { reply })
}
