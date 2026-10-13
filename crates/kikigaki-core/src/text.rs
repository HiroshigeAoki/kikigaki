/// Removes Whisper's Japanese punctuation and question marks, and whitespace except
/// between ASCII alphanumeric characters. Preserved whitespace runs remain unchanged.
pub fn strip_whisper_punct(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    let mut previous = None::<char>;
    while let Some((start, ch)) = chars.next() {
        if ch.is_whitespace() {
            let mut end = start + ch.len_utf8();
            while let Some(&(index, next)) = chars.peek() {
                if !next.is_whitespace() {
                    break;
                }
                end = index + next.len_utf8();
                chars.next();
            }
            if previous.is_some_and(|ch| ch.is_ascii_alphanumeric())
                && chars
                    .peek()
                    .is_some_and(|(_, ch)| ch.is_ascii_alphanumeric())
            {
                result.push_str(&s[start..end]);
            }
        } else {
            if !matches!(ch, '、' | '。' | '?' | '？') {
                result.push(ch);
            }
            previous = Some(ch);
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
            ("a ! b . c , d 「 e 」", "a!b.c,d「e」"),
            (" \t\u{3000}", ""),
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
