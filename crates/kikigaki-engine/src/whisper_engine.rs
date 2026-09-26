//! Utterance-mode worker for the Whisper engine: no mid-utterance decoding, one decode per
//! utterance at `End`, VAD used only as an empty-speech guard.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;
#[cfg(any(test, feature = "whisper"))]
use std::time::Instant;

use anyhow::Context;
#[cfg(any(test, feature = "whisper"))]
use kikigaki_core::engine::run_worker;
use kikigaki_core::engine::{
    join_workers, AudioSink, Engine, EngineCmd, EngineMsg, EventSender, Waker,
};
#[cfg(any(test, feature = "whisper"))]
use kikigaki_core::models::{VAD_MODEL_ID, WHISPER_MODEL_ID};

use crate::framer::Framer;
#[cfg(any(test, feature = "whisper"))]
use crate::local::model_load_error;
#[cfg(feature = "whisper")]
use crate::local::report_model_load_failure;
use crate::local::{Recognizer, Vad};
use crate::whisper_guard::{collapse_immediate_repetition, drop_stock_hallucination};

/// 320ms of silence at 16kHz — the fixed warm-up decode buffer. Chosen to be short (fast) while
/// still exercising a full whisper.cpp `state.full` call, so the first *real* press doesn't pay
/// first-decode kernel/compile cost (Metal shader compilation, CPU codepath JIT-ish warm caches).
pub const WARMUP_SAMPLES: usize = 5_120;

/// Runs one throwaway decode on `WARMUP_SAMPLES` samples of silence and discards the result.
/// Called by the real loader (task D2's `WhisperEngine::start` wiring) once models are loaded,
/// before `Ready` is sent — mirrors `local.rs`'s model-load logging pattern
/// (`local.rs:246`'s `vad_load_ms, asr_load_ms`), just for a warm-up decode instead of a load.
pub fn run_warmup(asr: &mut dyn Recognizer) {
    let silence = vec![0.0f32; WARMUP_SAMPLES];
    let _ = asr.transcribe(&silence);
}

/// Applies the shipped hallucination and immediate-repetition guards to one decode result.
pub fn apply_output_guards(decoded: Option<String>) -> String {
    let decoded = decoded.unwrap_or_default();
    let decoded = drop_stock_hallucination(&decoded);
    collapse_immediate_repetition(decoded).to_owned()
}

/// Transcribes a complete offline utterance using the same VAD-empty skip and output guards as
/// the shipped worker path.
pub fn transcribe_utterance(
    vad: &mut dyn Vad,
    asr: &mut dyn Recognizer,
    samples: &[f32],
) -> String {
    let mut framer = Framer::new(512);
    framer.push(samples, |frame| vad.accept(frame));
    framer.flush_zero_padded(|frame| vad.accept(frame));
    vad.flush();
    let has_speech = match vad.drain() {
        Ok(segments) => !segments.is_empty(),
        Err(error) => {
            tracing::warn!(%error, "vad drain failed; treating as no speech");
            false
        }
    };
    if !has_speech {
        return String::new();
    }
    apply_output_guards(asr.transcribe(samples))
}

/// Sentinel meaning "not currently decoding" for `DecodeCancel::decoding_gen`.
const NO_GENERATION: u64 = u64::MAX;

/// Per-decode cancellation signal shared between the worker and the engine controller.
///
/// This signal is reset before every decode and affects only the utterance currently in flight.
/// The generation token ensures that stale cancellation requests cannot affect a later decode.
#[derive(Clone)]
pub struct DecodeCancel {
    canceled: Arc<AtomicBool>,
    decoding_gen: Arc<AtomicU64>,
}

impl DecodeCancel {
    /// Creates an idle per-decode cancellation signal.
    pub fn new() -> Self {
        Self {
            canceled: Arc::new(AtomicBool::new(false)),
            decoding_gen: Arc::new(AtomicU64::new(NO_GENERATION)),
        }
    }

    /// Publishes that the worker is about to decode `gen` and clears stale cancellation state.
    fn begin(&self, gen: u64) {
        self.canceled.store(false, Ordering::Relaxed);
        self.decoding_gen.store(gen, Ordering::Relaxed);
    }

    #[cfg(all(test, feature = "whisper"))]
    pub(crate) fn begin_for_test(&self, gen: u64) {
        self.begin(gen);
    }

    /// Clears the in-flight generation and returns whether its decode was canceled.
    fn end(&self) -> bool {
        self.decoding_gen.store(NO_GENERATION, Ordering::Relaxed);
        self.canceled.swap(false, Ordering::Relaxed)
    }

    /// Requests cancellation if `gen` is the generation currently being decoded.
    pub fn cancel(&self, gen: u64) {
        if self.decoding_gen.load(Ordering::Relaxed) == gen {
            self.canceled.store(true, Ordering::Relaxed);
        }
    }

    /// Returns whether the current decode has been asked to stop.
    pub fn should_abort(&self) -> bool {
        self.canceled.load(Ordering::Relaxed)
    }
}

impl Default for DecodeCancel {
    fn default() -> Self {
        Self::new()
    }
}

/// In-process Whisper transcription engine.
pub struct WhisperEngine {
    sink: AudioSink,
    events_rx: Receiver<EngineMsg>,
    events_tx: EventSender,
    done_rx: Receiver<()>,
    worker: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    decode_cancel: DecodeCancel,
}

impl WhisperEngine {
    /// Starts a worker thread that loads the VAD and Whisper models, warms up Whisper, and emits
    /// `Ready` when initialization finishes.
    #[cfg(feature = "whisper")]
    pub fn start(
        models_dir: std::path::PathBuf,
        vad: kikigaki_core::config::VadConfig,
        use_gpu: bool,
    ) -> anyhow::Result<Self> {
        let (sink, cmds) = kikigaki_core::engine::channel();
        let (events_tx, events_rx) = kikigaki_core::engine::event_channel(32);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let decode_cancel = DecodeCancel::new();
        let worker_sink = sink.clone();
        let worker_events = events_tx.clone();
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_decode_cancel = decode_cancel.clone();
        let failure_reporter = events_tx.clone();
        let tracing_dispatch = tracing::dispatcher::get_default(Clone::clone);
        let worker = std::thread::Builder::new()
            .name("whisper-engine".into())
            .spawn(move || {
                tracing::dispatcher::with_default(&tracing_dispatch, || {
                    let vad_models_dir = models_dir.clone();
                    let vad_config = vad.clone();
                    let recognizer_shutdown = Arc::clone(&worker_shutdown);
                    let recognizer_decode_cancel = worker_decode_cancel.clone();
                    let failure_events = worker_events.clone();
                    run_worker(&failure_reporter, &done_tx, || {
                        let result = run_whisper_with_loaders(
                            cmds,
                            worker_events,
                            worker_sink,
                            worker_shutdown,
                            worker_decode_cancel,
                            move || {
                                crate::sherpa::build_vad(&vad_models_dir, &vad_config)
                                    .map(|detector| Box::new(detector) as Box<dyn Vad>)
                            },
                            move || {
                                crate::whisper_recognizer::build_whisper_recognizer(
                                    &models_dir,
                                    use_gpu,
                                    recognizer_shutdown,
                                    recognizer_decode_cancel,
                                )
                                .map(|recognizer| Box::new(recognizer) as Box<dyn Recognizer>)
                            },
                        );
                        report_model_load_failure(&failure_events, result)
                    });
                });
            })
            .context("spawn Whisper engine worker")?;

        Ok(Self {
            sink,
            events_rx,
            events_tx,
            done_rx,
            worker: Some(worker),
            shutdown,
            decode_cancel,
        })
    }

    #[cfg(test)]
    fn start_with(
        vad: Box<dyn Vad + Send>,
        recognizer: Box<dyn Recognizer + Send>,
        shutdown: Arc<AtomicBool>,
        decode_cancel: DecodeCancel,
    ) -> Self {
        let (sink, cmds) = kikigaki_core::engine::channel();
        let (events_tx, events_rx) = kikigaki_core::engine::event_channel(32);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let worker_sink = sink.clone();
        let worker_events = events_tx.clone();
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_decode_cancel = decode_cancel.clone();
        let failure_reporter = events_tx.clone();
        let worker = std::thread::Builder::new()
            .name("whisper-engine-test".into())
            .spawn(move || {
                run_worker(
                    &failure_reporter,
                    &done_tx,
                    std::panic::AssertUnwindSafe(move || {
                        run_whisper_worker(
                            cmds,
                            worker_events,
                            worker_sink,
                            vad,
                            recognizer,
                            worker_shutdown,
                            worker_decode_cancel,
                        )
                    }),
                );
            })
            .expect("spawn test Whisper engine worker");

        Self {
            sink,
            events_rx,
            events_tx,
            done_rx,
            worker: Some(worker),
            shutdown,
            decode_cancel,
        }
    }
}

#[cfg(any(test, feature = "whisper"))]
fn run_whisper_with_loaders<V, A>(
    cmds: Receiver<EngineCmd>,
    events: EventSender,
    sink: AudioSink,
    shutdown: Arc<AtomicBool>,
    decode_cancel: DecodeCancel,
    make_vad: V,
    make_asr: A,
) -> anyhow::Result<()>
where
    V: FnOnce() -> anyhow::Result<Box<dyn Vad>>,
    A: FnOnce() -> anyhow::Result<Box<dyn Recognizer>>,
{
    let started = Instant::now();
    let vad = make_vad().map_err(|error| model_load_error(VAD_MODEL_ID, error))?;
    let vad_load_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let mut asr = make_asr().map_err(|error| model_load_error(WHISPER_MODEL_ID, error))?;
    let model_load_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let warmup_started = Instant::now();
    run_warmup(asr.as_mut());
    let warmup_ms = warmup_started.elapsed().as_secs_f64() * 1_000.0;
    tracing::info!(
        model_load_ms,
        vad_load_ms,
        warmup_ms,
        "whisper engine models loaded"
    );
    events
        .send(EngineMsg::Ready)
        .context("send whisper-engine ready")?;
    run_whisper_worker(cmds, events, sink, vad, asr, shutdown, decode_cancel)
}

impl Engine for WhisperEngine {
    fn sink(&self) -> AudioSink {
        self.sink.clone()
    }

    fn events(&mut self) -> &mut Receiver<EngineMsg> {
        &mut self.events_rx
    }

    fn set_waker(&mut self, waker: Option<Waker>) {
        self.events_tx.set_waker(waker);
    }

    fn cancel(&self, gen: u64) {
        self.decode_cancel.cancel(gen);
    }

    fn shutdown(mut self: Box<Self>) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.sink.send(EngineCmd::Shutdown);
        join_workers(
            [(&mut self.worker, "whisper engine worker")],
            &self.done_rx,
            "whisper engine",
        );
    }
}

/// Runs the ordered Whisper utterance-mode decode loop until shutdown or channel closure.
pub fn run_whisper_worker(
    cmds: Receiver<EngineCmd>,
    events: EventSender,
    sink: AudioSink,
    mut vad: Box<dyn Vad>,
    mut asr: Box<dyn Recognizer>,
    shutdown: Arc<AtomicBool>,
    decode_cancel: DecodeCancel,
) -> anyhow::Result<()> {
    let mut framer = Framer::new(512);
    let mut buffer: Vec<f32> = Vec::new();
    let mut current: Option<u64> = None;

    while let Ok(command) = cmds.recv() {
        match command {
            EngineCmd::Begin { gen } => {
                current = Some(gen);
                buffer.clear();
                framer.reset();
                vad.reset()?;
            }
            EngineCmd::Audio(samples) if current.is_some() => {
                buffer.extend_from_slice(&samples);
                framer.push(&samples, |frame| vad.accept(frame));
            }
            EngineCmd::Audio(_) => {
                tracing::trace!("dropping whisper-engine audio without an active generation");
            }
            EngineCmd::End { gen, pad_ms } if current == Some(gen) => {
                let pad_samples = usize::try_from(pad_ms.saturating_mul(16))
                    .context("silence pad sample count exceeds address space")?;
                let silence = vec![0.0; pad_samples];
                framer.push(&silence, |frame| vad.accept(frame));
                framer.flush_zero_padded(|frame| vad.accept(frame));
                vad.flush();
                let has_speech = !vad.drain()?.is_empty();

                let started_at = std::time::Instant::now();
                let (text, aborted) = if has_speech {
                    decode_cancel.begin(gen);
                    let decoded = asr.transcribe(&buffer);
                    let canceled = decode_cancel.end();
                    let aborted = canceled || shutdown.load(Ordering::Relaxed);
                    let text = if aborted {
                        String::new()
                    } else {
                        apply_output_guards(decoded)
                    };
                    (text, aborted)
                } else {
                    (String::new(), false)
                };

                current = None;
                buffer.clear();
                if aborted {
                    tracing::debug!(
                        gen,
                        "whisper decode aborted (cancel or shutdown); suppressing stale Final"
                    );
                    continue;
                }

                let engine_latency_ms = started_at.elapsed().as_secs_f64() * 1_000.0;
                let dropped_chunks = sink.take_dropped();
                events.send(EngineMsg::Final {
                    gen,
                    text,
                    engine_latency_ms: Some(engine_latency_ms),
                    vad_close_at: std::time::Instant::now(),
                    asr_end_at: std::time::Instant::now(),
                    dropped_chunks,
                })?;
            }
            EngineCmd::End { .. } => {
                tracing::debug!("ignoring stale whisper-engine end command");
            }
            EngineCmd::Cancel { .. } => {
                current = None;
                buffer.clear();
                framer.reset();
                vad.reset()?;
                let _ = sink.take_dropped();
            }
            EngineCmd::Shutdown => return Ok(()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use kikigaki_core::engine::{self, Engine, EngineCmd, EngineMsg};
    use kikigaki_core::models::{VAD_MODEL_ID, WHISPER_MODEL_ID};

    use super::*;
    use crate::local::{Recognizer, Vad, VadSegment};

    struct AlwaysSpeechVad;

    impl Vad for AlwaysSpeechVad {
        fn accept(&mut self, _frame: &[f32]) {}

        fn flush(&mut self) {}

        fn reset(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn drain(&mut self) -> anyhow::Result<Vec<VadSegment>> {
            Ok(vec![VadSegment {
                start: 0,
                samples: vec![0.0; 512],
            }])
        }
    }

    struct SilentVad;

    impl Vad for SilentVad {
        fn accept(&mut self, _frame: &[f32]) {}

        fn flush(&mut self) {}

        fn reset(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn drain(&mut self) -> anyhow::Result<Vec<VadSegment>> {
            Ok(Vec::new())
        }
    }

    struct CapturingRecognizer(Arc<Mutex<Vec<Vec<f32>>>>);

    impl Recognizer for CapturingRecognizer {
        fn transcribe(&mut self, samples: &[f32]) -> Option<String> {
            self.0.lock().unwrap().push(samples.to_vec());
            Some("decoded".into())
        }
    }

    /// Simulates whisper.cpp's abort callback after signaling that the decode is in flight.
    struct AbortAwareRecognizer {
        shutdown: Arc<AtomicBool>,
        decode_cancel: DecodeCancel,
        entered: std::sync::mpsc::SyncSender<()>,
    }

    impl Recognizer for AbortAwareRecognizer {
        fn transcribe(&mut self, _samples: &[f32]) -> Option<String> {
            self.entered.send(()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            loop {
                if self.shutdown.load(Ordering::Relaxed) || self.decode_cancel.should_abort() {
                    return None;
                }
                if std::time::Instant::now() > deadline {
                    panic!("abort was never observed");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn run(
        vad: Box<dyn Vad>,
        asr: Box<dyn Recognizer>,
        commands: impl IntoIterator<Item = EngineCmd>,
    ) -> std::sync::mpsc::Receiver<EngineMsg> {
        let (sink, cmds) = engine::channel();
        for command in commands {
            sink.send(command).unwrap();
        }
        sink.send(EngineCmd::Shutdown).unwrap();
        let (events_tx, events_rx) = engine::event_channel(8);
        let shutdown = Arc::new(AtomicBool::new(false));
        let decode_cancel = DecodeCancel::new();
        run_whisper_worker(cmds, events_tx, sink, vad, asr, shutdown, decode_cancel).unwrap();
        events_rx
    }

    #[test]
    fn warmup_decodes_a_fixed_silent_buffer_and_discards_the_result() {
        struct CountingRecognizer(Arc<AtomicUsize>, Arc<Mutex<Vec<Vec<f32>>>>);
        impl Recognizer for CountingRecognizer {
            fn transcribe(&mut self, samples: &[f32]) -> Option<String> {
                self.0.fetch_add(1, Ordering::Relaxed);
                self.1.lock().unwrap().push(samples.to_vec());
                Some("should be discarded".into())
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut recognizer = CountingRecognizer(Arc::clone(&calls), Arc::clone(&captured));

        run_warmup(&mut recognizer);

        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "expected exactly one warm-up decode call"
        );
        let captured = captured.lock().unwrap();
        assert_eq!(captured[0].len(), WARMUP_SAMPLES);
        assert!(
            captured[0].iter().all(|&sample| sample == 0.0),
            "warm-up buffer must be silence"
        );
    }

    fn loader_failure(
        make_vad: impl FnMut() -> anyhow::Result<Box<dyn Vad>>,
        make_asr: impl FnMut() -> anyhow::Result<Box<dyn Recognizer>>,
    ) -> (String, Option<&'static str>) {
        let (sink, cmds) = engine::channel();
        let (events_tx, events_rx) = engine::event_channel(8);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker_events = events_tx.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let decode_cancel = DecodeCancel::new();
        kikigaki_core::engine::run_worker(
            &events_tx,
            &done_tx,
            std::panic::AssertUnwindSafe(|| {
                let result = run_whisper_with_loaders(
                    cmds,
                    worker_events,
                    sink,
                    shutdown,
                    decode_cancel,
                    make_vad,
                    make_asr,
                );
                crate::local::report_model_load_failure(&events_tx, result)
            }),
        );
        done_rx.recv().unwrap();
        events_rx
            .try_iter()
            .find_map(|message| match message {
                EngineMsg::Disconnected {
                    reason,
                    failed_model,
                } => Some((reason, failed_model)),
                _ => None,
            })
            .expect("worker should emit a disconnect reason")
    }

    #[test]
    fn vad_load_failure_reports_the_vad_model_id_not_whisper() {
        let failure = loader_failure(
            || anyhow::bail!("bad Whisper VAD"),
            || {
                Ok(Box::new(CapturingRecognizer(Arc::new(Mutex::new(
                    Vec::new(),
                )))))
            },
        );

        assert!(failure.0.contains("bad Whisper VAD"), "{failure:?}");
        assert_eq!(failure.1, Some(VAD_MODEL_ID));
    }

    #[test]
    fn recognizer_load_failure_reports_the_whisper_model_id() {
        let failure = loader_failure(
            || Ok(Box::new(AlwaysSpeechVad)),
            || anyhow::bail!("bad Whisper recognizer"),
        );

        assert!(failure.0.contains("bad Whisper recognizer"), "{failure:?}");
        assert_eq!(failure.1, Some(WHISPER_MODEL_ID));
    }

    #[test]
    fn ready_is_sent_only_after_warmup_ran() {
        struct CountingRecognizer(Arc<AtomicUsize>);
        impl Recognizer for CountingRecognizer {
            fn transcribe(&mut self, _samples: &[f32]) -> Option<String> {
                self.0.fetch_add(1, Ordering::Relaxed);
                None
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let recognizer_calls = Arc::clone(&calls);
        let (sink, cmds) = engine::channel();
        sink.send(EngineCmd::Shutdown).unwrap();
        let (events_tx, events_rx) = engine::event_channel(8);
        let calls_when_ready_was_sent = Arc::new(AtomicUsize::new(usize::MAX));
        events_tx.set_waker(Some(Arc::new({
            let calls = Arc::clone(&calls);
            let calls_when_ready_was_sent = Arc::clone(&calls_when_ready_was_sent);
            move || {
                calls_when_ready_was_sent.store(calls.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        })));

        run_whisper_with_loaders(
            cmds,
            events_tx,
            sink,
            Arc::new(AtomicBool::new(false)),
            DecodeCancel::new(),
            || Ok(Box::new(AlwaysSpeechVad)),
            move || Ok(Box::new(CountingRecognizer(recognizer_calls))),
        )
        .unwrap();

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(calls_when_ready_was_sent.load(Ordering::Relaxed), 1);
        assert!(matches!(events_rx.recv().unwrap(), EngineMsg::Ready));
    }

    #[test]
    fn one_decode_call_receives_the_whole_utterance() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let events = run(
            Box::new(AlwaysSpeechVad),
            Box::new(CapturingRecognizer(Arc::clone(&captured))),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(vec![0.1; 512]),
                EngineCmd::Audio(vec![0.2; 512]),
                EngineCmd::End { gen: 1, pad_ms: 0 },
            ],
        );
        let captured = captured.lock().unwrap();
        assert_eq!(
            captured.len(),
            1,
            "expected exactly one decode call, got {}",
            captured.len()
        );
        assert_eq!(captured[0].len(), 1024);
        match events.recv().unwrap() {
            EngineMsg::Final { gen, text, .. } => {
                assert_eq!(gen, 1);
                assert_eq!(text, "decoded");
            }
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn silent_utterance_skips_the_recognizer_and_emits_empty_final() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let events = run(
            Box::new(SilentVad),
            Box::new(CapturingRecognizer(Arc::clone(&captured))),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(vec![0.0; 512]),
                EngineCmd::End { gen: 1, pad_ms: 0 },
            ],
        );
        assert!(
            captured.lock().unwrap().is_empty(),
            "recognizer must not be called on silence"
        );
        match events.recv().unwrap() {
            EngineMsg::Final { text, .. } => assert_eq!(text, ""),
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn transcribe_utterance_skips_decode_when_vad_finds_no_speech() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut vad = SilentVad;
        let mut recognizer = CapturingRecognizer(Arc::clone(&captured));

        let text = transcribe_utterance(&mut vad, &mut recognizer, &[0.0; 700]);

        assert_eq!(text, "");
        assert!(
            captured.lock().unwrap().is_empty(),
            "recognizer must not be called on silence"
        );
    }

    #[test]
    fn transcribe_utterance_decodes_once_and_applies_output_guards() {
        struct RepeatingRecognizer(Arc<Mutex<Vec<Vec<f32>>>>);

        impl Recognizer for RepeatingRecognizer {
            fn transcribe(&mut self, samples: &[f32]) -> Option<String> {
                self.0.lock().unwrap().push(samples.to_vec());
                Some("テストテスト".into())
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let samples = vec![0.25; 700];
        let mut vad = AlwaysSpeechVad;
        let mut recognizer = RepeatingRecognizer(Arc::clone(&captured));

        let text = transcribe_utterance(&mut vad, &mut recognizer, &samples);

        assert_eq!(text, "テスト");
        assert_eq!(*captured.lock().unwrap(), vec![samples]);
    }

    #[test]
    fn cancel_then_begin_keeps_only_the_second_utterance() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let events = run(
            Box::new(AlwaysSpeechVad),
            Box::new(CapturingRecognizer(Arc::clone(&captured))),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(vec![0.1; 512]),
                EngineCmd::Cancel { gen: 1 },
                EngineCmd::Begin { gen: 2 },
                EngineCmd::Audio(vec![0.2; 512]),
                EngineCmd::End { gen: 2, pad_ms: 0 },
            ],
        );
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0], vec![0.2; 512]);
        match events.recv().unwrap() {
            EngineMsg::Final { gen, .. } => assert_eq!(gen, 2),
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn a_decode_canceled_mid_flight_emits_no_final_for_that_generation() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let decode_cancel = DecodeCancel::new();
        let (sink, cmds) = engine::channel();
        sink.send(EngineCmd::Begin { gen: 1 }).unwrap();
        sink.send(EngineCmd::Audio(vec![0.1; 512])).unwrap();
        sink.send(EngineCmd::End { gen: 1, pad_ms: 0 }).unwrap();
        sink.send(EngineCmd::Shutdown).unwrap();
        let (events_tx, events_rx) = engine::event_channel(8);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let recognizer = AbortAwareRecognizer {
            shutdown: Arc::clone(&shutdown),
            decode_cancel: decode_cancel.clone(),
            entered: entered_tx,
        };
        let canceler = decode_cancel.clone();
        let worker = std::thread::spawn(move || {
            run_whisper_worker(
                cmds,
                events_tx,
                sink,
                Box::new(AlwaysSpeechVad),
                Box::new(recognizer),
                shutdown,
                decode_cancel,
            )
        });

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker never entered the decode for generation 1");
        canceler.cancel(1);
        worker.join().unwrap().unwrap();
        assert!(
            events_rx.try_recv().is_err(),
            "a canceled decode must not emit a Final for its generation"
        );
    }

    #[test]
    fn shutdown_sets_the_abort_flag_before_or_with_the_shutdown_send() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let decode_cancel = DecodeCancel::new();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let recognizer = AbortAwareRecognizer {
            shutdown: Arc::clone(&shutdown),
            decode_cancel: decode_cancel.clone(),
            entered: entered_tx,
        };
        let engine = WhisperEngine::start_with(
            Box::new(AlwaysSpeechVad),
            Box::new(recognizer),
            Arc::clone(&shutdown),
            decode_cancel,
        );
        let sink = engine.sink();
        sink.send(EngineCmd::Begin { gen: 1 }).unwrap();
        sink.send(EngineCmd::Audio(vec![0.1; 512])).unwrap();
        sink.send(EngineCmd::End { gen: 1, pad_ms: 0 }).unwrap();

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker never entered the decode for generation 1");
        let started = std::time::Instant::now();
        Box::new(engine).shutdown();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "shutdown must not wait out the full blocking decode"
        );
    }

    #[test]
    fn engine_cancel_aborts_an_in_flight_decode_and_emits_no_final() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let decode_cancel = DecodeCancel::new();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let recognizer = AbortAwareRecognizer {
            shutdown: Arc::clone(&shutdown),
            decode_cancel: decode_cancel.clone(),
            entered: entered_tx,
        };
        let mut engine = WhisperEngine::start_with(
            Box::new(AlwaysSpeechVad),
            Box::new(recognizer),
            shutdown,
            decode_cancel,
        );
        let sink = engine.sink();
        sink.send(EngineCmd::Begin { gen: 7 }).unwrap();
        sink.send(EngineCmd::Audio(vec![0.1; 512])).unwrap();
        sink.send(EngineCmd::End { gen: 7, pad_ms: 0 }).unwrap();

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker never entered the decode for generation 7");
        engine.cancel(7);
        let result = engine.events().recv_timeout(Duration::from_millis(500));
        assert!(
            result.is_err(),
            "expected no Final after a canceled decode, got {result:?}"
        );
        Box::new(engine).shutdown();
    }

    #[test]
    fn cancel_for_a_different_generation_does_not_suppress_this_decode() {
        let decode_cancel = DecodeCancel::new();
        decode_cancel.begin(1);
        decode_cancel.cancel(2);
        assert!(!decode_cancel.should_abort());
        assert!(!decode_cancel.end());
    }
}
