use std::collections::HashMap;

use anyhow::{ensure, Context};
use unicode_normalization::UnicodeNormalization;

/// Maximum text characters sent to the Mojicast model in one inference call.
pub const MAX_CHARS: usize = 500;

/// Characters that suppress adjacent punctuation insertion.
pub const JA_PUNCT_CHARS: &str = "。、！？!?…「」『』（）()【】・,.\n";

/// Sentence suffixes that turn a model-produced Japanese period into a question mark.
pub const QUESTION_SUFFIXES: &[&str] = &[
    "ですか",
    "ますか",
    "でしょうか",
    "かな",
    "かしら",
    "かい",
    "の",
    "だろうか",
    "でしたか",
    "ましたか",
];

/// Character token IDs and the special IDs required by the model.
#[derive(Debug, Clone)]
pub struct Vocab {
    ids: HashMap<char, i64>,
    /// `[CLS]` token ID.
    pub cls: i64,
    /// `[SEP]` token ID.
    pub sep: i64,
    /// `[UNK]` token ID used for characters absent from the vocabulary.
    pub unk: i64,
}

impl Vocab {
    /// Parses a BERT `vocab.txt`, where each line number is its token ID.
    pub fn parse(vocab_txt: &str) -> anyhow::Result<Self> {
        let mut ids = HashMap::new();
        let mut cls = None;
        let mut sep = None;
        let mut unk = None;

        for (index, token) in vocab_txt.lines().enumerate() {
            let id = i64::try_from(index).context("vocabulary has too many entries")?;
            match token {
                "[CLS]" => cls = Some(id),
                "[SEP]" => sep = Some(id),
                "[UNK]" => unk = Some(id),
                _ => {
                    let mut characters = token.chars();
                    if let (Some(character), None) = (characters.next(), characters.next()) {
                        ids.insert(character, id);
                    }
                }
            }
        }

        Ok(Self {
            ids,
            cls: cls.context("vocabulary is missing [CLS]")?,
            sep: sep.context("vocabulary is missing [SEP]")?,
            unk: unk.context("vocabulary is missing [UNK]")?,
        })
    }

    /// Returns the token ID for `character`, or the vocabulary's `[UNK]` ID.
    pub fn id(&self, character: char) -> i64 {
        self.ids.get(&character).copied().unwrap_or(self.unk)
    }
}

/// Normalizes text using Unicode Normalization Form KC.
pub fn nfkc(text: &str) -> String {
    text.nfkc().collect()
}

/// Encodes characters between `[CLS]` and `[SEP]` with an all-ones attention mask.
pub fn encode(vocab: &Vocab, chars: &[char]) -> (Vec<i64>, Vec<i64>) {
    let mut ids = Vec::with_capacity(chars.len() + 2);
    ids.push(vocab.cls);
    ids.extend(chars.iter().map(|character| vocab.id(*character)));
    ids.push(vocab.sep);
    let mask = vec![1; ids.len()];
    (ids, mask)
}

/// Probability thresholds controlling punctuation decisions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Minimum comma probability that inserts `、`.
    pub comma: f32,
    /// Minimum period probability that inserts `。`.
    pub period: f32,
    /// Whether unpunctuated output must end in `。`.
    pub force_final_period: bool,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            comma: 0.5,
            period: 0.5,
            force_final_period: true,
        }
    }
}

/// Inserts Japanese punctuation from per-character `(comma, period)` probabilities.
///
/// `probs` must contain exactly one entry for every input character.
pub fn decide(chars: &[char], probs: &[(f32, f32)], thresholds: Thresholds) -> String {
    assert_eq!(
        chars.len(),
        probs.len(),
        "one probability pair per character"
    );
    let mut output = String::new();
    for (index, (&character, &(comma_probability, period_probability))) in
        chars.iter().zip(probs).enumerate()
    {
        output.push(character);
        let is_last = index + 1 == chars.len();
        let next = chars.get(index + 1).copied();
        if JA_PUNCT_CHARS.contains(character)
            || next.is_some_and(|next_character| JA_PUNCT_CHARS.contains(next_character))
        {
            continue;
        }
        if period_probability >= thresholds.period && (!is_last || thresholds.force_final_period) {
            output.push('。');
        } else if comma_probability >= thresholds.comma {
            output.push('、');
        }
    }
    if thresholds.force_final_period
        && !output.is_empty()
        && !output
            .chars()
            .next_back()
            .is_some_and(|character| JA_PUNCT_CHARS.contains(character))
    {
        output.push('。');
    }
    output
}

/// Applies the Japanese question-suffix heuristic while preserving existing punctuation.
pub fn apply_question_marks(text: &str) -> String {
    fn append_segment(output: &mut String, segment: &str, has_period: bool) {
        if segment.is_empty() {
            if has_period {
                output.push('。');
            }
            return;
        }
        output.push_str(segment);
        if segment
            .chars()
            .next_back()
            .is_some_and(|character| JA_PUNCT_CHARS.contains(character))
        {
            return;
        }
        if QUESTION_SUFFIXES
            .iter()
            .any(|suffix| segment.ends_with(suffix))
        {
            output.push('？');
        } else {
            output.push('。');
        }
    }

    let mut output = String::with_capacity(text.len());
    let mut start = 0;
    for (index, character) in text.char_indices() {
        if character == '。' {
            append_segment(&mut output, &text[start..index], true);
            start = index + character.len_utf8();
        }
    }
    if start < text.len() {
        append_segment(&mut output, &text[start..], false);
    }
    output
}

/// Runs the model callback in consecutive windows and decides punctuation over the full text.
pub fn punctuate_windowed(
    chars: &[char],
    mut run: impl FnMut(&[char]) -> anyhow::Result<Vec<(f32, f32)>>,
    thresholds: Thresholds,
) -> anyhow::Result<String> {
    let mut probabilities = Vec::with_capacity(chars.len());
    for window in chars.chunks(MAX_CHARS) {
        let window_probabilities = run(window)?;
        ensure!(
            window_probabilities.len() == window.len(),
            "model returned {} probabilities for {} characters",
            window_probabilities.len(),
            window.len()
        );
        probabilities.extend(window_probabilities);
    }
    Ok(decide(chars, &probabilities, thresholds))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds(force_final_period: bool) -> Thresholds {
        Thresholds {
            comma: 0.5,
            period: 0.5,
            force_final_period,
        }
    }

    #[test]
    fn decide_inserts_period_then_comma_without_doubling_punctuation() {
        let chars: Vec<char> = "今日は晴れ。明日".chars().collect();
        let mut probs = vec![(0.0, 0.0); chars.len()];
        probs[1] = (0.9, 0.9);
        probs[2] = (0.9, 0.0);
        probs[4] = (0.9, 0.9);
        probs[chars.len() - 1] = (0.0, 0.9);
        assert_eq!(
            decide(&chars, &probs, thresholds(false)),
            "今日。は、晴れ。明日"
        );
    }

    #[test]
    fn force_final_period_appends_period() {
        let chars: Vec<char> = "今日は晴れ".chars().collect();
        let probs = vec![(0.0, 0.0); chars.len()];
        assert_eq!(decide(&chars, &probs, thresholds(true)), "今日は晴れ。");
        assert_eq!(decide(&chars, &probs, thresholds(false)), "今日は晴れ");
    }

    #[test]
    fn question_marks_are_applied_without_doubling_existing_marks() {
        assert_eq!(
            apply_question_marks("元気ですか。今日は。"),
            "元気ですか？今日は。"
        );
        assert_eq!(
            apply_question_marks("元気ですか？今日は！"),
            "元気ですか？今日は！"
        );
        assert_eq!(apply_question_marks("元気ですか？。"), "元気ですか？");
        assert_eq!(apply_question_marks("今日は。。"), "今日は。。");
        assert_eq!(apply_question_marks("一行目ですか。\n"), "一行目ですか？\n");
    }

    #[test]
    fn vocab_uses_line_indexes_and_unknown_id() {
        let vocab = Vocab::parse("[PAD]\n[UNK]\n[CLS]\n[SEP]\nあ\nmultiple\nい\n").unwrap();
        assert_eq!(vocab.unk, 1);
        assert_eq!(vocab.cls, 2);
        assert_eq!(vocab.sep, 3);
        assert_eq!(vocab.id('あ'), 4);
        assert_eq!(vocab.id('い'), 6);
        assert_eq!(vocab.id('外'), vocab.unk);
        let (ids, mask) = encode(&vocab, &['あ', '外']);
        assert_eq!(ids, [2, 4, 1, 3]);
        assert_eq!(mask, [1, 1, 1, 1]);
    }

    #[test]
    fn vocab_requires_all_three_special_tokens() {
        for text in [
            "[UNK]\n[SEP]\nあ\n",
            "[CLS]\n[SEP]\nあ\n",
            "[UNK]\n[CLS]\nあ\n",
        ] {
            assert!(Vocab::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn nfkc_normalizes_compatibility_characters() {
        assert_eq!(nfkc("ＡＢＣ①㍉"), "ABC1ミリ");
    }

    #[test]
    fn windowing_runs_three_windows_and_preserves_all_characters() {
        let chars = vec!['あ'; 1_200];
        let mut window_sizes = Vec::new();
        let output = punctuate_windowed(
            &chars,
            |window| {
                window_sizes.push(window.len());
                Ok(vec![(0.0, 0.0); window.len()])
            },
            thresholds(false),
        )
        .unwrap();
        assert_eq!(window_sizes, [500, 500, 200]);
        assert_eq!(output.chars().count(), 1_200);
        assert!(output.chars().all(|character| character == 'あ'));
    }

    #[test]
    fn windowing_rejects_probability_count_mismatch() {
        let error = punctuate_windowed(&['あ'], |_| Ok(Vec::new()), thresholds(false)).unwrap_err();
        assert!(format!("{error:#}").contains("probabilities"));
    }
}
