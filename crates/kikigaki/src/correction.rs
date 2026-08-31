//! Unicode grapheme-aware correction diffing.

use serde::Serialize;
use unicode_segmentation::UnicodeSegmentation;

const MAX_WORD_GRAPHEMES: usize = 12;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Correction {
    Word { from: String, to: String },
    Sentence { from: String, to: String },
    None { message: String },
}

struct Hunk {
    from: String,
    to: String,
    from_len: usize,
}

pub fn diff(raw: &str, corrected: &str) -> Correction {
    if raw == corrected {
        return Correction::None {
            message: "変更はありません。".into(),
        };
    }
    let raw_graphemes: Vec<&str> = raw.graphemes(true).collect();
    let corrected_graphemes: Vec<&str> = corrected.graphemes(true).collect();
    let hunks = grapheme_hunks(&raw_graphemes, &corrected_graphemes);
    if hunks.len() == 1 {
        let hunk = &hunks[0];
        if hunk.from_len == 0 {
            return Correction::None {
                message: "追加のみの変更は学習しません。".into(),
            };
        }
        if hunk.from_len <= MAX_WORD_GRAPHEMES {
            return Correction::Word {
                from: hunk.from.clone(),
                to: hunk.to.clone(),
            };
        }
    }
    Correction::Sentence {
        from: raw.to_owned(),
        to: corrected.to_owned(),
    }
}

fn grapheme_hunks(raw: &[&str], corrected: &[&str]) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    for (raw_index, corrected_index) in lcs_alignment(raw, corrected) {
        match (raw_index, corrected_index) {
            (Some(_), Some(_)) => {
                if let Some(hunk) = current.take() {
                    hunks.push(hunk);
                }
            }
            _ => {
                let hunk = current.get_or_insert_with(|| Hunk {
                    from: String::new(),
                    to: String::new(),
                    from_len: 0,
                });
                if let Some(index) = raw_index {
                    hunk.from.push_str(raw[index]);
                    hunk.from_len += 1;
                }
                if let Some(index) = corrected_index {
                    hunk.to.push_str(corrected[index]);
                }
            }
        }
    }
    if let Some(hunk) = current {
        hunks.push(hunk);
    }
    hunks
}

fn lcs_alignment(a: &[&str], b: &[&str]) -> Vec<(Option<usize>, Option<usize>)> {
    let (n, m) = (a.len(), b.len());
    let mut lengths = vec![vec![0_u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lengths[i][j] = if a[i] == b[j] {
                lengths[i + 1][j + 1] + 1
            } else {
                lengths[i + 1][j].max(lengths[i][j + 1])
            };
        }
    }

    let mut alignment = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            alignment.push((Some(i), Some(j)));
            i += 1;
            j += 1;
        } else if lengths[i + 1][j] >= lengths[i][j + 1] {
            alignment.push((Some(i), None));
            i += 1;
        } else {
            alignment.push((None, Some(j)));
            j += 1;
        }
    }
    while i < n {
        alignment.push((Some(i), None));
        i += 1;
    }
    while j < m {
        alignment.push((None, Some(j)));
        j += 1;
    }
    alignment
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_is_none() {
        assert_eq!(
            diff("同じ", "同じ"),
            Correction::None {
                message: "変更はありません。".into()
            }
        );
    }

    #[test]
    fn insertion_only_is_none() {
        assert!(matches!(
            diff("テスト", "テストです"),
            Correction::None { .. }
        ));
    }

    #[test]
    fn single_short_replacement_is_a_word_rule() {
        assert_eq!(
            diff("ハザーの分析", "発話の分析"),
            Correction::Word {
                from: "ハザー".into(),
                to: "発話".into()
            }
        );
    }

    #[test]
    fn aa_to_a_is_a_word_rule() {
        assert_eq!(
            diff("aa", "a"),
            Correction::Word {
                from: "a".into(),
                to: "".into()
            }
        );
    }

    #[test]
    fn xyx_to_x_is_a_single_hunk_word_rule() {
        assert_eq!(
            diff("xyx", "x"),
            Correction::Word {
                from: "yx".into(),
                to: "".into()
            }
        );
    }

    #[test]
    fn two_disjoint_single_character_edits_become_a_sentence_replacement() {
        assert!(matches!(
            diff("AxByC", "AzByD"),
            Correction::Sentence { .. }
        ));
    }

    #[test]
    fn hunk_longer_than_twelve_graphemes_is_a_sentence_replacement() {
        assert!(matches!(
            diff(&"あ".repeat(13), &"い".repeat(13)),
            Correction::Sentence { .. }
        ));
    }

    #[test]
    fn grapheme_clusters_and_emoji_are_not_split_mid_character() {
        assert_eq!(
            diff("はい👨‍👩‍👧‍👦です", "はい🙂です"),
            Correction::Word {
                from: "👨‍👩‍👧‍👦".into(),
                to: "🙂".into()
            }
        );
    }
}
