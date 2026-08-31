//! Engine-independent command transport and lifecycle supervision.

use std::any::Any;
use std::panic::UnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SendError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(test)]
/// Test engine implementation and inspection helpers.
pub mod fake;

/// Number of queued 20 ms chunks retained before microphone audio is dropped.
pub const AUDIO_CHANNEL_SLOTS: usize = 1_500;

/// Thread-safe callback used by background event producers to wake their consumer.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Engine event producer that wakes the supervisor's consumer after each successful send.
#[derive(Clone)]
pub struct EventSender {
    tx: SyncSender<EngineMsg>,
    waker: Arc<RwLock<Option<Waker>>>,
}

impl EventSender {
    /// Sends one engine event and invokes the configured waker after it is queued.
    pub fn send(&self, message: EngineMsg) -> Result<(), SendError<EngineMsg>> {
        self.tx.send(message)?;
        let waker = self.waker.read().expect("engine waker lock").clone();
        if let Some(waker) = waker {
            waker();
        }
        Ok(())
    }

    /// Replaces the callback invoked after successful event sends.
    pub fn set_waker(&self, waker: Option<Waker>) {
        *self.waker.write().expect("engine waker lock") = waker;
    }
}

/// Creates a bounded engine event channel whose producer supports wake callbacks.
pub fn event_channel(capacity: usize) -> (EventSender, Receiver<EngineMsg>) {
    let (tx, rx) = mpsc::sync_channel(capacity);
    (
        EventSender {
            tx,
            waker: Arc::new(RwLock::new(None)),
        },
        rx,
    )
}

const CONTROL_RETRY: Duration = Duration::from_millis(500);
const CONTROL_RETRY_INTERVAL: Duration = Duration::from_millis(5);
/// Maximum time to wait for a worker to report shutdown completion.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// An ordered command sent from the application and microphone to an engine worker.
#[derive(Debug, PartialEq)]
pub enum EngineCmd {
    /// Starts a new utterance generation.
    Begin {
        /// Session-assigned utterance generation.
        gen: u64,
    },
    /// A chunk of 16 kHz mono floating-point audio.
    ///
    /// Nothing downstream re-checks or converts the sample rate.
    Audio(Vec<f32>),
    /// Flushes and finalizes the active utterance.
    End {
        /// Session-assigned utterance generation.
        gen: u64,
        /// Silence appended before finalization, in milliseconds.
        pad_ms: u64,
    },
    /// Discards pending work for an utterance generation.
    Cancel {
        /// Session-assigned utterance generation.
        gen: u64,
    },
    /// Stops the engine worker.
    Shutdown,
}

/// An event emitted by an engine worker.
#[derive(Debug)]
pub enum EngineMsg {
    /// The engine is ready to accept an utterance.
    Ready,
    /// A completed transcription.
    Final {
        /// Session-assigned utterance generation.
        gen: u64,
        /// Transcribed text.
        text: String,
        /// Engine-reported processing latency in milliseconds, when available.
        engine_latency_ms: Option<f64>,
        /// Time at which voice activity detection closed.
        vad_close_at: Instant,
        /// Time at which speech recognition finished.
        asr_end_at: Instant,
        /// Audio chunks dropped for this generation.
        dropped_chunks: u64,
    },
    /// The engine stopped serving requests.
    Disconnected {
        /// Human-readable diagnostic reason.
        reason: String,
        /// Model package whose load failed, when the disconnect came from model initialization.
        failed_model: Option<&'static str>,
    },
}

/// Failure to enqueue an engine command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SinkError {
    /// The bounded queue remained full for the control-command retry window.
    #[error("engine command queue is full")]
    Full,
    /// The engine worker has closed its command receiver.
    #[error("engine command channel is closed")]
    Closed,
}

/// Cloneable producer for the single ordered engine command channel.
#[derive(Clone)]
pub struct AudioSink {
    tx: SyncSender<EngineCmd>,
    dropped: Arc<AtomicU64>,
    warned: Arc<AtomicBool>,
}

impl AudioSink {
    /// Enqueues a command while applying audio-drop and bounded control retry policy.
    ///
    /// Audio is attempted once and silently dropped on backpressure. Control commands retry
    /// every 5 ms for at most 500 ms. If that window expires, callers rely on the session's
    /// finalization timeout as the safety net.
    pub fn send(&self, cmd: EngineCmd) -> Result<(), SinkError> {
        if matches!(cmd, EngineCmd::Begin { .. }) {
            self.dropped.store(0, Ordering::Relaxed);
            self.warned.store(false, Ordering::Relaxed);
        }

        if matches!(cmd, EngineCmd::Audio(_)) {
            return match self.tx.try_send(cmd) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    if !self.warned.swap(true, Ordering::Relaxed) {
                        tracing::warn!("engine audio queue full; dropping chunks");
                    }
                    Ok(())
                }
                Err(TrySendError::Disconnected(_)) => Err(SinkError::Closed),
            };
        }

        let deadline = Instant::now() + CONTROL_RETRY;
        let mut pending = cmd;
        loop {
            match self.tx.try_send(pending) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => return Err(SinkError::Closed),
                Err(TrySendError::Full(cmd)) => pending = cmd,
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(SinkError::Full);
            }
            thread::sleep(CONTROL_RETRY_INTERVAL.min(deadline.saturating_duration_since(now)));
        }
    }

    /// Takes and resets the number of audio chunks dropped in the current generation.
    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

/// Creates the bounded ordered command channel shared by every engine implementation.
pub fn channel() -> (AudioSink, Receiver<EngineCmd>) {
    channel_with_capacity(AUDIO_CHANNEL_SLOTS)
}

fn channel_with_capacity(capacity: usize) -> (AudioSink, Receiver<EngineCmd>) {
    let (tx, rx) = mpsc::sync_channel(capacity);
    (
        AudioSink {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
            warned: Arc::new(AtomicBool::new(false)),
        },
        rx,
    )
}

/// A transcription engine controlled through an ordered command sink.
pub trait Engine: Send {
    /// Returns a cheap clone of the engine's command sink.
    fn sink(&self) -> AudioSink;
    /// Returns the event receiver drained by the application loop.
    fn events(&mut self) -> &mut Receiver<EngineMsg>;
    /// Configures the callback invoked after the engine queues an event.
    ///
    /// Implementations that cannot provide producer-side wakeups may keep the default no-op.
    fn set_waker(&mut self, _waker: Option<Waker>) {}
    /// Requests shutdown and waits at most two seconds for the worker.
    ///
    /// Implementations send `Shutdown` best-effort, wait on a done channel, and join only after
    /// done is observed. On timeout the thread handle is detached. This accepted trade-off keeps
    /// application shutdown bounded even when a worker is stuck in foreign or network code.
    fn shutdown(self: Box<Self>);
}

/// Factory used by the supervisor to construct fresh engine instances.
pub type EngineFactory = Box<dyn Fn() -> anyhow::Result<Box<dyn Engine>> + Send>;

/// Current startup/readiness state of the supervised engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePhase {
    /// The engine is being installed, connected, or initialized.
    Starting,
    /// The engine has emitted `Ready`.
    Ready,
    /// The engine failed or disconnected.
    Failed,
}

/// Owns the active engine and restarts it after failure.
pub struct EngineSupervisor {
    factory: EngineFactory,
    current: Option<Box<dyn Engine>>,
    phase: EnginePhase,
    disconnected_seen: bool,
    waker: Option<Waker>,
}

impl EngineSupervisor {
    /// Creates a supervisor in the startup phase without constructing an engine yet.
    pub fn new(factory: EngineFactory) -> Self {
        Self {
            factory,
            current: None,
            phase: EnginePhase::Starting,
            disconnected_seen: false,
            waker: None,
        }
    }

    /// Configures a callback that active engines invoke after queuing each event.
    pub fn with_waker(mut self, waker: Waker) -> Self {
        self.waker = Some(waker);
        self
    }

    /// Constructs the initial engine and leaves it in the startup phase until `Ready` arrives.
    pub fn start(&mut self) -> anyhow::Result<()> {
        if let Some(engine) = self.current.take() {
            engine.shutdown();
        }
        self.phase = EnginePhase::Starting;
        self.disconnected_seen = false;
        match (self.factory)() {
            Ok(mut engine) => {
                engine.set_waker(self.waker.clone());
                self.current = Some(engine);
                Ok(())
            }
            Err(error) => {
                self.phase = EnginePhase::Failed;
                Err(error)
            }
        }
    }

    /// Restarts a ready or failed engine, or returns `false` while startup is still in progress.
    pub fn restart(&mut self) -> anyhow::Result<bool> {
        if self.phase == EnginePhase::Starting {
            tracing::info!("engine still starting");
            return Ok(false);
        }
        if let Some(engine) = self.current.take() {
            engine.shutdown();
        }
        self.start()?;
        Ok(true)
    }

    /// Shuts the current engine down (bounded) and marks the supervisor failed.
    ///
    /// Used when a dependency that must accompany the engine (for example the post-processing
    /// pipeline) fails after `start` already spawned the engine worker.
    pub fn shutdown(&mut self) {
        if let Some(engine) = self.current.take() {
            engine.shutdown();
        }
        self.phase = EnginePhase::Failed;
    }

    /// Returns a command sink when an engine currently exists.
    pub fn sink(&self) -> Option<AudioSink> {
        self.current.as_ref().map(|engine| engine.sink())
    }

    /// Returns the current engine phase.
    pub fn phase(&self) -> EnginePhase {
        self.phase
    }

    /// Attempts to receive one event and updates the engine phase.
    pub fn try_recv(&mut self) -> Option<EngineMsg> {
        let result = self.current.as_mut()?.events().try_recv();
        match result {
            Ok(EngineMsg::Ready) => {
                self.phase = EnginePhase::Ready;
                Some(EngineMsg::Ready)
            }
            Ok(message @ EngineMsg::Final { .. }) => Some(message),
            Ok(message @ EngineMsg::Disconnected { .. }) => {
                self.phase = EnginePhase::Failed;
                self.disconnected_seen = true;
                Some(message)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                if let Some(engine) = self.current.take() {
                    engine.shutdown();
                }
                self.phase = EnginePhase::Failed;
                if self.disconnected_seen {
                    None
                } else {
                    self.disconnected_seen = true;
                    Some(EngineMsg::Disconnected {
                        reason: "engine events channel closed".into(),
                        failed_model: None,
                    })
                }
            }
        }
    }
}

impl Drop for EngineSupervisor {
    fn drop(&mut self) {
        if let Some(engine) = self.current.take() {
            engine.shutdown();
        }
    }
}

/// Runs a worker body with panic reporting and an unconditional completion signal.
pub fn run_worker<F>(events: &EventSender, done_tx: &SyncSender<()>, body: F)
where
    F: FnOnce() -> anyhow::Result<()> + UnwindSafe,
{
    let failure = match std::panic::catch_unwind(body) {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!("{error:#}")),
        Err(payload) => Some(panic_reason(payload.as_ref())),
    };
    if let Some(reason) = failure {
        let _ = events.send(EngineMsg::Disconnected {
            reason,
            failed_model: None,
        });
    }
    let _ = done_tx.send(());
}

pub(crate) fn panic_reason(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|reason| (*reason).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".into())
}

/// Waits for worker completion, then joins completed threads or detaches them on timeout.
pub fn join_workers<const N: usize>(
    handles: [(&mut Option<JoinHandle<()>>, &str); N],
    done_rx: &Receiver<()>,
    name: &str,
) {
    match done_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            for (handle, thread_name) in handles {
                if handle.take().is_some_and(|handle| handle.join().is_err()) {
                    tracing::warn!(thread = thread_name, "worker panicked after completion");
                }
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::error!(
                worker = name,
                "worker did not stop within 2 s; detaching thread"
            );
            for (handle, _) in handles {
                handle.take();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::engine::fake::FakeEngine;

    #[test]
    fn audio_on_full_drops_counts_and_warns_once() {
        let (sink, _rx) = channel_with_capacity(1);
        sink.send(EngineCmd::Begin { gen: 1 }).unwrap();

        assert_eq!(sink.send(EngineCmd::Audio(vec![0.1])), Ok(()));
        assert_eq!(sink.send(EngineCmd::Audio(vec![0.2])), Ok(()));
        assert_eq!(sink.dropped.load(Ordering::Relaxed), 2);
        assert!(sink.warned.load(Ordering::Relaxed));
        assert_eq!(sink.take_dropped(), 2);
        assert_eq!(sink.take_dropped(), 0);
    }

    #[test]
    fn control_on_full_times_out_or_succeeds_when_space_opens() {
        let (sink, _rx) = channel_with_capacity(1);
        sink.send(EngineCmd::Audio(vec![0.0])).unwrap();
        let started = Instant::now();
        assert_eq!(
            sink.send(EngineCmd::End {
                gen: 1,
                pad_ms: 500,
            }),
            Err(SinkError::Full)
        );
        assert!(started.elapsed() >= Duration::from_millis(500));

        let (sink, rx) = channel_with_capacity(1);
        sink.send(EngineCmd::Audio(vec![0.0])).unwrap();
        let drain = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let audio = rx.recv().unwrap();
            let end = rx.recv().unwrap();
            (audio, end)
        });
        assert_eq!(
            sink.send(EngineCmd::End {
                gen: 1,
                pad_ms: 500,
            }),
            Ok(())
        );
        let (audio, end) = drain.join().unwrap();
        assert!(matches!(audio, EngineCmd::Audio(_)));
        assert!(matches!(end, EngineCmd::End { .. }));
    }

    #[test]
    fn end_remains_ordered_after_audio() {
        let (sink, rx) = channel_with_capacity(11);
        for index in 0..10 {
            sink.send(EngineCmd::Audio(vec![index as f32])).unwrap();
        }
        sink.send(EngineCmd::End {
            gen: 7,
            pad_ms: 500,
        })
        .unwrap();

        let received: Vec<_> = rx.try_iter().collect();
        assert_eq!(received.len(), 11);
        assert_eq!(
            received.last(),
            Some(&EngineCmd::End {
                gen: 7,
                pad_ms: 500,
            })
        );
    }

    #[test]
    fn panicking_worker_reports_once_and_always_signals_done() {
        let (events_tx, events_rx) = event_channel(4);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        run_worker(&events_tx, &done_tx, || -> anyhow::Result<()> {
            panic!("decoder exploded")
        });

        assert!(matches!(
            events_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            EngineMsg::Disconnected {
                reason,
                failed_model: None,
            } if reason.contains("decoder exploded")
        ));
        assert!(events_rx.try_recv().is_err());
        done_rx.recv_timeout(Duration::from_millis(100)).unwrap();
    }

    #[test]
    fn supervisor_waker_runs_after_an_engine_event_is_sent() {
        struct TestEngine {
            sink: AudioSink,
            events_tx: EventSender,
            events_rx: Receiver<EngineMsg>,
        }

        impl Engine for TestEngine {
            fn sink(&self) -> AudioSink {
                self.sink.clone()
            }

            fn events(&mut self) -> &mut Receiver<EngineMsg> {
                &mut self.events_rx
            }

            fn set_waker(&mut self, waker: Option<Waker>) {
                self.events_tx.set_waker(waker);
            }

            fn shutdown(self: Box<Self>) {}
        }

        let (events_tx, events_rx) = event_channel(4);
        let factory_tx = events_tx.clone();
        let receiver = Arc::new(std::sync::Mutex::new(Some(events_rx)));
        let factory_receiver = Arc::clone(&receiver);
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let mut supervisor = EngineSupervisor::new(Box::new(move || {
            let (sink, _commands) = channel();
            Ok(Box::new(TestEngine {
                sink,
                events_tx: factory_tx.clone(),
                events_rx: factory_receiver.lock().unwrap().take().unwrap(),
            }))
        }))
        .with_waker(Arc::new(move || {
            wake_count.fetch_add(1, Ordering::Relaxed);
        }));
        supervisor.start().unwrap();

        events_tx.send(EngineMsg::Ready).unwrap();

        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(matches!(supervisor.try_recv(), Some(EngineMsg::Ready)));
    }

    #[test]
    fn closed_events_are_reported_once_and_restart_builds_fresh_engine() {
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::clone(&calls);
        let mut supervisor = EngineSupervisor::new(Box::new(move || {
            factory_calls.fetch_add(1, Ordering::Relaxed);
            let mut fake = FakeEngine::new();
            fake.close_events();
            Ok(Box::new(fake))
        }));
        supervisor.start().unwrap();

        assert!(matches!(
            supervisor.try_recv(),
            Some(EngineMsg::Disconnected {
                reason,
                failed_model: None,
            }) if reason == "engine events channel closed"
        ));
        assert!(supervisor.try_recv().is_none());
        assert!(supervisor.restart().unwrap());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn supervisor_phase_follows_ready_and_disconnected() {
        let mut ready = EngineSupervisor::new(Box::new(|| {
            let fake = FakeEngine::new();
            fake.emit(EngineMsg::Ready);
            Ok(Box::new(fake))
        }));
        ready.start().unwrap();
        assert_eq!(ready.phase(), EnginePhase::Starting);
        assert!(matches!(ready.try_recv(), Some(EngineMsg::Ready)));
        assert_eq!(ready.phase(), EnginePhase::Ready);

        let mut failed = EngineSupervisor::new(Box::new(|| {
            let fake = FakeEngine::new();
            fake.emit(EngineMsg::Disconnected {
                reason: "gone".into(),
                failed_model: None,
            });
            Ok(Box::new(fake))
        }));
        failed.start().unwrap();
        assert!(matches!(
            failed.try_recv(),
            Some(EngineMsg::Disconnected { .. })
        ));
        assert_eq!(failed.phase(), EnginePhase::Failed);
    }

    #[test]
    fn restart_while_starting_is_a_noop() {
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::clone(&calls);
        let mut supervisor = EngineSupervisor::new(Box::new(move || {
            factory_calls.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(FakeEngine::new()))
        }));
        supervisor.start().unwrap();

        assert!(!supervisor.restart().unwrap());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dropping_supervisor_shuts_down_current_engine() {
        let fake = FakeEngine::new();
        let shutdown = fake.shutdown_flag();
        let engine = Arc::new(std::sync::Mutex::new(Some(fake)));
        let factory_engine = Arc::clone(&engine);
        let mut supervisor = EngineSupervisor::new(Box::new(move || {
            Ok(Box::new(
                factory_engine.lock().unwrap().take().expect("one start"),
            ))
        }));
        supervisor.start().unwrap();
        drop(supervisor);

        assert!(shutdown.load(Ordering::Relaxed));
    }
}
