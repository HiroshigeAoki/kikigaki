//! In-memory engine used by unit and downstream integration tests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use super::{channel, event_channel, AudioSink, Engine, EngineCmd, EngineMsg, EventSender, Waker};

/// Inspectable in-memory implementation of [`Engine`].
pub struct FakeEngine {
    sink: AudioSink,
    _commands: Mutex<Receiver<EngineCmd>>,
    events_tx: Mutex<Option<EventSender>>,
    events_rx: Receiver<EngineMsg>,
    shutdown_called: Arc<AtomicBool>,
}

impl FakeEngine {
    /// Creates a fake engine with open command and event channels.
    pub fn new() -> Self {
        let (sink, commands) = channel();
        let (events_tx, events_rx) = event_channel(16);
        Self {
            sink,
            _commands: Mutex::new(commands),
            events_tx: Mutex::new(Some(events_tx)),
            events_rx,
            shutdown_called: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Emits one event to the fake engine receiver.
    pub fn emit(&self, msg: EngineMsg) {
        self.events_tx
            .lock()
            .expect("fake event lock")
            .as_ref()
            .expect("fake events are open")
            .send(msg)
            .expect("fake event receiver is open");
    }

    /// Closes the fake engine's event channel.
    pub fn close_events(&mut self) {
        self.events_tx.lock().expect("fake event lock").take();
    }

    /// Returns the shared flag set by [`Engine::shutdown`].
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown_called)
    }
}

impl Default for FakeEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for FakeEngine {
    fn sink(&self) -> AudioSink {
        self.sink.clone()
    }

    fn events(&mut self) -> &mut Receiver<EngineMsg> {
        &mut self.events_rx
    }

    fn set_waker(&mut self, waker: Option<Waker>) {
        if let Some(events) = self.events_tx.lock().expect("fake event lock").as_ref() {
            events.set_waker(waker);
        }
    }

    fn shutdown(self: Box<Self>) {
        self.shutdown_called.store(true, Ordering::Relaxed);
        let _ = self.sink.send(EngineCmd::Shutdown);
    }
}
