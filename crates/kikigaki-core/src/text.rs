/// Removes Whisper's Japanese punctuation and question marks, then removes whitespace
/// unless both neighbouring characters are ASCII (so English phrases keep their spaces
/// while spaces next to Japanese text go). Preserved whitespace runs remain unchanged.
pub fn strip_whisper_punct(s: &str) -> String {
    let chars: Vec<char> = s
        .chars()
        .filter(|ch| !matches!(ch, '、' | '。' | '?' | '？'))
        .collect();
    let is_ascii = |ch: Option<&char>| ch.is_some_and(char::is_ascii_graphic);
    let mut result = String::with_capacity(s.len());
    let mut index = 0;
    while index < chars.len() {
        if !chars[index].is_whitespace() {
            result.push(chars[index]);
            index += 1;
            continue;
        }
        let start = index;
        while chars.get(index).is_some_and(|ch| ch.is_whitespace()) {
            index += 1;
        }
        let before = start.checked_sub(1).and_then(|i| chars.get(i));
        if is_ascii(before) && is_ascii(chars.get(index)) {
            result.extend(&chars[start..index]);
        }
    }
    result
}

/// Removes one trailing Japanese or ASCII period.
pub fn strip_trailing_period(s: &str) -> String {
    s.strip_suffix('。')
        .or_else(|| s.strip_suffix('.'))
        .unwrap_or(s)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_whisper_punctuation_and_preserves_other_symbols() {
        assert_eq!(
            strip_whisper_punct("、こんにちは。?？a,b.c!「」"),
            "こんにちはa,b.c!「」"
        );
        assert_eq!(strip_whisper_punct(""), "");
        assert_eq!(strip_whisper_punct("この注意マークは、注意マークの吹き出しのところって、どういう条件で追加されているんですか?"), "この注意マークは注意マークの吹き出しのところってどういう条件で追加されているんですか");
    }

    #[test]
    fn strips_whisper_whitespace_except_between_ascii_alphanumerics() {
        for (input, expected) in [
            (
                "他の確認事項を出してください あともすでに mac には",
                "他の確認事項を出してくださいあともすでにmacには",
            ),
            ("x でこれ", "xでこれ"),
            (" mac には ", "macには"),
            ("Shift Alt Space", "Shift Alt Space"),
            (" \t\u{3000}a \t\u{3000}B 12\t3 ", "a \t\u{3000}B 12\t3"),
            ("甲 \t\u{3000}乙", "甲乙"),
            ("a ! b . c , d 「 e 」", "a ! b . c , d「e」"),
            (" \t\u{3000}", ""),
        ] {
            assert_eq!(strip_whisper_punct(input), expected, "{input:?}");
        }
    }

    #[test]
    fn keeps_whitespace_between_ascii_words_around_stripped_question_marks() {
        for (input, expected) in [
            ("a? b", "a b"),
            ("a ?b", "a b"),
            ("甲 ? 乙", "甲乙"),
            ("これは test? です", "これはtestです"),
            (
                "Run kubectl apply. Then check the pods, ok",
                "Run kubectl apply. Then check the pods, ok",
            ),
        ] {
            assert_eq!(strip_whisper_punct(input), expected, "{input:?}");
        }
    }

    #[test]
    fn strips_single_trailing_kuten() {
        assert_eq!(strip_trailing_period("こんにちは。"), "こんにちは");
        assert_eq!(strip_trailing_period("hello."), "hello");
    }

    #[test]
    fn strips_only_one() {
        assert_eq!(strip_trailing_period("a。。"), "a。");
    }

    #[test]
    fn unchanged_text_is_owned() {
        assert_eq!(strip_trailing_period("hello"), "hello");
    }
}
