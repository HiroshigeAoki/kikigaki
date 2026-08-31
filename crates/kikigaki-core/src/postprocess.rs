use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::engine::{join_workers, EngineMsg, SinkError, Waker};
use crate::protocol;
use crate::replace::ReplaceFile;
use crate::text::strip_trailing_period;

/// Adds punctuation to transcription text.
pub trait Punctuator: Send {
    /// Returns punctuated text or an error that leaves the pipeline input unchanged.
    fn punctuate(&mut self, text: &str) -> anyhow::Result<String>;
}

/// Punctuator that returns its input unchanged.
pub struct NoopPunctuator;

impl Punctuator for NoopPunctuator {
    fn punctuate(&mut self, text: &str) -> anyhow::Result<String> {
        Ok(text.to_owned())
    }
}

/// Ordered replacement, punctuation, and trailing-period processing.
pub struct Pipeline {
    /// Hot-reloaded replacement dictionary.
    pub replace: ReplaceFile,
    /// Punctuation implementation owned by this pipeline.
    pub punctuator: Box<dyn Punctuator>,
    /// Whether punctuation is currently enabled.
    pub punct_enabled: bool,
    /// Whether to remove one trailing Japanese or ASCII period.
    pub strip_trailing_period: bool,
    learned: std::sync::Arc<crate::replace::Rules>,
    effective: std::sync::Arc<crate::replace::Rules>,
    effective_replace_generation: u64,
}

/// Text and elapsed time produced by one pipeline run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    /// Fully processed transcription text.
    pub text: String,
    /// Wall-clock pipeline duration in milliseconds.
    pub postprocess_ms: u64,
}

impl Pipeline {
    /// Constructs a pipeline and its initial effective-rule cache.
    pub fn new(
        mut replace: ReplaceFile,
        punctuator: Box<dyn Punctuator>,
        punct_enabled: bool,
        strip_trailing_period: bool,
        learned: std::sync::Arc<crate::replace::Rules>,
    ) -> Self {
        let effective =
            std::sync::Arc::new(crate::replace::Rules::merge(replace.rules(), &learned));
        let effective_replace_generation = replace.generation();
        Self {
            replace,
            punctuator,
            punct_enabled,
            strip_trailing_period,
            learned,
            effective,
            effective_replace_generation,
        }
    }

    /// Replaces the learned overlay and immediately rebuilds the effective-rule cache.
    pub fn set_learned(&mut self, learned: std::sync::Arc<crate::replace::Rules>) {
        self.effective =
            std::sync::Arc::new(crate::replace::Rules::merge(self.replace.rules(), &learned));
        self.effective_replace_generation = self.replace.generation();
        self.learned = learned;
    }

    /// Runs trim, replacement, punctuation, and optional trailing-period removal in order.
    pub fn run(&mut self, raw: &str) -> Processed {
        let started = Instant::now();
        let _ = self.replace.rules();
        if self.replace.generation() != self.effective_replace_generation {
            self.effective = std::sync::Arc::new(crate::replace::Rules::merge(
                self.replace.rules(),
                &self.learned,
            ));
            self.effective_replace_generation = self.replace.generation();
        }
        let replaced = self.effective.apply(raw.trim());
        let punctuated = if self.punct_enabled {
            match self.punctuator.punctuate(&replaced) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(%error, "punctuation failed; keeping unpunctuated text");
                    replaced
                }
            }
        } else {
            replaced
        };
        let text = if self.strip_trailing_period {
            strip_trailing_period(&punctuated)
        } else {
            punctuated
        };
        Processed {
            text,
            postprocess_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        }
    }
}

/// Post-processed final plus engine timing and drop metadata.
#[derive(Debug, Clone)]
pub struct ProcessedFinal {
    /// Session generation assigned to the transcription.
    pub gen: u64,
    /// Pre-replacement transcription text.
    pub raw: String,
    /// Fully processed transcription text.
    pub text: String,
    /// Wall-clock pipeline duration in milliseconds.
    pub postprocess_ms: u64,
    /// Engine-reported processing latency in milliseconds, when available.
    pub engine_latency_ms: Option<f64>,
    /// Time at which voice activity detection closed.
    pub vad_close_at: Instant,
    /// Time at which speech recognition finished.
    pub asr_end_at: Instant,
    /// Audio chunks dropped for this generation.
    pub dropped_chunks: u64,
}

enum Work {
    Final {
        gen: u64,
        msg: EngineMsg,
    },
    Configure {
        punct_enabled: bool,
        strip_trailing_period: bool,
    },
    ReloadRules(std::sync::Arc<crate::replace::Rules>),
    Shutdown,
}

/// Dedicated ordered worker that owns a post-processing pipeline.
pub struct PostprocessWorker {
    tx: Option<Sender<Work>>,
    rx: Receiver<ProcessedFinal>,
    done_rx: Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl PostprocessWorker {
    /// Spawns the dedicated pipeline thread.
    pub fn spawn(pipeline: Pipeline) -> Self {
        Self::spawn_with_waker(pipeline, None)
    }

    /// Spawns the dedicated pipeline thread with an optional result-ready callback.
    pub fn spawn_with_waker(mut pipeline: Pipeline, waker: Option<Waker>) -> Self {
        let (tx, work_rx) = mpsc::channel();
        let (result_tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("postprocess".into())
            .spawn(move || {
                while let Ok(work) = work_rx.recv() {
                    match work {
                        Work::Final {
                            gen,
                            msg:
                                EngineMsg::Final {
                                    text,
                                    engine_latency_ms,
                                    vad_close_at,
                                    asr_end_at,
                                    dropped_chunks,
                                    ..
                                },
                        } => {
                            let processed = pipeline.run(&text);
                            if result_tx
                                .send(ProcessedFinal {
                                    gen,
                                    raw: text,
                                    text: processed.text,
                                    postprocess_ms: processed.postprocess_ms,
                                    engine_latency_ms,
                                    vad_close_at,
                                    asr_end_at,
                                    dropped_chunks,
                                })
                                .is_err()
                            {
                                break;
                            }
                            if let Some(waker) = waker.as_ref() {
                                waker();
                            }
                        }
                        Work::Final { .. } => {
                            tracing::warn!("post-processing worker received a non-final event");
                        }
                        Work::Configure {
                            punct_enabled,
                            strip_trailing_period,
                        } => {
                            pipeline.punct_enabled = punct_enabled;
                            pipeline.strip_trailing_period = strip_trailing_period;
                        }
                        Work::ReloadRules(rules) => pipeline.set_learned(rules),
                        Work::Shutdown => break,
                    }
                }
                let _ = done_tx.send(());
            })
            .expect("spawn post-processing worker");
        Self {
            tx: Some(tx),
            rx,
            done_rx,
            worker: Some(worker),
        }
    }

    /// Submits one engine final for ordered post-processing.
    pub fn submit(&self, gen: u64, msg: EngineMsg) -> Result<(), SinkError> {
        self.tx
            .as_ref()
            .ok_or(SinkError::Closed)?
            .send(Work::Final { gen, msg })
            .map_err(|_| SinkError::Closed)
    }

    /// Applies runtime punctuation settings before subsequently queued finals.
    pub fn configure(
        &self,
        punct_enabled: bool,
        strip_trailing_period: bool,
    ) -> Result<(), SinkError> {
        self.tx
            .as_ref()
            .ok_or(SinkError::Closed)?
            .send(Work::Configure {
                punct_enabled,
                strip_trailing_period,
            })
            .map_err(|_| SinkError::Closed)
    }

    /// Replaces the learned-rule overlay before subsequently queued finals.
    pub fn reload_rules(
        &self,
        rules: std::sync::Arc<crate::replace::Rules>,
    ) -> Result<(), SinkError> {
        self.tx
            .as_ref()
            .ok_or(SinkError::Closed)?
            .send(Work::ReloadRules(rules))
            .map_err(|_| SinkError::Closed)
    }

    /// Attempts to receive the next processed final without blocking.
    pub fn try_recv(&self) -> Option<ProcessedFinal> {
        self.rx.try_recv().ok()
    }
}

impl Drop for PostprocessWorker {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Work::Shutdown);
        }
        join_workers(
            [(&mut self.worker, "post-processing worker")],
            &self.done_rx,
            "post-processing worker",
        );
    }
}

/// Converts processed engine output into the protocol final consumed by the session.
pub fn to_protocol_final(processed: &ProcessedFinal) -> protocol::Final {
    protocol::Final {
        gen: processed.gen,
        text: processed.text.clone(),
        lang: String::new(),
        engine_latency_ms: processed.engine_latency_ms,
        dropped_chunks: processed.dropped_chunks,
        vad_close_to_asr_end_ms: Some(
            processed
                .asr_end_at
                .saturating_duration_since(processed.vad_close_at)
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        ),
        postprocess_ms: Some(processed.postprocess_ms),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::bail;

    use super::*;

    struct FailingPunctuator;

    impl Punctuator for FailingPunctuator {
        fn punctuate(&mut self, _text: &str) -> anyhow::Result<String> {
            bail!("punctuation failed")
        }
    }

    struct RecordingPunctuator(Arc<Mutex<Vec<String>>>);

    impl Punctuator for RecordingPunctuator {
        fn punctuate(&mut self, text: &str) -> anyhow::Result<String> {
            self.0.lock().unwrap().push(text.to_owned());
            Ok(format!("{text}。"))
        }
    }

    fn replace_file(body: &str) -> (tempfile::TempDir, ReplaceFile) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("replace.toml");
        std::fs::write(&path, body).unwrap();
        (temp, ReplaceFile::new(path))
    }

    fn pipeline(punctuator: Box<dyn Punctuator>, strip: bool) -> (tempfile::TempDir, Pipeline) {
        let (temp, replace) = replace_file("");
        (
            temp,
            Pipeline::new(
                replace,
                punctuator,
                true,
                strip,
                std::sync::Arc::new(crate::replace::Rules::default()),
            ),
        )
    }

    fn final_msg(gen: u64, text: &str) -> EngineMsg {
        let vad_close_at = Instant::now();
        EngineMsg::Final {
            gen,
            text: text.into(),
            engine_latency_ms: Some(12.5),
            vad_close_at,
            asr_end_at: vad_close_at + Duration::from_millis(7),
            dropped_chunks: gen,
        }
    }

    fn recv(worker: &PostprocessWorker) -> ProcessedFinal {
        for _ in 0..100 {
            if let Some(final_) = worker.try_recv() {
                return final_;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("post-processing result not received")
    }

    #[test]
    fn trims_whitespace_and_strips_one_trailing_period() {
        let (_temp, mut pipeline) = pipeline(Box::new(NoopPunctuator), true);
        assert_eq!(pipeline.run("  a。。  ").text, "a。");
        assert_eq!(pipeline.run("  b..  ").text, "b.");
    }

    #[test]
    fn keeps_trailing_period_when_disabled() {
        let (_temp, mut pipeline) = pipeline(Box::new(NoopPunctuator), false);
        assert_eq!(pipeline.run("  a。  ").text, "a。");
    }

    #[test]
    fn failing_punctuator_leaves_text_intact() {
        let (_temp, mut pipeline) = pipeline(Box::new(FailingPunctuator), false);
        assert_eq!(pipeline.run("  intact  ").text, "intact");
    }

    #[test]
    fn replacement_runs_before_punctuation() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (_temp, replace) = replace_file("[[rule]]\nfrom = [\"raw\"]\nto = \"replaced\"\n");
        let mut pipeline = Pipeline::new(
            replace,
            Box::new(RecordingPunctuator(Arc::clone(&seen))),
            true,
            true,
            std::sync::Arc::new(crate::replace::Rules::default()),
        );
        assert_eq!(pipeline.run("raw").text, "replaced");
        assert_eq!(*seen.lock().unwrap(), ["replaced"]);
    }

    #[test]
    fn worker_preserves_final_order_and_fields() {
        let (_temp, pipeline) = pipeline(Box::new(NoopPunctuator), false);
        let worker = PostprocessWorker::spawn(pipeline);
        worker.submit(1, final_msg(1, "first")).unwrap();
        worker.submit(2, final_msg(2, "second")).unwrap();
        let first = recv(&worker);
        let second = recv(&worker);
        assert_eq!((first.gen, first.text.as_str()), (1, "first"));
        assert_eq!((second.gen, second.text.as_str()), (2, "second"));
        assert_eq!(first.engine_latency_ms, Some(12.5));
        assert_eq!(first.dropped_chunks, 1);
    }

    #[test]
    fn worker_wakes_once_per_processed_result() {
        let (_temp, pipeline) = pipeline(Box::new(NoopPunctuator), false);
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let worker = PostprocessWorker::spawn_with_waker(
            pipeline,
            Some(Arc::new(move || {
                wake_count.fetch_add(1, Ordering::Relaxed);
            })),
        );

        worker.submit(1, final_msg(1, "first")).unwrap();
        worker.submit(2, final_msg(2, "second")).unwrap();
        recv(&worker);
        recv(&worker);

        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn worker_keeps_text_when_punctuation_fails() {
        let (_temp, pipeline) = pipeline(Box::new(FailingPunctuator), false);
        let worker = PostprocessWorker::spawn(pipeline);
        worker.submit(9, final_msg(9, "intact")).unwrap();
        assert_eq!(recv(&worker).text, "intact");
    }

    #[test]
    fn configure_toggles_punctuation_and_strip_without_rebuilding_pipeline() {
        let (_temp, pipeline) = pipeline(
            Box::new(RecordingPunctuator(Arc::new(Mutex::new(Vec::new())))),
            false,
        );
        let worker = PostprocessWorker::spawn(pipeline);
        worker.submit(1, final_msg(1, "one")).unwrap();
        assert_eq!(recv(&worker).text, "one。");

        worker.configure(false, false).unwrap();
        worker.submit(2, final_msg(2, "two")).unwrap();
        assert_eq!(recv(&worker).text, "two");

        worker.configure(true, true).unwrap();
        worker.submit(3, final_msg(3, "three")).unwrap();
        assert_eq!(recv(&worker).text, "three");
    }

    #[test]
    fn effective_rules_are_cached_across_finals_and_rebuilt_only_after_reload() {
        let (_temp, replace) = replace_file("[[rule]]\nfrom = [\"a\"]\nto = \"b\"\n");
        let learned_rules: Vec<crate::replace::Rule> = (0..500)
            .map(|i| crate::replace::Rule {
                from: vec![format!("w{i}")],
                to: format!("r{i}"),
            })
            .collect();
        let learned = std::sync::Arc::new(
            crate::replace::Rules::from_learned_checked(learned_rules).unwrap(),
        );
        let mut pipeline = Pipeline::new(replace, Box::new(NoopPunctuator), true, false, learned);
        pipeline.run("a");
        let cached = std::sync::Arc::clone(&pipeline.effective);
        for _ in 0..50 {
            pipeline.run("a");
        }
        assert!(std::sync::Arc::ptr_eq(&cached, &pipeline.effective));
        pipeline.set_learned(std::sync::Arc::new(crate::replace::Rules::default()));
        assert!(!std::sync::Arc::ptr_eq(&cached, &pipeline.effective));
    }

    #[test]
    fn reload_rules_replaces_learned_rules_and_takes_effect_on_the_next_final() {
        let (_temp, mut pipeline) = pipeline(Box::new(NoopPunctuator), false);
        pipeline.set_learned(std::sync::Arc::new(
            crate::replace::Rules::parse("[[rule]]\nfrom = [\"x\"]\nto = \"y\"\n").unwrap(),
        ));
        let worker = PostprocessWorker::spawn(pipeline);
        worker.submit(1, final_msg(1, "x")).unwrap();
        assert_eq!(recv(&worker).text, "y");
        worker
            .reload_rules(std::sync::Arc::new(
                crate::replace::Rules::parse("[[rule]]\nfrom = [\"x\"]\nto = \"z\"\n").unwrap(),
            ))
            .unwrap();
        worker.submit(2, final_msg(2, "x")).unwrap();
        assert_eq!(recv(&worker).text, "z");
    }

    #[test]
    fn converts_processed_final_to_protocol_fields() {
        let vad_close_at = Instant::now();
        let processed = ProcessedFinal {
            gen: 4,
            raw: "raw text".into(),
            text: "done".into(),
            postprocess_ms: 3,
            engine_latency_ms: Some(10.0),
            vad_close_at,
            asr_end_at: vad_close_at + Duration::from_millis(8),
            dropped_chunks: 2,
        };
        assert_eq!(
            to_protocol_final(&processed),
            crate::protocol::Final {
                gen: 4,
                text: "done".into(),
                lang: String::new(),
                engine_latency_ms: Some(10.0),
                dropped_chunks: 2,
                vad_close_to_asr_end_ms: Some(8),
                postprocess_ms: Some(3),
            }
        );
    }
}
