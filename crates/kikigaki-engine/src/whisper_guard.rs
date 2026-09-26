//! Pure post-decode guards and parameter selection for the Whisper utterance-mode engine.
//! No dependency on whisper-rs: these are ordinary string/duration functions, unit-tested
//! without a model or the `whisper` feature.

/// Returns the `audio_ctx` value to pass to whisper.cpp for a clip of this duration, per the
/// design's PoC finding that `audio_ctx=750` silently truncates tails past ~12-15 s.
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
}
