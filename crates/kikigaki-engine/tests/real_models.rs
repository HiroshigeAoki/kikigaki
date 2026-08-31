mod common;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use kikigaki_core::config::{AsrConfig, VadConfig};
use kikigaki_core::engine::{Engine, EngineCmd, EngineMsg};
use kikigaki_engine::local::{LocalEngine, Recognizer, Vad};
use kikigaki_engine::sherpa::{build_recognizer, build_vad};
use sherpa_onnx::LinearResampler;

use common::load_wav_16k;

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
