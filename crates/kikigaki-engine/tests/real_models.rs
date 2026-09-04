mod common;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kikigaki_core::config::{AsrConfig, VadConfig};
use kikigaki_core::engine::{Engine, EngineCmd, EngineMsg};
use kikigaki_engine::hotwords::materialize;
use kikigaki_engine::local::{LocalEngine, Recognizer, Vad};
use kikigaki_engine::sherpa::{build_recognizer, build_recognizer_auto, build_vad};
use sha2::{Digest, Sha256};
use sherpa_onnx::LinearResampler;

use common::load_wav_16k;

#[test]
fn materialized_hotwords_match_real_model_tokens() {
    let Some(dir) = std::env::var_os("KIKIGAKI_MODELS_DIR") else {
        eprintln!("SKIPPED: models missing (KIKIGAKI_MODELS_DIR unset)");
        return;
    };
    let setup = materialize(&PathBuf::from(dir)).unwrap();
    let vocab = std::fs::read(&setup.bpe_vocab).unwrap();
    // Tied to the current ASR_MODEL_ID tokens.txt. Model updates must regenerate this value and
    // rerun the hotword evaluation harness against the new tokenization.
    let expected_vocab_sha256 = "ec46034586fa6e35f317af744273adca09c95ef7ad699b9a723d98548eba6f09";
    assert_eq!(hex::encode(Sha256::digest(vocab)), expected_vocab_sha256);

    let materialized = std::fs::read_to_string(setup.hotwords_file).unwrap();
    let embedded = include_str!("../data/hotwords.txt");
    assert_eq!(materialized.lines().count(), embedded.lines().count());
}

#[test]
fn transcribe_test_ja_1_golden() {
    let Some(dir) = std::env::var_os("KIKIGAKI_MODELS_DIR") else {
        eprintln!("SKIPPED: models missing (KIKIGAKI_MODELS_DIR unset)");
        return;
    };
    let samples = load_wav_16k("test_ja_1.wav");
    let mut recognizer = build_recognizer(&PathBuf::from(dir), &AsrConfig::default()).unwrap();
    let actual = recognizer
        .transcribe(&samples)
        .expect("recognizer returned no result");
    // Golden measured 2026-08-30 on dgx-1 through the product path (48 kHz fixture →
    // `kikigaki_core::audio::StreamResampler` → int8 transducer, modified_beam_search). The
    // upstream reference transcript is `日本語ちゃんと聞き取れてますかちゃんと聞こえてんの
    // ちゃんと聞こえてるのじゃあマイクを持ってもらって`; feeding sherpa's own resampler yields
    // `ちゃんと聞こえてんのちゃんと聞こえてんのでマイクを持ってもらって` (Task 0 result). Both
    // resamplers agree to >30 dB SNR (see `core_resampler_matches_sherpa_resampler`), so
    // the difference is decoder sensitivity on this clip, not a resampling defect.
    let expected = "日本語ちゃんと聞いといマイクを持ってもらって";
    assert_eq!(
        actual, expected,
        "golden mismatch; expected={expected:?}, actual={actual:?}"
    );
}

#[test]
fn production_hotword_path_biases_known_eval_wav() {
    let Some(models_dir) = real_models_dir() else {
        return;
    };
    let models_dir = std::fs::canonicalize(models_dir).expect("canonicalize real models directory");
    let Some(samples) = optional_wav_16k(&models_dir, "t09.wav") else {
        return;
    };
    let asr = AsrConfig {
        num_threads: 4,
        ..AsrConfig::default()
    };

    let mut baseline = build_recognizer(&models_dir, &asr).unwrap();
    let baseline_text = baseline
        .transcribe(&samples)
        .expect("baseline recognizer returned no result");
    drop(baseline);
    let mut boosted = build_recognizer_auto(&models_dir, &asr, Some(3.0)).unwrap();
    let boosted_text = boosted
        .transcribe(&samples)
        .expect("hotword recognizer returned no result");

    assert!(
        boosted_text.contains("ドッカー"),
        "boosted decode did not contain the expected reading; baseline={baseline_text:?}, boosted={boosted_text:?}"
    );
    assert_ne!(
        boosted_text, baseline_text,
        "known biasing fixture did not differ from baseline"
    );
}

#[cfg(unix)]
#[test]
fn unwritable_hotword_directory_degrades_and_local_engine_decodes() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let Some(models_dir) = real_models_dir() else {
        return;
    };
    let models_dir = std::fs::canonicalize(models_dir).expect("canonicalize real models directory");
    let Some(samples) = optional_wav_16k(&models_dir, "t09.wav") else {
        return;
    };
    let isolated = tempfile::tempdir().unwrap();
    symlink(
        models_dir.join(kikigaki_core::models::ASR_MODEL_ID),
        isolated.path().join(kikigaki_core::models::ASR_MODEL_ID),
    )
    .unwrap();
    symlink(
        models_dir.join(kikigaki_core::models::VAD_MODEL_ID),
        isolated.path().join(kikigaki_core::models::VAD_MODEL_ID),
    )
    .unwrap();
    let hotwords_dir = isolated.path().join("hotwords");
    std::fs::create_dir(&hotwords_dir).unwrap();
    std::fs::set_permissions(&hotwords_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let _permissions = RestoreDirectoryPermissions(hotwords_dir.clone());
    let probe = hotwords_dir.join("write-probe");
    if std::fs::write(&probe, b"probe").is_ok() {
        std::fs::remove_file(probe).unwrap();
        eprintln!("SKIPPED: current user can write through mode 0555");
        return;
    }

    let messages = Arc::new(Mutex::new(Vec::new()));
    let subscriber = WarningSubscriber(Arc::clone(&messages));
    let text = tracing::subscriber::with_default(subscriber, || {
        let asr = AsrConfig {
            num_threads: 4,
            ..AsrConfig::default()
        };
        let mut engine = LocalEngine::start(
            isolated.path().to_path_buf(),
            asr,
            VadConfig::default(),
            Some(3.0),
        )
        .unwrap();
        assert!(matches!(
            engine
                .events()
                .recv_timeout(Duration::from_secs(30))
                .unwrap(),
            EngineMsg::Ready
        ));
        let sink = engine.sink();
        sink.send(EngineCmd::Begin { gen: 1 }).unwrap();
        for chunk in samples.chunks(320) {
            sink.send(EngineCmd::Audio(chunk.to_vec())).unwrap();
        }
        sink.send(EngineCmd::End {
            gen: 1,
            pad_ms: 500,
        })
        .unwrap();
        let text = collect_final_text(engine.events(), 1);
        Box::new(engine).shutdown();
        text
    });

    assert!(!text.is_empty(), "degraded baseline decode was empty");
    let warning = messages.lock().unwrap().join("\n");
    assert!(warning.contains("hotword setup failed"), "{warning:?}");
    assert!(warning.contains("without hotwords"), "{warning:?}");
}

#[test]
fn vad_segments_on_test_ja_2() {
    let Some(dir) = std::env::var_os("KIKIGAKI_MODELS_DIR") else {
        eprintln!("SKIPPED: models missing (KIKIGAKI_MODELS_DIR unset)");
        return;
    };
    let samples = load_wav_16k("test_ja_2.wav");
    let mut vad = build_vad(&PathBuf::from(dir), &VadConfig::default()).unwrap();
    for chunk in samples.chunks(512) {
        if chunk.len() == 512 {
            vad.accept(chunk);
        } else {
            let mut frame = vec![0.0; 512];
            frame[..chunk.len()].copy_from_slice(chunk);
            vad.accept(&frame);
        }
    }
    vad.flush();
    let count = vad.drain().unwrap().len();
    assert!(
        (1..=4).contains(&count),
        "unexpected VAD segment count {count}"
    );
}

#[test]
fn local_engine_end_to_end() {
    let Some(dir) = std::env::var_os("KIKIGAKI_MODELS_DIR") else {
        eprintln!("SKIPPED: models missing (KIKIGAKI_MODELS_DIR unset)");
        return;
    };
    let mut engine = LocalEngine::start(
        PathBuf::from(dir),
        AsrConfig::default(),
        VadConfig::default(),
        None,
    )
    .unwrap();
    assert!(matches!(
        engine
            .events()
            .recv_timeout(Duration::from_secs(30))
            .unwrap(),
        EngineMsg::Ready
    ));

    let sink = engine.sink();
    sink.send(EngineCmd::Begin { gen: 1 }).unwrap();
    for chunk in load_wav_16k("test_ja_2.wav").chunks(320) {
        sink.send(EngineCmd::Audio(chunk.to_vec())).unwrap();
    }
    sink.send(EngineCmd::End {
        gen: 1,
        pad_ms: 500,
    })
    .unwrap();

    let text = collect_final_text(engine.events(), 1);
    assert!(!text.is_empty(), "end-to-end transcription was empty");

    sink.send(EngineCmd::Begin { gen: 2 }).unwrap();
    for chunk in load_wav_16k("test_ja_1.wav").chunks(320) {
        sink.send(EngineCmd::Audio(chunk.to_vec())).unwrap();
    }
    sink.send(EngineCmd::End {
        gen: 2,
        pad_ms: 500,
    })
    .unwrap();

    let text = collect_final_text(engine.events(), 2);
    assert!(
        text.contains("マイクを持ってもらって"),
        "onset-clipping regression; transcription was {text:?}"
    );
    assert!(engine.events().try_recv().is_err());
    Box::new(engine).shutdown();
}

fn collect_final_text(events: &Receiver<EngineMsg>, gen: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut text = String::new();
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match events.recv_timeout(remaining) {
            Ok(EngineMsg::Final {
                gen: event_gen,
                text: part,
                ..
            }) if event_gen == gen => text.push_str(&part),
            Ok(EngineMsg::Disconnected { reason, .. }) => {
                panic!("local engine disconnected while awaiting gen {gen}: {reason}")
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                panic!("local engine event channel disconnected while awaiting gen {gen}")
            }
        }
    }
    text
}

fn real_models_dir() -> Option<PathBuf> {
    let Some(dir) = std::env::var_os("KIKIGAKI_MODELS_DIR") else {
        eprintln!("SKIPPED: models missing (KIKIGAKI_MODELS_DIR unset)");
        return None;
    };
    Some(PathBuf::from(dir))
}

fn optional_wav_16k(models_dir: &Path, name: &str) -> Option<Vec<f32>> {
    let wavs_dir = std::env::var_os("KIKIGAKI_TEST_WAVS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            models_dir
                .join(kikigaki_core::models::ASR_MODEL_ID)
                .join("test_wavs")
        });
    let path = wavs_dir.join(name);
    if !path.is_file() {
        eprintln!("SKIPPED: WAV fixture missing ({})", path.display());
        return None;
    }
    Some(
        kikigaki_engine::audio::load_wav_16k(&path)
            .unwrap_or_else(|error| panic!("failed to load WAV {}: {error:#}", path.display())),
    )
}

#[cfg(unix)]
struct RestoreDirectoryPermissions(PathBuf);

#[cfg(unix)]
impl Drop for RestoreDirectoryPermissions {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
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
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
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

/// Guards the core resampler against sherpa's reference windowed-sinc resampler: the two
/// outputs must agree to at least 30 dB SNR (after aligning the core filter's 32-sample
/// group delay) on a real 48 kHz fixture. A plain linear interpolator scores far below this.
#[test]
fn core_resampler_matches_sherpa_resampler() {
    if std::env::var_os("KIKIGAKI_MODELS_DIR").is_none() {
        eprintln!("SKIPPED: models missing (KIKIGAKI_MODELS_DIR unset)");
        return;
    }
    let (raw, sample_rate) = common::load_wav_raw("test_ja_1.wav");
    assert_eq!(sample_rate, 48_000, "fixture is expected to be 48 kHz");
    let ours = kikigaki_core::audio::StreamResampler::new(sample_rate, 16_000).push(&raw);
    let theirs = LinearResampler::create(sample_rate as i32, 16_000)
        .expect("sherpa resampler")
        .resample(&raw, true);
    let best = (-64i64..=64)
        .map(|shift| snr_db(&ours, &theirs, shift))
        .fold(f64::MIN, f64::max);
    assert!(
        best >= 30.0,
        "core resampler deviates from sherpa's: best SNR {best:.1} dB"
    );
}

fn snr_db(a: &[f32], b: &[f32], shift: i64) -> f64 {
    let (mut signal, mut noise) = (0f64, 0f64);
    for (i, &x) in a.iter().enumerate() {
        let j = i as i64 + shift;
        if j < 0 || j >= b.len() as i64 {
            continue;
        }
        let y = b[j as usize];
        signal += f64::from(x) * f64::from(x);
        noise += f64::from(x - y) * f64::from(x - y);
    }
    10.0 * (signal / noise.max(1e-30)).log10()
}
