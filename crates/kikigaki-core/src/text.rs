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
