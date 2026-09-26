//! Pure post-decode guards and parameter selection for the Whisper utterance-mode engine.
//! No dependency on whisper-rs: these are ordinary string/duration functions, unit-tested
//! without a model or the `whisper` feature.

/// Returns the `audio_ctx` value to pass to whisper.cpp for a clip of this duration. The reduced
/// context speeds up short clips but can silently truncate the tail of clips longer than ~12-15 s.
///
/// `None` means "full context" (no audio_ctx override).
pub fn audio_ctx_for(duration_ms: u64) -> Option<i32> {
    const SHORT_CLIP_AUDIO_CTX: i32 = 750;
    const DURATION_GUARD_MS: u64 = 10_000;
    if duration_ms <= DURATION_GUARD_MS {
        Some(SHORT_CLIP_AUDIO_CTX)
    } else {
        None
    }
}

/// Known stock hallucinations whisper.cpp emits on silence/near-silence input.
const STOCK_HALLUCINATIONS: &[&str] = &[
    "ご視聴ありがとうございました。",
    "ご視聴ありがとうございました",
];

/// Drops output that exactly matches a known stock hallucination.
pub fn drop_stock_hallucination(text: &str) -> &str {
    if STOCK_HALLUCINATIONS.contains(&text.trim()) {
        ""
    } else {
        text
    }
}

/// Collapses an exact immediate repetition of the whole decoded text, a failure mode observed
/// during manual Whisper testing on macOS.
pub fn collapse_immediate_repetition(text: &str) -> &str {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return text;
    }
    let half = trimmed.len() / 2;
    if trimmed.len().is_multiple_of(2) && trimmed.is_char_boundary(half) {
        let (first, second) = trimmed.split_at(half);
        if !first.is_empty() && first == second {
            return &trimmed[..half];
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_clips_use_the_fixed_audio_ctx() {
        assert_eq!(audio_ctx_for(0), Some(750));
        assert_eq!(audio_ctx_for(9_999), Some(750));
        assert_eq!(audio_ctx_for(10_000), Some(750));
    }

    #[test]
    fn clips_over_ten_seconds_use_full_context() {
        assert_eq!(audio_ctx_for(10_001), None);
        assert_eq!(audio_ctx_for(25_000), None);
    }

    #[test]
    fn stock_hallucination_on_silence_is_dropped() {
        assert_eq!(
            drop_stock_hallucination("ご視聴ありがとうございました。"),
            ""
        );
        assert_eq!(drop_stock_hallucination("ご視聴ありがとうございました"), "");
        assert_eq!(
            drop_stock_hallucination("これは違う内容です"),
            "これは違う内容です"
        );
    }

    #[test]
    fn exact_immediate_repetition_is_collapsed() {
        assert_eq!(
            collapse_immediate_repetition("マイクを持ってもらってマイクを持ってもらって"),
            "マイクを持ってもらって"
        );
        assert_eq!(
            collapse_immediate_repetition("マイクを持ってもらって"),
            "マイクを持ってもらって"
        );
        assert_eq!(collapse_immediate_repetition(""), "");
    }

    #[test]
    fn mixed_width_text_with_non_boundary_byte_half_is_unchanged() {
        assert_eq!(collapse_immediate_repetition("あa"), "あa");
        assert_eq!(collapse_immediate_repetition("aあ"), "aあ");
        assert_eq!(collapse_immediate_repetition("あいa"), "あいa");
    }
}
