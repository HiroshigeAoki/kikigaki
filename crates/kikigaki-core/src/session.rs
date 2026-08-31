//! Push-to-talk session state and effects.
//!
//! Every press receives a monotonically increasing generation. Finals from older generations are
//! dropped, including after a re-press. The remote engine has a weaker compatibility policy because
//! hayamimi does not carry generation IDs and therefore attributes a late final to its newest begin.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::metrics::Utterance;
use crate::protocol::{Event, Final};

/// An external event delivered to the session state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// The push-to-talk hotkey was pressed.
    Pressed,
    /// The push-to-talk hotkey was released.
    Released,
    /// The engine emitted a protocol event.
    Engine(Event),
    /// The engine WebSocket disconnected.
    EngineDisconnected,
    /// A periodic timeout check.
    Tick,
}

/// An effect for the application to execute.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Begin a new engine utterance generation.
    Begin {
        /// Session-assigned utterance generation.
        gen: u64,
    },
    /// Begin microphone capture.
    StartMic,
    /// End microphone capture.
    StopMic,
    /// End the active engine utterance after appending silence.
    EndUtterance {
        /// Session-assigned utterance generation.
        gen: u64,
        /// Silence duration in milliseconds.
        pad_ms: u64,
    },
    /// Cancel pending work for an earlier generation.
    Cancel {
        /// Session-assigned utterance generation.
        gen: u64,
    },
    /// Paste transcription text exactly as supplied by the application pipeline.
    Paste(String),
    /// Persist an utterance metrics row.
    Record(Utterance),
    /// Publish a session state change.
    SetState(State),
    /// Reconnect to the transcription engine.
    Reconnect,
}

/// Current push-to-talk session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Connected and waiting for a hotkey press.
    Idle,
    /// Capturing and streaming microphone audio.
    Recording,
    /// Waiting for the engine to finalize buffered audio.
    Finalizing,
    /// Not connected to the engine.
    Disconnected,
}

/// Timing settings for a session.
pub struct SessionConfig {
    /// Silence appended on release, in milliseconds.
    pub silence_pad_ms: u64,
    /// Maximum time to wait for final transcription.
    pub final_timeout: Duration,
}

struct PendingRow {
    row: Utterance,
    t_final: Instant,
}

/// Pure push-to-talk state machine driven by caller-provided timestamps.
pub struct Session {
    state: State,
    t_press: Option<Instant>,
    t_release: Option<Instant>,
    pending: VecDeque<PendingRow>,
    gen: u64,
    cfg: SessionConfig,
}

impl Session {
    /// Creates a disconnected session awaiting engine readiness.
    pub fn new(cfg: SessionConfig) -> Self {
        Self {
            state: State::Disconnected,
            t_press: None,
            t_release: None,
            pending: VecDeque::new(),
            gen: 0,
            cfg,
        }
    }

    /// Returns the current state.
    pub fn state(&self) -> State {
        self.state
    }

    /// Applies an input at the supplied timestamp and returns requested effects.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Output> {
        match (self.state, input) {
            (State::Idle, Input::Pressed) => {
                self.gen = self.gen.wrapping_add(1);
                self.t_press = Some(now);
                self.t_release = None;
                self.state = State::Recording;
                vec![
                    Output::Begin { gen: self.gen },
                    Output::StartMic,
                    Output::SetState(State::Recording),
                ]
            }
            (State::Recording, Input::Released) => {
                self.t_release = Some(now);
                self.state = State::Finalizing;
                vec![
                    Output::StopMic,
                    Output::EndUtterance {
                        gen: self.gen,
                        pad_ms: self.cfg.silence_pad_ms,
                    },
                    Output::SetState(State::Finalizing),
                ]
            }
            (State::Finalizing, Input::Pressed) => {
                let old_gen = self.gen;
                self.gen = self.gen.wrapping_add(1);
                self.t_press = Some(now);
                self.t_release = None;
                self.state = State::Recording;
                vec![
                    Output::Cancel { gen: old_gen },
                    Output::Begin { gen: self.gen },
                    Output::StartMic,
                    Output::SetState(State::Recording),
                ]
            }
            (State::Recording, Input::Engine(Event::Final(final_event))) => {
                self.handle_final(final_event, now, false)
            }
            (State::Finalizing, Input::Engine(Event::Final(final_event))) => {
                self.handle_final(final_event, now, true)
            }
            (State::Finalizing, Input::Tick)
                if self.t_release.is_some_and(|release| {
                    now.checked_duration_since(release).unwrap_or_default()
                        >= self.cfg.final_timeout
                }) =>
            {
                self.finish_timeout()
            }
            (State::Finalizing, Input::Engine(Event::Error(_))) => self.finish_timeout(),
            (State::Recording, Input::EngineDisconnected) => {
                self.state = State::Disconnected;
                self.t_press = None;
                self.t_release = None;
                vec![Output::StopMic, Output::SetState(State::Disconnected)]
            }
            (State::Idle | State::Finalizing, Input::EngineDisconnected) => {
                self.state = State::Disconnected;
                self.t_press = None;
                self.t_release = None;
                vec![Output::SetState(State::Disconnected)]
            }
            (State::Disconnected, Input::Pressed) => vec![Output::Reconnect],
            (State::Disconnected, Input::Engine(Event::Ready { .. })) => {
                self.state = State::Idle;
                vec![Output::SetState(State::Idle)]
            }
            (State::Idle, Input::Engine(Event::Final(final_event))) => {
                tracing::debug!(
                    received = final_event.gen,
                    current = self.gen,
                    "dropping final while idle"
                );
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Completes the pending metrics row after a successful paste.
    pub fn pasted(&mut self, now: Instant) -> Vec<Output> {
        let Some(mut pending) = self.pending.pop_front() else {
            return Vec::new();
        };
        pending.row.final_to_paste_ms = Some(elapsed_ms(pending.t_final, now));
        vec![Output::Record(pending.row)]
    }

    /// Completes the pending metrics row after a failed paste.
    pub fn paste_failed(&mut self, _now: Instant) -> Vec<Output> {
        let Some(mut pending) = self.pending.pop_front() else {
            return Vec::new();
        };
        pending.row.paste_failed = true;
        vec![Output::Record(pending.row)]
    }

    fn handle_final(&mut self, final_event: Final, now: Instant, to_idle: bool) -> Vec<Output> {
        if final_event.gen != self.gen {
            tracing::debug!(
                received = final_event.gen,
                current = self.gen,
                "dropping stale final"
            );
            return Vec::new();
        }
        let text = final_event.text.clone();
        let row = self.final_row(&final_event, &text, now);
        let mut outputs = if text.trim().is_empty() {
            vec![Output::Record(row)]
        } else {
            self.pending.push_back(PendingRow { row, t_final: now });
            vec![Output::Paste(text)]
        };
        if to_idle {
            self.state = State::Idle;
            self.t_press = None;
            self.t_release = None;
            outputs.push(Output::SetState(State::Idle));
        }
        outputs
    }

    fn final_row(&self, final_event: &Final, text: &str, now: Instant) -> Utterance {
        Utterance {
            ts: String::new(),
            audio_ms: match (self.t_press, self.t_release) {
                (Some(press), Some(release)) => elapsed_ms(press, release),
                (Some(press), None) => elapsed_ms(press, now),
                _ => 0,
            },
            since_press_ms: if self.state == State::Recording {
                self.t_press.map(|press| elapsed_ms(press, now))
            } else {
                None
            },
            release_to_final_ms: self.t_release.map(|release| elapsed_ms(release, now)),
            final_to_paste_ms: None,
            engine_latency_ms: final_event.engine_latency_ms,
            vad_close_to_asr_end_ms: final_event.vad_close_to_asr_end_ms,
            postprocess_ms: final_event.postprocess_ms,
            dropped_chunks: final_event.dropped_chunks,
            gen: final_event.gen,
            chars: text.chars().count(),
            lang: final_event.lang.clone(),
            timeout: false,
            paste_failed: false,
        }
    }

    fn finish_timeout(&mut self) -> Vec<Output> {
        let row = Utterance {
            ts: String::new(),
            audio_ms: match (self.t_press, self.t_release) {
                (Some(press), Some(release)) => elapsed_ms(press, release),
                _ => 0,
            },
            since_press_ms: None,
            release_to_final_ms: None,
            final_to_paste_ms: None,
            engine_latency_ms: None,
            vad_close_to_asr_end_ms: None,
            postprocess_ms: None,
            dropped_chunks: 0,
            gen: self.gen,
            chars: 0,
            lang: String::new(),
            timeout: true,
            paste_failed: false,
        };
        self.state = State::Idle;
        self.t_press = None;
        self.t_release = None;
        vec![Output::Record(row), Output::SetState(State::Idle)]
    }
}

fn elapsed_ms(start: Instant, end: Instant) -> u64 {
    end.checked_duration_since(start)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Final;

    fn session() -> Session {
        let mut session = Session::new(SessionConfig {
            silence_pad_ms: 500,
            final_timeout: Duration::from_millis(3_000),
        });
        session.handle(Input::Engine(Event::Ready { sr: 16_000 }), Instant::now());
        session
    }

    fn final_event(text: &str) -> Event {
        final_event_for(1, text)
    }

    fn final_event_for(gen: u64, text: &str) -> Event {
        Event::Final(Final {
            gen,
            text: text.into(),
            lang: "ja".into(),
            engine_latency_ms: Some(98.4),
            dropped_chunks: 2,
            vad_close_to_asr_end_ms: Some(7),
            postprocess_ms: Some(3),
        })
    }

    #[test]
    fn idle_pressed_starts_mic_and_enters_recording() {
        let mut session = session();
        let now = Instant::now();
        assert_eq!(
            session.handle(Input::Pressed, now),
            vec![
                Output::Begin { gen: 1 },
                Output::StartMic,
                Output::SetState(State::Recording),
            ]
        );
        assert_eq!(session.state(), State::Recording);
    }

    #[test]
    fn pressed_while_recording_is_ignored() {
        let mut session = session();
        let now = Instant::now();
        session.handle(Input::Pressed, now);
        assert!(session
            .handle(Input::Pressed, now + Duration::from_millis(1))
            .is_empty());
    }

    #[test]
    fn released_ends_utterance_with_pad_and_enters_finalizing() {
        let mut session = session();
        let now = Instant::now();
        session.handle(Input::Pressed, now);
        assert_eq!(
            session.handle(Input::Released, now + Duration::from_secs(1)),
            vec![
                Output::StopMic,
                Output::EndUtterance {
                    gen: 1,
                    pad_ms: 500,
                },
                Output::SetState(State::Finalizing),
            ]
        );
        assert_eq!(session.state(), State::Finalizing);
    }

    #[test]
    fn released_while_idle_is_ignored() {
        let mut session = session();
        assert!(session.handle(Input::Released, Instant::now()).is_empty());
    }

    #[test]
    fn final_in_finalizing_pastes_text_verbatim() {
        let mut session = session();
        let pressed = Instant::now();
        let released = pressed + Duration::from_millis(1_000);
        let finalized = released + Duration::from_millis(120);
        session.handle(Input::Pressed, pressed);
        session.handle(Input::Released, released);

        assert_eq!(
            session.handle(Input::Engine(final_event("こんにちは。")), finalized),
            vec![
                Output::Paste("こんにちは。".into()),
                Output::SetState(State::Idle)
            ]
        );
        assert_eq!(session.state(), State::Idle);
        assert_eq!(
            session.pasted(finalized + Duration::from_millis(14)),
            vec![Output::Record(Utterance {
                ts: String::new(),
                audio_ms: 1_000,
                since_press_ms: None,
                release_to_final_ms: Some(120),
                final_to_paste_ms: Some(14),
                engine_latency_ms: Some(98.4),
                vad_close_to_asr_end_ms: Some(7),
                postprocess_ms: Some(3),
                dropped_chunks: 2,
                gen: 1,
                chars: 6,
                lang: "ja".into(),
                timeout: false,
                paste_failed: false,
            })]
        );
    }

    #[test]
    fn empty_final_records_but_does_not_paste() {
        let mut session = session();
        let pressed = Instant::now();
        let released = pressed + Duration::from_millis(500);
        let finalized = released + Duration::from_millis(40);
        session.handle(Input::Pressed, pressed);
        session.handle(Input::Released, released);

        assert_eq!(
            session.handle(Input::Engine(final_event("")), finalized),
            vec![
                Output::Record(Utterance {
                    ts: String::new(),
                    audio_ms: 500,
                    since_press_ms: None,
                    release_to_final_ms: Some(40),
                    final_to_paste_ms: None,
                    engine_latency_ms: Some(98.4),
                    vad_close_to_asr_end_ms: Some(7),
                    postprocess_ms: Some(3),
                    dropped_chunks: 2,
                    gen: 1,
                    chars: 0,
                    lang: "ja".into(),
                    timeout: false,
                    paste_failed: false,
                }),
                Output::SetState(State::Idle),
            ]
        );
        assert!(session.pasted(finalized).is_empty());
    }

    #[test]
    fn partial_and_refine_are_ignored() {
        let now = Instant::now();
        for state in [
            State::Idle,
            State::Recording,
            State::Finalizing,
            State::Disconnected,
        ] {
            let mut session = session();
            session.state = state;
            assert!(session
                .handle(Input::Engine(Event::Partial("x".into())), now)
                .is_empty());
            assert!(session
                .handle(Input::Engine(Event::Refine("x".into())), now)
                .is_empty());
            assert_eq!(session.state(), state);
        }
    }

    #[test]
    fn final_while_recording_is_pasted_too() {
        let mut session = session();
        let pressed = Instant::now();
        let finalized = pressed + Duration::from_millis(750);
        session.handle(Input::Pressed, pressed);

        assert_eq!(
            session.handle(Input::Engine(final_event("一文目")), finalized),
            vec![Output::Paste("一文目".into())]
        );
        assert_eq!(session.state(), State::Recording);
        assert_eq!(
            session.pasted(finalized + Duration::from_millis(10)),
            vec![Output::Record(Utterance {
                ts: String::new(),
                audio_ms: 750,
                since_press_ms: Some(750),
                release_to_final_ms: None,
                final_to_paste_ms: Some(10),
                engine_latency_ms: Some(98.4),
                vad_close_to_asr_end_ms: Some(7),
                postprocess_ms: Some(3),
                dropped_chunks: 2,
                gen: 1,
                chars: 3,
                lang: "ja".into(),
                timeout: false,
                paste_failed: false,
            })]
        );
    }

    #[test]
    fn two_finals_while_recording_then_two_pasted_calls_record_both_rows() {
        let mut session = session();
        let pressed = Instant::now();
        let first_final = pressed + Duration::from_millis(100);
        let second_final = pressed + Duration::from_millis(250);
        session.handle(Input::Pressed, pressed);

        assert_eq!(
            session.handle(Input::Engine(final_event("一")), first_final),
            vec![Output::Paste("一".into())]
        );
        assert_eq!(
            session.handle(Input::Engine(final_event("二文")), second_final),
            vec![Output::Paste("二文".into())]
        );

        assert!(matches!(
            session.pasted(pressed + Duration::from_millis(400)).as_slice(),
            [Output::Record(row)]
                if row.chars == 1 && row.final_to_paste_ms == Some(300)
        ));
        assert!(matches!(
            session.pasted(pressed + Duration::from_millis(500)).as_slice(),
            [Output::Record(row)]
                if row.chars == 2 && row.final_to_paste_ms == Some(250)
        ));
    }

    #[test]
    fn final_in_recording_then_final_in_finalizing_keeps_rows_in_order() {
        let mut session = session();
        let pressed = Instant::now();
        let first_final = pressed + Duration::from_millis(100);
        let released = pressed + Duration::from_millis(200);
        let second_final = pressed + Duration::from_millis(300);
        session.handle(Input::Pressed, pressed);
        session.handle(Input::Engine(final_event("一")), first_final);
        session.handle(Input::Released, released);
        session.handle(Input::Engine(final_event("二文")), second_final);

        assert!(matches!(
            session.pasted(pressed + Duration::from_millis(400)).as_slice(),
            [Output::Record(row)]
                if row.chars == 1 && row.final_to_paste_ms == Some(300)
        ));
        assert!(matches!(
            session.pasted(pressed + Duration::from_millis(500)).as_slice(),
            [Output::Record(row)]
                if row.chars == 2 && row.final_to_paste_ms == Some(200)
        ));
    }

    #[test]
    fn paste_failed_retires_front_row_with_paste_failed_flag() {
        let mut session = session();
        let pressed = Instant::now();
        let finalized = pressed + Duration::from_millis(100);
        session.handle(Input::Pressed, pressed);
        session.handle(Input::Engine(final_event("失敗")), finalized);

        assert!(matches!(
            session
                .paste_failed(finalized + Duration::from_millis(20))
                .as_slice(),
            [Output::Record(row)]
                if row.chars == 2
                    && row.final_to_paste_ms.is_none()
                    && row.paste_failed
        ));
        assert!(session
            .pasted(finalized + Duration::from_millis(30))
            .is_empty());
    }

    #[test]
    fn paste_then_paste_failed_keeps_fifo_order() {
        let mut session = session();
        let pressed = Instant::now();
        let first_final = pressed + Duration::from_millis(100);
        let second_final = pressed + Duration::from_millis(200);
        session.handle(Input::Pressed, pressed);
        session.handle(Input::Engine(final_event("一")), first_final);
        session.handle(Input::Engine(final_event("二文")), second_final);

        assert!(matches!(
            session.pasted(pressed + Duration::from_millis(300)).as_slice(),
            [Output::Record(row)]
                if row.chars == 1
                    && row.final_to_paste_ms == Some(200)
                    && !row.paste_failed
        ));
        assert!(matches!(
            session
                .paste_failed(pressed + Duration::from_millis(400))
                .as_slice(),
            [Output::Record(row)]
                if row.chars == 2
                    && row.final_to_paste_ms.is_none()
                    && row.paste_failed
        ));
    }

    #[test]
    fn pressed_in_finalizing_starts_new_recording_without_timeout_row() {
        let mut session = session();
        let pressed = Instant::now();
        let released = pressed + Duration::from_millis(500);
        let repressed = released + Duration::from_millis(100);
        session.handle(Input::Pressed, pressed);
        session.handle(Input::Released, released);

        assert_eq!(
            session.handle(Input::Pressed, repressed),
            vec![
                Output::Cancel { gen: 1 },
                Output::Begin { gen: 2 },
                Output::StartMic,
                Output::SetState(State::Recording),
            ]
        );
        assert_eq!(session.state(), State::Recording);
        assert!(session
            .handle(Input::Tick, released + Duration::from_millis(3_001))
            .is_empty());
    }

    #[test]
    fn late_final_after_repress_is_pasted() {
        let mut session = session();
        let pressed = Instant::now();
        let released = pressed + Duration::from_millis(500);
        let repressed = released + Duration::from_millis(100);
        let finalized = repressed + Duration::from_millis(80);
        session.handle(Input::Pressed, pressed);
        session.handle(Input::Released, released);
        session.handle(Input::Pressed, repressed);

        assert_eq!(
            session.handle(Input::Engine(final_event_for(2, "遅延")), finalized),
            vec![Output::Paste("遅延".into())]
        );
        assert_eq!(session.state(), State::Recording);
        assert!(matches!(
            session.pasted(finalized + Duration::from_millis(10)).as_slice(),
            [Output::Record(row)]
                if row.audio_ms == 80
                    && row.since_press_ms == Some(80)
                    && row.release_to_final_ms.is_none()
        ));
    }

    #[test]
    fn tick_after_timeout_in_finalizing_returns_to_idle_with_timeout_row() {
        let mut session = session();
        let pressed = Instant::now();
        let released = pressed + Duration::from_millis(1_000);
        session.handle(Input::Pressed, pressed);
        session.handle(Input::Released, released);

        assert!(session
            .handle(Input::Tick, released + Duration::from_millis(2_999))
            .is_empty());
        assert_eq!(
            session.handle(Input::Tick, released + Duration::from_millis(3_001)),
            vec![
                Output::Record(Utterance {
                    ts: String::new(),
                    audio_ms: 1_000,
                    since_press_ms: None,
                    release_to_final_ms: None,
                    final_to_paste_ms: None,
                    engine_latency_ms: None,
                    vad_close_to_asr_end_ms: None,
                    postprocess_ms: None,
                    dropped_chunks: 0,
                    gen: 1,
                    chars: 0,
                    lang: String::new(),
                    timeout: true,
                    paste_failed: false,
                }),
                Output::SetState(State::Idle),
            ]
        );
        assert_eq!(session.state(), State::Idle);
    }

    #[test]
    fn error_event_records_timeout_row_and_returns_idle() {
        let mut session = session();
        let pressed = Instant::now();
        let released = pressed + Duration::from_millis(800);
        session.handle(Input::Pressed, pressed);
        session.handle(Input::Released, released);

        assert_eq!(
            session.handle(
                Input::Engine(Event::Error("busy".into())),
                released + Duration::from_millis(20),
            ),
            vec![
                Output::Record(Utterance {
                    ts: String::new(),
                    audio_ms: 800,
                    since_press_ms: None,
                    release_to_final_ms: None,
                    final_to_paste_ms: None,
                    engine_latency_ms: None,
                    vad_close_to_asr_end_ms: None,
                    postprocess_ms: None,
                    dropped_chunks: 0,
                    gen: 1,
                    chars: 0,
                    lang: String::new(),
                    timeout: true,
                    paste_failed: false,
                }),
                Output::SetState(State::Idle),
            ]
        );
        assert_eq!(session.state(), State::Idle);
    }

    #[test]
    fn engine_disconnected_goes_to_disconnected_and_next_press_reconnects() {
        let now = Instant::now();
        let mut idle = session();
        assert_eq!(
            idle.handle(Input::EngineDisconnected, now),
            vec![Output::SetState(State::Disconnected)]
        );
        assert_eq!(idle.state(), State::Disconnected);
        assert_eq!(
            idle.handle(Input::Pressed, now + Duration::from_millis(1)),
            vec![Output::Reconnect]
        );
        assert_eq!(idle.state(), State::Disconnected);
        assert_eq!(
            idle.handle(
                Input::Engine(Event::Ready { sr: 16_000 }),
                now + Duration::from_millis(2),
            ),
            vec![Output::SetState(State::Idle)]
        );
        assert_eq!(idle.state(), State::Idle);

        let mut recording = session();
        recording.handle(Input::Pressed, now);
        assert_eq!(
            recording.handle(Input::EngineDisconnected, now),
            vec![Output::StopMic, Output::SetState(State::Disconnected)]
        );
        assert_eq!(recording.state(), State::Disconnected);
    }

    #[test]
    fn ready_in_idle_is_noop() {
        let mut session = session();
        assert!(session
            .handle(Input::Engine(Event::Ready { sr: 16_000 }), Instant::now())
            .is_empty());
        assert_eq!(session.state(), State::Idle);
    }

    #[test]
    fn stale_gen_final_is_dropped() {
        let mut session = session();
        let now = Instant::now();
        session.handle(Input::Pressed, now);
        session.handle(Input::Released, now + Duration::from_millis(10));
        session.handle(Input::Pressed, now + Duration::from_millis(20));

        assert!(session
            .handle(
                Input::Engine(final_event_for(1, "stale")),
                now + Duration::from_millis(30),
            )
            .is_empty());
        assert_eq!(session.state(), State::Recording);
    }

    #[test]
    fn repress_in_finalizing_emits_cancel_then_begin() {
        let mut session = session();
        let now = Instant::now();
        session.handle(Input::Pressed, now);
        session.handle(Input::Released, now + Duration::from_millis(10));

        assert_eq!(
            session.handle(Input::Pressed, now + Duration::from_millis(20)),
            vec![
                Output::Cancel { gen: 1 },
                Output::Begin { gen: 2 },
                Output::StartMic,
                Output::SetState(State::Recording),
            ]
        );
    }

    #[test]
    fn final_in_idle_is_dropped() {
        let mut session = session();
        assert!(session
            .handle(Input::Engine(final_event_for(0, "late")), Instant::now())
            .is_empty());
        assert_eq!(session.state(), State::Idle);
    }

    #[test]
    fn session_starts_disconnected() {
        let session = Session::new(SessionConfig {
            silence_pad_ms: 500,
            final_timeout: Duration::from_secs(3),
        });
        assert_eq!(session.state(), State::Disconnected);
    }

    #[test]
    fn session_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Session>();
    }
}
