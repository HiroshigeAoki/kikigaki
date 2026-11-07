//! Pure post-decode guards and parameter selection for the Whisper utterance-mode engine.
//! No dependency on whisper-rs: these are ordinary string/duration functions, unit-tested
//! without a model or the `whisper` feature.

use std::fmt;
use std::str::FromStr;

/// Selects the Whisper encoder context used for an utterance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum AudioCtxPolicy {
    /// Preserve the app's short-clip optimization.
    #[default]
    Default,
    /// Always use Whisper's full context.
    Full,
    /// Use a fixed context for clips up to ten seconds.
    Fixed(i32),
    /// Scale context with clip duration and clamp it to a minimum and Whisper's maximum.
    Scaled {
        /// Padding added to the clip duration before scaling.
        pad_ms: u64,
        /// Minimum number of encoder positions.
        min: i32,
    },
}

impl AudioCtxPolicy {
    /// Returns the `audio_ctx` override selected for a clip duration.
    pub fn audio_ctx(self, duration_ms: u64) -> Option<i32> {
        const DURATION_GUARD_MS: u64 = 10_000;
        if duration_ms > DURATION_GUARD_MS {
            return None;
        }
        match self {
            Self::Default => audio_ctx_for(duration_ms),
            Self::Full => None,
            Self::Fixed(value) => Some(value),
            Self::Scaled { pad_ms, min } => {
                let milliseconds = u128::from(duration_ms) + u128::from(pad_ms);
                let positions = milliseconds.saturating_mul(50).div_ceil(1_000);
                let positions = i32::try_from(positions).unwrap_or(i32::MAX);
                Some(positions.max(min).min(1_500))
            }
        }
    }
}

impl FromStr for AudioCtxPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "default" => Ok(Self::Default),
            "full" => Ok(Self::Full),
            "scaled" => Ok(Self::Scaled {
                pad_ms: 1_000,
                min: 128,
            }),
            _ => {
                if let Some(arguments) = value.strip_prefix("scaled:") {
                    let mut parts = arguments.split(':');
                    let pad = parts.next().unwrap_or_default();
                    let min = parts.next().unwrap_or_default();
                    if parts.next().is_some() || pad.is_empty() || min.is_empty() {
                        return Err(format!(
                            "invalid audio context policy {value:?}; expected scaled:<pad_ms>:<min>"
                        ));
                    }
                    let pad_ms = pad.parse::<u64>().map_err(|_| {
                        format!("invalid scaled audio context padding {pad:?}; expected u64")
                    })?;
                    let min = parse_context_size(min, "scaled minimum")?;
                    return Ok(Self::Scaled { pad_ms, min });
                }
                value.parse::<i32>().map(Self::Fixed).map_err(|_| {
                    format!(
                        "invalid audio context policy {value:?}; expected default, full, 1..=1500, scaled, or scaled:<pad_ms>:<min>"
                    )
                }).and_then(|policy| match policy {
                    Self::Fixed(size) if (1..=1_500).contains(&size) => Ok(policy),
                    Self::Fixed(_) => Err(format!("audio context size {value:?} must be in 1..=1500")),
                    _ => unreachable!(),
                })
            }
        }
    }
}

fn parse_context_size(value: &str, label: &str) -> Result<i32, String> {
    let size = value
        .parse::<i32>()
        .map_err(|_| format!("invalid {label} {value:?}; expected an integer in 1..=1500"))?;
    if (1..=1_500).contains(&size) {
        Ok(size)
    } else {
        Err(format!("{label} {value:?} must be in 1..=1500"))
    }
}

impl fmt::Display for AudioCtxPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => formatter.write_str("default"),
            Self::Full => formatter.write_str("full"),
            Self::Fixed(value) => write!(formatter, "{value}"),
            Self::Scaled { pad_ms, min } => write!(formatter, "scaled:{pad_ms}:{min}"),
        }
    }
}

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
    fn audio_ctx_policy_selects_each_context_strategy() {
        assert_eq!(AudioCtxPolicy::default(), AudioCtxPolicy::Default);
        assert_eq!(AudioCtxPolicy::Default.audio_ctx(10_000), Some(750));
        assert_eq!(AudioCtxPolicy::Full.audio_ctx(0), None);
        assert_eq!(AudioCtxPolicy::Fixed(512).audio_ctx(10_000), Some(512));
        assert_eq!(
            AudioCtxPolicy::Scaled {
                pad_ms: 1_000,
                min: 1,
            }
            .audio_ctx(1_001),
            Some(101)
        );
    }

    #[test]
    fn configurable_policies_keep_the_ten_second_full_context_guard() {
        for policy in [
            AudioCtxPolicy::Fixed(512),
            AudioCtxPolicy::Scaled {
                pad_ms: 1_000,
                min: 128,
            },
        ] {
            assert!(policy.audio_ctx(10_000).is_some());
            assert_eq!(policy.audio_ctx(10_001), None);
        }
    }

    #[test]
    fn scaled_policy_rounds_up_and_clamps_to_its_bounds() {
        let policy = AudioCtxPolicy::Scaled {
            pad_ms: 1_000,
            min: 128,
        };
        assert_eq!(policy.audio_ctx(0), Some(128));
        assert_eq!(policy.audio_ctx(1_000), Some(128));
        assert_eq!(policy.audio_ctx(10_000), Some(550));
        assert_eq!(
            AudioCtxPolicy::Scaled {
                pad_ms: 100_000,
                min: 1,
            }
            .audio_ctx(10_000),
            Some(1_500)
        );
    }

    #[test]
    fn audio_ctx_policy_parses_and_displays_canonically() {
        let cases = [
            ("default", AudioCtxPolicy::Default, "default"),
            ("full", AudioCtxPolicy::Full, "full"),
            ("1", AudioCtxPolicy::Fixed(1), "1"),
            ("1500", AudioCtxPolicy::Fixed(1_500), "1500"),
            (
                "scaled",
                AudioCtxPolicy::Scaled {
                    pad_ms: 1_000,
                    min: 128,
                },
                "scaled:1000:128",
            ),
            (
                "scaled:250:64",
                AudioCtxPolicy::Scaled {
                    pad_ms: 250,
                    min: 64,
                },
                "scaled:250:64",
            ),
        ];
        for (input, expected, display) in cases {
            let parsed: AudioCtxPolicy = input.parse().unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), display);
            assert_eq!(display.parse::<AudioCtxPolicy>().unwrap(), parsed);
        }
    }

    #[test]
    fn audio_ctx_policy_rejects_invalid_values() {
        for input in [
            "",
            "0",
            "1501",
            "-1",
            "default:1",
            "scaled:",
            "scaled:1",
            "scaled:1:0",
            "scaled:1:1501",
            "scaled:x:1",
            "scaled:1:x",
            "scaled:1:1:1",
        ] {
            let error = input.parse::<AudioCtxPolicy>().unwrap_err();
            assert!(!error.to_string().is_empty(), "empty error for {input:?}");
        }
    }

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
