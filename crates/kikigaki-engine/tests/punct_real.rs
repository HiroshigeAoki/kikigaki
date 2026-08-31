#![cfg(feature = "punct")]

use std::path::PathBuf;

use kikigaki_core::postprocess::Punctuator;
use kikigaki_core::punct::Thresholds;
use kikigaki_engine::punct::MojicastPunctuator;

#[test]
fn mojicast_measured_goldens_and_input_sensitivity() {
    let Some(dir) = std::env::var_os("KIKIGAKI_MODELS_DIR") else {
        eprintln!("SKIPPED: models missing (KIKIGAKI_MODELS_DIR unset)");
        return;
    };
    let mut punctuator =
        MojicastPunctuator::new(PathBuf::from(dir), Thresholds::default(), 4).unwrap();

    let first = "これはテスト文ですこの機械が日本語をちゃんと聞き取れているかどうかを計ります今日は天気がいいので散歩に行きませんか";
    assert_eq!(
        punctuator.punctuate(first).unwrap(),
        "これはテスト文です。この機械が日本語をちゃんと聞き取れているかどうかを計ります。今日は天気がいいので散歩に行きませんか。"
    );
    let period_count = punctuator
        .probabilities(first)
        .unwrap()
        .iter()
        .filter(|(_, period)| *period >= 0.5)
        .count();
    assert_eq!(period_count, 3);

    assert_eq!(
        punctuator
            .punctuate("日本語ちゃんと聞き取れてますかちゃんと聞こえてんのちゃんと聞こえてるのじゃあマイクを持ってもらって")
            .unwrap(),
        "日本語ちゃんと聞き取れてますか？ちゃんと聞こえてんの？ちゃんと聞こえてるのじゃあマイクを持ってもらって。"
    );
}
