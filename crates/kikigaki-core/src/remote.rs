//! Remote engine adapter for the hayamimi WebSocket ingest protocol.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc as tokio_mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::audio::{f32_to_s16le, silence_s16le};
use crate::config::RemoteConfig;
use crate::engine::{
    channel, event_channel, join_workers, run_worker, AudioSink, Engine, EngineCmd, EngineMsg,
    EventSender, Waker, AUDIO_CHANNEL_SLOTS, SHUTDOWN_TIMEOUT,
};
use crate::protocol::{hello_frame, Event, SAMPLE_RATE};

const RETRY_INTERVAL: Duration = Duration::from_millis(300);
const BRIDGE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SIDECAR_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Running sidecar process operations needed by the platform-independent remote engine.
pub trait SidecarProcess: Send {
    /// Returns whether the process is still running.
    fn is_running(&mut self) -> bool;
    /// Terminates the process best-effort.
    fn kill(&mut self);
}

/// Platform adapter that optionally starts a hayamimi sidecar.
pub trait SidecarSpawner: Send {
    /// Starts the configured sidecar, or returns `None` when an existing server should be used.
    fn spawn(&self, cfg: &RemoteConfig) -> anyhow::Result<Option<Box<dyn SidecarProcess>>>;
}

/// Engine implementation that bridges ordered synchronous commands to hayamimi over WebSocket.
pub struct RemoteEngine {
    sink: AudioSink,
    events_rx: Receiver<EngineMsg>,
    events_tx: EventSender,
    worker: Option<JoinHandle<()>>,
    bridge: Option<JoinHandle<()>>,
    done_rx: Receiver<()>,
    shutdown: Arc<AtomicBool>,
}

impl RemoteEngine {
    /// Starts bridge and Tokio worker threads for a remote engine connection.
    pub fn start(cfg: RemoteConfig, spawner: Box<dyn SidecarSpawner>) -> anyhow::Result<Self> {
        let (sink, cmd_rx) = channel();
        let (events_tx, events_rx) = event_channel(64);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let (async_tx, async_rx) = tokio_mpsc::channel(AUDIO_CHANNEL_SLOTS);
        let shutdown = Arc::new(AtomicBool::new(false));

        let worker_events = events_tx.clone();
        let loop_events = events_tx.clone();
        let worker_sink = sink.clone();
        let worker = thread::Builder::new()
            .name("remote-engine".into())
            .spawn({
                let shutdown = Arc::clone(&shutdown);
                move || {
                    run_worker(
                        &worker_events,
                        &done_tx,
                        AssertUnwindSafe(move || {
                            let mut sidecar = SidecarGuard(spawner.spawn(&cfg)?);
                            let runtime = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .context("create remote engine runtime")?;
                            runtime.block_on(connection_loop(
                                cfg,
                                async_rx,
                                loop_events,
                                worker_sink,
                                &mut sidecar,
                                shutdown,
                            ))
                        }),
                    );
                }
            })
            .context("spawn remote engine thread")?;

        let bridge = match thread::Builder::new().name("remote-bridge".into()).spawn({
            let shutdown = Arc::clone(&shutdown);
            move || bridge_commands(cmd_rx, async_tx, shutdown)
        }) {
            Ok(bridge) => bridge,
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                let _ = done_rx.recv_timeout(SHUTDOWN_TIMEOUT);
                let _ = worker.join();
                return Err(error).context("spawn remote command bridge");
            }
        };

        Ok(Self {
            sink,
            events_rx,
            events_tx,
            worker: Some(worker),
            bridge: Some(bridge),
            done_rx,
            shutdown,
        })
    }
}

impl Engine for RemoteEngine {
    fn sink(&self) -> AudioSink {
        self.sink.clone()
    }

    fn events(&mut self) -> &mut Receiver<EngineMsg> {
        &mut self.events_rx
    }

    fn set_waker(&mut self, waker: Option<Waker>) {
        self.events_tx.set_waker(waker);
    }

    fn shutdown(mut self: Box<Self>) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.sink.send(EngineCmd::Shutdown);
        join_workers(
            [
                (&mut self.bridge, "remote command bridge"),
                (&mut self.worker, "remote engine worker"),
            ],
            &self.done_rx,
            "remote engine",
        );
    }
}

fn bridge_commands(
    commands: Receiver<EngineCmd>,
    async_tx: tokio_mpsc::Sender<EngineCmd>,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match commands.recv_timeout(BRIDGE_POLL_INTERVAL) {
            Ok(command) => {
                let is_shutdown = matches!(command, EngineCmd::Shutdown);
                if async_tx.blocking_send(command).is_err() || is_shutdown {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

async fn connection_loop(
    cfg: RemoteConfig,
    mut commands: tokio_mpsc::Receiver<EngineCmd>,
    events: EventSender,
    sink: AudioSink,
    sidecar: &mut SidecarGuard,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let socket =
        connect_with_retry(&cfg.ws_url, Duration::from_millis(cfg.connect_timeout_ms)).await?;
    let (mut writer, mut reader) = socket.split();
    writer
        .send(Message::Text(hello_frame().into()))
        .await
        .context("send hayamimi hello frame")?;

    let mut active_gen = 0;
    let mut sidecar_poll = tokio::time::interval(SIDECAR_POLL_INTERVAL);
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(EngineCmd::Begin { gen }) => active_gen = gen,
                    Some(EngineCmd::Audio(samples)) => {
                        if let Err(error) = writer.send(Message::Binary(f32_to_s16le(&samples).into())).await {
                            send_disconnected(&events, format!("write failed: {error}"));
                            return Ok(());
                        }
                    }
                    Some(EngineCmd::End { gen: _, pad_ms }) => {
                        if let Err(error) = writer.send(Message::Binary(silence_s16le(pad_ms, SAMPLE_RATE).into())).await {
                            send_disconnected(&events, format!("write failed: {error}"));
                            return Ok(());
                        }
                    }
                    Some(EngineCmd::Cancel { gen }) => {
                        if gen == active_gen {
                            sink.take_dropped();
                        }
                    }
                    Some(EngineCmd::Shutdown) | None => {
                        let _ = writer.close().await;
                        return Ok(());
                    }
                }
            }
            message = reader.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match Event::parse(&text) {
                            Ok(Event::Ready { .. }) => {
                                if events.send(EngineMsg::Ready).is_err() {
                                    return Ok(());
                                }
                            }
                            Ok(Event::Final(final_event)) => {
                                let now = Instant::now();
                                if events.send(EngineMsg::Final {
                                    gen: active_gen,
                                    text: final_event.text,
                                    engine_latency_ms: final_event.engine_latency_ms,
                                    vad_close_at: now,
                                    asr_end_at: now,
                                    dropped_chunks: sink.take_dropped(),
                                }).is_err() {
                                    return Ok(());
                                }
                            }
                            Ok(Event::Error(reason)) => {
                                tracing::warn!(%reason, "remote engine error");
                            }
                            Ok(Event::Partial(_) | Event::Refine(_)) => {
                                tracing::trace!("dropping interim remote engine event");
                            }
                            Ok(Event::Other(kind)) => {
                                tracing::trace!(%kind, "ignoring remote engine event");
                            }
                            Err(error) => {
                                tracing::warn!(%error, frame = %text, "skipping invalid engine frame");
                            }
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        send_disconnected(&events, format!("server closed connection: {frame:?}"));
                        return Ok(());
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        send_disconnected(&events, format!("read failed: {error}"));
                        return Ok(());
                    }
                    None => {
                        send_disconnected(&events, "websocket stream ended".into());
                        return Ok(());
                    }
                }
            }
            _ = sidecar_poll.tick() => {
                if sidecar.0.as_mut().is_some_and(|process| !process.is_running()) {
                    send_disconnected(&events, "hayamimi sidecar exited".into());
                    return Ok(());
                }
                if shutdown.load(Ordering::Acquire) {
                    let _ = writer.close().await;
                    return Ok(());
                }
            }
        }
    }
}

async fn connect_with_retry(
    url: &str,
    connect_timeout: Duration,
) -> anyhow::Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let started = tokio::time::Instant::now();
    let mut last_error = None;
    loop {
        let remaining = connect_timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            anyhow::bail!(
                "timed out connecting to {url}: {}",
                last_error.unwrap_or_else(|| "no connection attempt completed".into())
            );
        }
        match tokio::time::timeout(remaining, tokio_tungstenite::connect_async(url)).await {
            Ok(Ok((socket, _))) => return Ok(socket),
            Ok(Err(error)) => last_error = Some(error.to_string()),
            Err(_) => anyhow::bail!("timed out connecting to {url}"),
        }
        let remaining = connect_timeout.saturating_sub(started.elapsed());
        if !remaining.is_zero() {
            tokio::time::sleep(RETRY_INTERVAL.min(remaining)).await;
        }
    }
}

fn send_disconnected(events: &EventSender, reason: String) {
    let _ = events.send(EngineMsg::Disconnected {
        reason,
        failed_model: None,
    });
}

struct SidecarGuard(Option<Box<dyn SidecarProcess>>);

impl Drop for SidecarGuard {
    fn drop(&mut self) {
        if let Some(process) = self.0.as_mut() {
            process.kill();
        }
    }
}
