use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::{ensure, Context};
use kikigaki_core::config::{AsrConfig, VadConfig};
use kikigaki_core::engine::{
    self, join_workers, run_worker, AudioSink, Engine, EngineCmd, EngineMsg, EventSender, Waker,
};
use kikigaki_core::models::{ASR_MODEL_ID, VAD_MODEL_ID};

use crate::framer::Framer;
use crate::hotwords::HotwordSetup;
use crate::sherpa::{build_recognizer_auto, build_vad};

const SAMPLE_RATE: usize = 16_000;
const PREROLL_MS: usize = 1_000;
const PREROLL_SAMPLES: usize = SAMPLE_RATE * PREROLL_MS / 1_000;
const HISTORY_MARGIN_SAMPLES: usize = SAMPLE_RATE;

/// One complete VAD speech segment and its start in the detector's sample stream.
pub struct VadSegment {
    /// Zero-based sample offset since the detector's last reset.
    pub start: usize,
    /// The speech samples emitted by the detector.
    pub samples: Vec<f32>,
}

struct AudioHistory {
    samples: VecDeque<f32>,
    base: usize,
    max_samples: usize,
}

impl AudioHistory {
    fn new(max_speech_s: f32) -> Self {
        let max_speech_samples = (max_speech_s * SAMPLE_RATE as f32).ceil() as usize;
        Self {
            samples: VecDeque::new(),
            base: 0,
            max_samples: max_speech_samples + PREROLL_SAMPLES + HISTORY_MARGIN_SAMPLES,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        self.samples.extend(samples);
        let overflow = self.samples.len().saturating_sub(self.max_samples);
        self.samples.drain(..overflow);
        self.base += overflow;
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.base = 0;
    }

    fn base(&self) -> usize {
        self.base
    }

    fn append_range(&self, start: usize, end: usize, output: &mut Vec<f32>) -> anyhow::Result<()> {
        let available_end = self.base + self.samples.len();
        if start < self.base || end > available_end || start > end {
            anyhow::bail!(
                "audio history range {start}..{end} outside {}..{available_end}",
                self.base
            );
        }
        output.extend(
            self.samples
                .range(start - self.base..end - self.base)
                .copied(),
        );
        Ok(())
    }
}

fn accept_frame(history: &mut AudioHistory, vad: &mut dyn Vad, frame: &[f32]) {
    history.push(frame);
    vad.accept(frame);
}

/// Voice activity detector operations required by the local worker.
pub trait Vad {
    /// Feeds one exact 512-sample frame into the detector.
    fn accept(&mut self, frame: &[f32]);
    /// Closes buffered trailing speech.
    fn flush(&mut self);
    /// Clears detector state at an utterance boundary.
    fn reset(&mut self) -> anyhow::Result<()>;
    /// Takes all complete speech segments currently queued by the detector.
    fn drain(&mut self) -> anyhow::Result<Vec<VadSegment>>;
}

/// Offline recognition operation required by the local worker.
pub trait Recognizer {
    /// Transcribes one 16 kHz mono speech segment.
    fn transcribe(&mut self, samples: &[f32]) -> Option<String>;
}

/// In-process Silero VAD and ReazonSpeech transcription engine.
pub struct LocalEngine {
    sink: AudioSink,
    events_rx: Receiver<EngineMsg>,
    events_tx: EventSender,
    done_rx: Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl LocalEngine {
    /// Starts a worker thread that loads models and emits `Ready` when initialization finishes.
    pub fn start(
        models_dir: std::path::PathBuf,
        asr: AsrConfig,
        vad: VadConfig,
        hotwords_score: Option<f32>,
    ) -> anyhow::Result<Self> {
        let (sink, cmds) = engine::channel();
        let (events_tx, events_rx) = engine::event_channel(32);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let worker_sink = sink.clone();
        let worker_events = events_tx.clone();
        let failure_reporter = events_tx.clone();
        let tracing_dispatch = tracing::dispatcher::get_default(Clone::clone);
        let worker = thread::Builder::new()
            .name("local-engine".into())
            .spawn(move || {
                tracing::dispatcher::with_default(&tracing_dispatch, || {
                    let vad_models_dir = models_dir.clone();
                    let vad_config = vad.clone();
                    let failure_events = worker_events.clone();
                    run_worker(&failure_reporter, &done_tx, || {
                        let result = run_with_loaders(
                            cmds,
                            worker_events,
                            worker_sink,
                            vad.max_speech_s,
                            move || {
                                build_vad(&vad_models_dir, &vad_config)
                                    .map(|detector| Box::new(detector) as Box<dyn Vad>)
                            },
                            move || {
                                build_recognizer_auto(&models_dir, &asr, hotwords_score)
                                    .map(|recognizer| Box::new(recognizer) as Box<dyn Recognizer>)
                            },
                        );
                        report_model_load_failure(&failure_events, result)
                    });
                });
            })
            .context("spawn local engine worker")?;

        Ok(Self {
            sink,
            events_rx,
            events_tx,
            done_rx,
            worker: Some(worker),
        })
    }
}

/// Runs the optional hotword setup/build path, degrading to the baseline builder on failure.
///
/// Invalid scores are configuration errors and are rejected before materialization. Filesystem
/// and recognizer-construction failures are optional-feature failures, so they are logged and the
/// baseline builder is used instead.
pub(crate) fn with_hotword_fallback<T, M, B, H>(
    hotwords_score: Option<f32>,
    materialize_hotwords: M,
    build_baseline: B,
    build_with_hotwords: H,
) -> anyhow::Result<T>
where
    M: FnOnce() -> anyhow::Result<HotwordSetup>,
    B: FnOnce() -> anyhow::Result<T>,
    H: FnOnce(HotwordSetup, f32) -> anyhow::Result<T>,
{
    let Some(score) = hotwords_score else {
        return build_baseline();
    };
    ensure!(
        score.is_finite() && score > 0.0,
        "hotwords score must be finite and greater than zero, got {score}"
    );

    let setup = match materialize_hotwords() {
        Ok(setup) => setup,
        Err(error) => {
            tracing::warn!(%error, "hotword setup failed; continuing without hotwords");
            return build_baseline();
        }
    };
    match build_with_hotwords(setup, score) {
        Ok(recognizer) => Ok(recognizer),
        Err(error) => {
            tracing::warn!(%error, "hotword recognizer build failed; continuing without hotwords");
            build_baseline()
        }
    }
}

impl Engine for LocalEngine {
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
        let _ = self.sink.send(EngineCmd::Shutdown);
        join_workers(
            [(&mut self.worker, "local engine worker")],
            &self.done_rx,
            "local engine",
        );
    }
}

fn run_with_loaders<V, A>(
    cmds: Receiver<EngineCmd>,
    events: EventSender,
    sink: AudioSink,
    max_speech_s: f32,
    mut make_vad: V,
    mut make_asr: A,
) -> anyhow::Result<()>
where
    V: FnMut() -> anyhow::Result<Box<dyn Vad>> + 'static,
    A: FnMut() -> anyhow::Result<Box<dyn Recognizer>>,
{
    let started = Instant::now();
    let vad = make_vad().map_err(|error| model_load_error(VAD_MODEL_ID, error))?;
    let vad_load_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let asr = make_asr().map_err(|error| model_load_error(ASR_MODEL_ID, error))?;
    let asr_load_ms = started.elapsed().as_secs_f64() * 1_000.0 - vad_load_ms;
    tracing::info!(vad_load_ms, asr_load_ms, "local engine models loaded");
    events
        .send(EngineMsg::Ready)
        .context("send local-engine ready")?;
    let rebuild_vad =
        Box::new(move || make_vad().map_err(|error| model_load_error(VAD_MODEL_ID, error)));
    run_local_worker(cmds, events, sink, vad, asr, max_speech_s, rebuild_vad)
}

#[derive(Debug)]
struct ModelLoadError {
    id: &'static str,
    error: anyhow::Error,
}

impl std::fmt::Display for ModelLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:#}", self.error)
    }
}

impl std::error::Error for ModelLoadError {}

fn model_load_error(id: &'static str, error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(ModelLoadError { id, error })
}

fn report_model_load_failure(
    events: &EventSender,
    result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => match error.downcast::<ModelLoadError>() {
            Ok(failure) => {
                let _ = events.send(EngineMsg::Disconnected {
                    reason: format!("{failure}"),
                    failed_model: Some(failure.id),
                });
                Ok(())
            }
            Err(error) => Err(error),
        },
    }
}

/// Runs the ordered local-engine decode loop until shutdown or channel closure.
pub fn run_local_worker(
    cmds: Receiver<EngineCmd>,
    events: EventSender,
    sink: AudioSink,
    mut vad: Box<dyn Vad>,
    mut asr: Box<dyn Recognizer>,
    max_speech_s: f32,
    mut make_vad: Box<dyn FnMut() -> anyhow::Result<Box<dyn Vad>>>,
) -> anyhow::Result<()> {
    let mut framer = Framer::new(512);
    let mut history = AudioHistory::new(max_speech_s);
    let mut last_segment_end = 0;
    let mut current = None;

    while let Ok(command) = cmds.recv() {
        match command {
            EngineCmd::Begin { gen } => {
                if current.is_some() {
                    framer.reset();
                }
                current = Some(gen);
                history.clear();
                last_segment_end = 0;
            }
            EngineCmd::Audio(samples) if current.is_some() => {
                framer.push(&samples, |frame| {
                    accept_frame(&mut history, vad.as_mut(), frame)
                });
                let vad_close_at = Instant::now();
                let segments = drain_and_transcribe(
                    vad.as_mut(),
                    asr.as_mut(),
                    &history,
                    &mut last_segment_end,
                )?;
                let gen = current.expect("active generation checked above");
                for segment in segments {
                    tracing::debug!(
                        mid_utterance = true,
                        segments = 1,
                        text = %segment.text,
                        "local-engine final"
                    );
                    let dropped_chunks = sink.take_dropped();
                    events
                        .send(EngineMsg::Final {
                            gen,
                            text: segment.text,
                            engine_latency_ms: Some(segment.engine_latency_ms),
                            vad_close_at,
                            asr_end_at: segment.asr_end_at,
                            dropped_chunks,
                        })
                        .context("send local-engine mid-utterance final")?;
                }
            }
            EngineCmd::Audio(_) => {
                tracing::trace!("dropping local-engine audio without an active generation");
            }
            EngineCmd::End { gen, pad_ms } if current == Some(gen) => {
                let pad_samples = usize::try_from(pad_ms.saturating_mul(16))
                    .context("silence pad sample count exceeds address space")?;
                let silence = vec![0.0; pad_samples];
                let mut feed = |frame: &[f32]| accept_frame(&mut history, vad.as_mut(), frame);
                framer.push(&silence, &mut feed);
                framer.flush_zero_padded(&mut feed);
                vad.flush();
                let vad_close_at = Instant::now();
                let segments = drain_and_transcribe(
                    vad.as_mut(),
                    asr.as_mut(),
                    &history,
                    &mut last_segment_end,
                )?;
                let asr_end_at = Instant::now();
                let segment_count = segments.len();
                let text = segments
                    .into_iter()
                    .map(|segment| segment.text)
                    .collect::<String>();
                tracing::debug!(
                    mid_utterance = false,
                    segments = segment_count,
                    %text,
                    "local-engine final"
                );
                let dropped_chunks = sink.take_dropped();
                events
                    .send(EngineMsg::Final {
                        gen,
                        text,
                        engine_latency_ms: Some(
                            asr_end_at.duration_since(vad_close_at).as_secs_f64() * 1_000.0,
                        ),
                        vad_close_at,
                        asr_end_at,
                        dropped_chunks,
                    })
                    .context("send local-engine final")?;
                reset_or_rebuild(&mut vad, make_vad.as_mut())?;
                current = None;
                history.clear();
                last_segment_end = 0;
            }
            EngineCmd::End { .. } => {
                tracing::debug!("ignoring stale local-engine end command");
            }
            EngineCmd::Cancel { .. } => {
                current = None;
                framer.reset();
                reset_or_rebuild(&mut vad, make_vad.as_mut())?;
                history.clear();
                last_segment_end = 0;
                let _ = sink.take_dropped();
            }
            EngineCmd::Shutdown => return Ok(()),
        }
    }
    Ok(())
}

struct DecodedSegment {
    text: String,
    engine_latency_ms: f64,
    asr_end_at: Instant,
}

fn drain_and_transcribe(
    vad: &mut dyn Vad,
    asr: &mut dyn Recognizer,
    history: &AudioHistory,
    last_segment_end: &mut usize,
) -> anyhow::Result<Vec<DecodedSegment>> {
    let mut decoded = Vec::new();
    for segment in vad.drain()? {
        let segment_ms = segment.samples.len() as f64 / 16.0;
        let want_start = segment
            .start
            .saturating_sub(PREROLL_SAMPLES)
            .max(*last_segment_end)
            .max(history.base());
        let mut decode_samples = Vec::with_capacity(
            segment
                .start
                .saturating_sub(want_start)
                .saturating_add(segment.samples.len()),
        );
        if want_start < segment.start {
            history.append_range(want_start, segment.start, &mut decode_samples)?;
        }
        let preroll_samples = decode_samples.len();
        decode_samples.extend_from_slice(&segment.samples);
        *last_segment_end = segment.start.saturating_add(segment.samples.len());
        let preroll_ms = preroll_samples as f64 / 16.0;
        let decode_started = Instant::now();
        if let Some(text) = asr.transcribe(&decode_samples) {
            let asr_end_at = Instant::now();
            tracing::debug!(segment_ms, preroll_ms, %text, "decoded VAD segment");
            if !text.is_empty() {
                decoded.push(DecodedSegment {
                    text,
                    engine_latency_ms: asr_end_at.duration_since(decode_started).as_secs_f64()
                        * 1_000.0,
                    asr_end_at,
                });
            }
        } else {
            tracing::warn!("local recognizer returned no result for VAD segment");
        }
    }
    Ok(decoded)
}

fn reset_or_rebuild(
    vad: &mut Box<dyn Vad>,
    make_vad: &mut dyn FnMut() -> anyhow::Result<Box<dyn Vad>>,
) -> anyhow::Result<()> {
    if let Err(error) = vad.reset() {
        tracing::warn!(%error, "VAD reset failed; rebuilding detector");
        *vad = make_vad()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::panic::AssertUnwindSafe;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Mutex};

    use kikigaki_core::engine::{self, EngineCmd, EngineMsg};
    use kikigaki_core::models::{ASR_MODEL_ID, VAD_MODEL_ID};

    use super::{
        report_model_load_failure, run_local_worker, run_with_loaders, with_hotword_fallback,
        Recognizer, Vad, VadSegment,
    };

    #[test]
    fn hotword_setup_failure_warns_and_selects_baseline_config() {
        let models_dir = std::path::Path::new("/models");
        let asr = kikigaki_core::config::AsrConfig::default();
        let expected = crate::sherpa::recognizer_config(models_dir, &asr, None).unwrap();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let subscriber = WarningSubscriber(Arc::clone(&messages));

        let actual = tracing::subscriber::with_default(subscriber, || {
            with_hotword_fallback(
                Some(3.0),
                || anyhow::bail!("injected materialization failure"),
                || crate::sherpa::recognizer_config(models_dir, &asr, None),
                |setup, score| {
                    crate::sherpa::recognizer_config(
                        models_dir,
                        &asr,
                        Some((&setup.hotwords_file, score, &setup.bpe_vocab)),
                    )
                },
            )
        })
        .unwrap();

        assert_eq!(format!("{actual:#?}"), format!("{expected:#?}"));
        let warning = messages.lock().unwrap().join("\n");
        assert!(
            warning.contains("injected materialization failure"),
            "{warning:?}"
        );
        assert!(warning.contains("without hotwords"), "{warning:?}");
    }

    struct WarningSubscriber(Arc<Mutex<Vec<String>>>);

    impl tracing::Subscriber for WarningSubscriber {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            struct Visitor(String);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push_str(&format!("{}={value:?} ", field.name()));
                }
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    struct FakeVad {
        every: usize,
        accepted: usize,
        pending: Vec<VadSegment>,
        fail_reset: Arc<AtomicBool>,
    }

    impl FakeVad {
        fn new(every: usize, fail_reset: Arc<AtomicBool>) -> Self {
            Self {
                every,
                accepted: 0,
                pending: Vec::new(),
                fail_reset,
            }
        }
    }

    impl Vad for FakeVad {
        fn accept(&mut self, frame: &[f32]) {
            self.accepted += 1;
            if self.accepted.is_multiple_of(self.every) {
                self.pending.push(VadSegment {
                    start: (self.accepted - 1) * frame.len(),
                    samples: frame.to_vec(),
                });
            }
        }

        fn flush(&mut self) {}

        fn reset(&mut self) -> anyhow::Result<()> {
            self.accepted = 0;
            self.pending.clear();
            if self.fail_reset.swap(false, Ordering::SeqCst) {
                anyhow::bail!("forced reset failure");
            }
            Ok(())
        }

        fn drain(&mut self) -> anyhow::Result<Vec<VadSegment>> {
            Ok(std::mem::take(&mut self.pending))
        }
    }

    struct OffsetVad {
        pending: Vec<VadSegment>,
        wait_for_reset: bool,
    }

    impl Vad for OffsetVad {
        fn accept(&mut self, _frame: &[f32]) {}

        fn flush(&mut self) {}

        fn reset(&mut self) -> anyhow::Result<()> {
            self.wait_for_reset = false;
            Ok(())
        }

        fn drain(&mut self) -> anyhow::Result<Vec<VadSegment>> {
            if self.wait_for_reset {
                Ok(Vec::new())
            } else {
                Ok(std::mem::take(&mut self.pending))
            }
        }
    }

    struct CapturingRecognizer(Arc<Mutex<Vec<Vec<f32>>>>);

    impl Recognizer for CapturingRecognizer {
        fn transcribe(&mut self, samples: &[f32]) -> Option<String> {
            self.0.lock().unwrap().push(samples.to_vec());
            Some("seg".into())
        }
    }

    struct FakeRecognizer;

    impl Recognizer for FakeRecognizer {
        fn transcribe(&mut self, _samples: &[f32]) -> Option<String> {
            Some("seg".into())
        }
    }

    struct EmptyRecognizer;

    impl Recognizer for EmptyRecognizer {
        fn transcribe(&mut self, _samples: &[f32]) -> Option<String> {
            None
        }
    }

    struct FlushVad {
        inner: FakeVad,
    }

    impl Vad for FlushVad {
        fn accept(&mut self, frame: &[f32]) {
            self.inner.accept(frame);
        }

        fn flush(&mut self) {
            self.inner.pending.push(VadSegment {
                start: self.inner.accepted * 512,
                samples: vec![0.0; 512],
            });
        }

        fn reset(&mut self) -> anyhow::Result<()> {
            self.inner.reset()
        }

        fn drain(&mut self) -> anyhow::Result<Vec<VadSegment>> {
            self.inner.drain()
        }
    }

    fn captured_segments(
        vad: OffsetVad,
        commands: impl IntoIterator<Item = EngineCmd>,
    ) -> Vec<Vec<f32>> {
        let (sink, cmds) = engine::channel();
        for command in commands {
            sink.send(command).unwrap();
        }
        sink.send(EngineCmd::Shutdown).unwrap();
        let (events_tx, _events_rx) = engine::event_channel(8);
        let captured = Arc::new(Mutex::new(Vec::new()));
        run_local_worker(
            cmds,
            events_tx,
            sink,
            Box::new(vad),
            Box::new(CapturingRecognizer(Arc::clone(&captured))),
            12.0,
            Box::new(|| Ok(Box::new(FakeVad::new(1, Arc::new(AtomicBool::new(false)))))),
        )
        .unwrap();
        Arc::try_unwrap(captured).unwrap().into_inner().unwrap()
    }

    #[test]
    fn segment_gets_one_second_of_preroll() {
        let history: Vec<f32> = (0..32_768).map(|sample| sample as f32).collect();
        let segment_samples = vec![-1.0; 8_000];
        let captured = captured_segments(
            OffsetVad {
                pending: vec![VadSegment {
                    start: 24_000,
                    samples: segment_samples.clone(),
                }],
                wait_for_reset: false,
            },
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(history.clone()),
            ],
        );

        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].len(), 16_000 + segment_samples.len());
        assert_eq!(&captured[0][..16_000], &history[8_000..24_000]);
        assert_eq!(&captured[0][16_000..], segment_samples);
    }

    #[test]
    fn preroll_is_clamped_to_history_base() {
        let history: Vec<f32> = (0..8_192).map(|sample| sample as f32).collect();
        let segment_samples = vec![-1.0; 8_000];
        let captured = captured_segments(
            OffsetVad {
                pending: vec![VadSegment {
                    start: 4_000,
                    samples: segment_samples.clone(),
                }],
                wait_for_reset: false,
            },
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(history.clone()),
            ],
        );

        assert_eq!(captured[0].len(), 4_000 + segment_samples.len());
        assert_eq!(&captured[0][..4_000], &history[..4_000]);
        assert_eq!(&captured[0][4_000..], segment_samples);
    }

    #[test]
    fn second_segment_preroll_stops_at_previous_segment_end() {
        let history: Vec<f32> = (0..20_480).map(|sample| sample as f32).collect();
        let second_samples = vec![-2.0; 4_000];
        let captured = captured_segments(
            OffsetVad {
                pending: vec![
                    VadSegment {
                        start: 4_000,
                        samples: vec![-1.0; 4_000],
                    },
                    VadSegment {
                        start: 16_000,
                        samples: second_samples.clone(),
                    },
                ],
                wait_for_reset: false,
            },
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(history.clone()),
            ],
        );

        assert_eq!(captured.len(), 2);
        assert_eq!(captured[1].len(), 8_000 + second_samples.len());
        assert_eq!(&captured[1][..8_000], &history[8_000..16_000]);
        assert_eq!(&captured[1][8_000..], second_samples);
    }

    #[test]
    fn cancel_then_begin_clears_preroll_history() {
        let segment_samples = vec![-1.0; 1_000];
        let captured = captured_segments(
            OffsetVad {
                pending: vec![VadSegment {
                    start: 3_200,
                    samples: segment_samples.clone(),
                }],
                wait_for_reset: true,
            },
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(vec![1.0; 4_096]),
                EngineCmd::Cancel { gen: 1 },
                EngineCmd::Begin { gen: 2 },
                EngineCmd::Audio(vec![2.0; 4_096]),
            ],
        );

        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].len(), 3_200 + segment_samples.len());
        assert!(captured[0][..3_200].iter().all(|sample| *sample == 2.0));
        assert_eq!(&captured[0][3_200..], segment_samples);
    }

    fn run(
        every: usize,
        fail_reset: Arc<AtomicBool>,
        factory_calls: Arc<AtomicUsize>,
        commands: impl IntoIterator<Item = EngineCmd>,
    ) -> Receiver<EngineMsg> {
        let (sink, cmds) = engine::channel();
        for command in commands {
            sink.send(command).unwrap();
        }
        sink.send(EngineCmd::Shutdown).unwrap();
        let (events_tx, events_rx) = engine::event_channel(8);
        let factory_fail_reset = Arc::clone(&fail_reset);
        run_local_worker(
            cmds,
            events_tx,
            sink,
            Box::new(FakeVad::new(every, fail_reset)),
            Box::new(FakeRecognizer),
            12.0,
            Box::new(move || {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(FakeVad::new(
                    every,
                    Arc::clone(&factory_fail_reset),
                )))
            }),
        )
        .unwrap();
        events_rx
    }

    fn run_with_fakes(
        vad: Box<dyn Vad>,
        recognizer: Box<dyn Recognizer>,
        commands: impl IntoIterator<Item = EngineCmd>,
    ) -> Receiver<EngineMsg> {
        let (sink, cmds) = engine::channel();
        for command in commands {
            sink.send(command).unwrap();
        }
        sink.send(EngineCmd::Shutdown).unwrap();
        let (events_tx, events_rx) = engine::event_channel(8);
        run_local_worker(
            cmds,
            events_tx,
            sink,
            vad,
            recognizer,
            12.0,
            Box::new(|| Ok(Box::new(FakeVad::new(1, Arc::new(AtomicBool::new(false)))))),
        )
        .unwrap();
        events_rx
    }

    fn final_text_and_gen(events: &Receiver<EngineMsg>) -> (u64, String) {
        match events.recv().unwrap() {
            EngineMsg::Final { gen, text, .. } => (gen, text),
            message => panic!("expected final, got {message:?}"),
        }
    }

    #[test]
    fn segments_during_audio_are_emitted_as_finals_before_end() {
        let events = run(
            1,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicUsize::new(0)),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(vec![0.5; 512 * 2]),
                EngineCmd::Audio(vec![0.5; 512]),
                EngineCmd::End { gen: 1, pad_ms: 0 },
            ],
        );
        assert_eq!(final_text_and_gen(&events), (1, "seg".into()));
        assert_eq!(final_text_and_gen(&events), (1, "seg".into()));
        assert_eq!(final_text_and_gen(&events), (1, "seg".into()));
        assert_eq!(final_text_and_gen(&events), (1, String::new()));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn three_segments_produce_three_finals_and_one_empty_end_final() {
        let events = run(
            1,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicUsize::new(0)),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(vec![0.5; 512 * 3]),
                EngineCmd::End { gen: 1, pad_ms: 0 },
            ],
        );
        assert_eq!(final_text_and_gen(&events), (1, "seg".into()));
        assert_eq!(final_text_and_gen(&events), (1, "seg".into()));
        assert_eq!(final_text_and_gen(&events), (1, "seg".into()));
        assert_eq!(final_text_and_gen(&events), (1, String::new()));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn end_final_contains_only_trailing_segments() {
        let events = run_with_fakes(
            Box::new(FlushVad {
                inner: FakeVad::new(1, Arc::new(AtomicBool::new(false))),
            }),
            Box::new(FakeRecognizer),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(vec![0.5; 512]),
                EngineCmd::End { gen: 1, pad_ms: 0 },
            ],
        );
        assert_eq!(final_text_and_gen(&events), (1, "seg".into()));
        assert_eq!(final_text_and_gen(&events), (1, "seg".into()));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn empty_recognizer_result_mid_utterance_is_not_emitted() {
        let events = run_with_fakes(
            Box::new(FakeVad::new(1, Arc::new(AtomicBool::new(false)))),
            Box::new(EmptyRecognizer),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(vec![0.5; 512]),
                EngineCmd::End { gen: 1, pad_ms: 0 },
            ],
        );
        assert_eq!(final_text_and_gen(&events), (1, String::new()));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn zero_segments_produce_one_empty_final() {
        let events = run(
            10,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicUsize::new(0)),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(vec![0.0; 512]),
                EngineCmd::End { gen: 1, pad_ms: 0 },
            ],
        );
        assert_eq!(final_text_and_gen(&events), (1, String::new()));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn cancel_then_begin_keeps_only_second_utterance() {
        let events = run(
            1,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicUsize::new(0)),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::Audio(vec![0.1; 256]),
                EngineCmd::Cancel { gen: 1 },
                EngineCmd::Begin { gen: 2 },
                EngineCmd::Audio(vec![0.2; 512 * 2]),
                EngineCmd::End { gen: 2, pad_ms: 0 },
            ],
        );
        assert_eq!(final_text_and_gen(&events), (2, "seg".into()));
        assert_eq!(final_text_and_gen(&events), (2, "seg".into()));
        assert_eq!(final_text_and_gen(&events), (2, String::new()));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn vad_factory_runs_only_after_reset_failure() {
        let normal_calls = Arc::new(AtomicUsize::new(0));
        let events = run(
            1,
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&normal_calls),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::End { gen: 1, pad_ms: 0 },
            ],
        );
        let _ = final_text_and_gen(&events);
        assert_eq!(normal_calls.load(Ordering::SeqCst), 0);

        let failed_calls = Arc::new(AtomicUsize::new(0));
        let events = run(
            1,
            Arc::new(AtomicBool::new(true)),
            Arc::clone(&failed_calls),
            [
                EngineCmd::Begin { gen: 1 },
                EngineCmd::End { gen: 1, pad_ms: 0 },
            ],
        );
        let _ = final_text_and_gen(&events);
        assert_eq!(failed_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn every_model_load_site_reports_a_structured_model_id() {
        fn failure_reason(
            commands: Vec<EngineCmd>,
            make_vad: impl FnMut() -> anyhow::Result<Box<dyn Vad>> + Send + 'static,
            make_asr: impl FnMut() -> anyhow::Result<Box<dyn Recognizer>> + Send + 'static,
        ) -> (String, Option<&'static str>) {
            let (sink, cmds) = engine::channel();
            for command in commands {
                sink.send(command).unwrap();
            }
            let (events_tx, events_rx) = engine::event_channel(8);
            let worker_events = events_tx.clone();
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            kikigaki_core::engine::run_worker(
                &events_tx,
                &done_tx,
                AssertUnwindSafe(|| {
                    let result =
                        run_with_loaders(cmds, worker_events, sink, 12.0, make_vad, make_asr);
                    report_model_load_failure(&events_tx, result)
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

        let initial_vad = failure_reason(
            Vec::new(),
            || anyhow::bail!("bad initial VAD"),
            || Ok(Box::new(FakeRecognizer)),
        );
        assert!(initial_vad.0.contains("bad initial VAD"), "{initial_vad:?}");
        assert_eq!(initial_vad.1, Some(VAD_MODEL_ID));

        let recognizer = failure_reason(
            Vec::new(),
            || Ok(Box::new(FakeVad::new(1, Arc::new(AtomicBool::new(false))))),
            || anyhow::bail!("bad recognizer"),
        );
        assert!(recognizer.0.contains("bad recognizer"), "{recognizer:?}");
        assert_eq!(recognizer.1, Some(ASR_MODEL_ID));

        let vad_loads = Arc::new(AtomicUsize::new(0));
        let rebuild_loads = Arc::clone(&vad_loads);
        let rebuild = failure_reason(
            vec![
                EngineCmd::Begin { gen: 7 },
                EngineCmd::End { gen: 7, pad_ms: 0 },
            ],
            move || {
                if rebuild_loads.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(Box::new(FakeVad::new(1, Arc::new(AtomicBool::new(true)))))
                } else {
                    anyhow::bail!("bad rebuilt VAD")
                }
            },
            || Ok(Box::new(FakeRecognizer)),
        );
        assert!(rebuild.0.contains("bad rebuilt VAD"), "{rebuild:?}");
        assert_eq!(rebuild.1, Some(VAD_MODEL_ID));
    }
}
