#![cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]

use kikigaki_core::engine::AudioSink;
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Skip,
    Process { reset: bool, generation: u64 },
}

#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) active: AtomicBool,
    pub(crate) reset_pending: AtomicBool,
    pub(crate) first_callback: AtomicBool,
    pub(crate) play_started_at: Mutex<Option<Instant>>,
    generation: AtomicU64,
    sink: Mutex<Option<AudioSink>>,
    errored: AtomicBool,
}

impl Shared {
    pub(crate) fn arm(&self, sink: AudioSink) {
        {
            let mut slot = self.sink.lock().unwrap();
            self.generation.fetch_add(1, Ordering::SeqCst);
            *slot = Some(sink);
        }
        self.first_callback.store(false, Ordering::Relaxed);
        self.reset_pending.store(true, Ordering::SeqCst);
        *self.play_started_at.lock().unwrap() = Some(Instant::now());
        self.active.store(true, Ordering::SeqCst);
    }

    pub(crate) fn gate_buffer(&self) -> Gate {
        if !self.active.load(Ordering::SeqCst) {
            return Gate::Skip;
        }
        Gate::Process {
            reset: self.reset_pending.swap(false, Ordering::SeqCst),
            generation: self.generation.load(Ordering::SeqCst),
        }
    }

    pub(crate) fn lock_sink_for_generation(
        &self,
        generation: u64,
    ) -> Option<MutexGuard<'_, Option<AudioSink>>> {
        let sink = self.sink.lock().unwrap();
        if self.generation.load(Ordering::SeqCst) != generation {
            return None;
        }
        Some(sink)
    }

    pub(crate) fn clear_sink(&self) {
        self.sink.lock().unwrap().take();
    }

    pub(crate) fn record_stream_error(&self, is_xrun: bool, error: &dyn Display) {
        if is_xrun {
            tracing::warn!(%error, "microphone stream xrun");
            return;
        }
        self.errored.store(true, Ordering::SeqCst);
        tracing::error!(%error, "microphone stream error");
    }

    pub(crate) fn needs_rebuild(&self) -> bool {
        self.errored.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::{Gate, Shared};
    use std::sync::atomic::Ordering;

    #[test]
    fn callback_gate_skips_inactive_buffers() {
        let shared = Shared::default();

        assert_eq!(shared.gate_buffer(), Gate::Skip);
    }

    #[test]
    fn callback_gate_resets_once_at_the_start_of_each_press() {
        let shared = Shared::default();
        let (sink, _commands) = kikigaki_core::engine::channel();
        shared.arm(sink);

        let Gate::Process { reset, generation } = shared.gate_buffer() else {
            panic!("active gate skipped a buffer");
        };
        assert!(reset);
        assert!(shared.lock_sink_for_generation(generation).is_some());

        assert_eq!(
            shared.gate_buffer(),
            Gate::Process {
                reset: false,
                generation,
            }
        );

        shared.active.store(false, Ordering::SeqCst);
        shared.clear_sink();
        let (next_sink, _next_commands) = kikigaki_core::engine::channel();
        shared.arm(next_sink);
        assert!(matches!(
            shared.gate_buffer(),
            Gate::Process { reset: true, .. }
        ));
    }

    #[test]
    fn callback_gate_reports_non_xrun_errors_as_needing_rebuild() {
        let shared = Shared::default();
        shared.record_stream_error(true, &"buffer underrun or overrun");
        assert!(!shared.needs_rebuild());

        shared.record_stream_error(false, &"device unavailable");
        assert!(shared.needs_rebuild());
    }

    #[test]
    fn callback_gate_rejects_a_buffer_from_an_earlier_press_generation() {
        let shared = Shared::default();
        let (first_sink, _first_commands) = kikigaki_core::engine::channel();
        shared.arm(first_sink);
        let Gate::Process {
            generation: first_generation,
            ..
        } = shared.gate_buffer()
        else {
            panic!("active gate skipped a buffer");
        };

        shared.active.store(false, Ordering::SeqCst);
        let (second_sink, _second_commands) = kikigaki_core::engine::channel();
        shared.arm(second_sink);

        assert!(shared.lock_sink_for_generation(first_generation).is_none());
        let Gate::Process {
            generation: second_generation,
            ..
        } = shared.gate_buffer()
        else {
            panic!("active gate skipped a buffer");
        };
        assert_ne!(first_generation, second_generation);
        assert!(shared.lock_sink_for_generation(second_generation).is_some());
    }
}
