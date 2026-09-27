//! Pixel-width wrapping for mixed Latin and CJK text.
//!
//! ASCII words stay intact unless the word itself is wider than the line.
//! CJK characters may break at any character boundary.

use crate::app::typography::UiTextStyle;

/// Wrap `text` so each line's measured width is at most `max_width`.
#[must_use]
pub fn wrap_text(text: &str, max_width: i32, measure: impl Fn(&str) -> i32) -> Vec<String> {
    if max_width <= 0 || text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut line = String::new();
        for token in tokens(paragraph) {
            match token {
                Token::Space => {
                    if !line.is_empty() && measure(&format!("{line} ")) <= max_width {
                        line.push(' ');
                    }
                }
                Token::Word(word) => {
                    let candidate = if line.is_empty() || line.ends_with(' ') {
                        format!("{line}{word}")
                    } else {
                        format!("{line}{word}")
                    };
                    if measure(&candidate) <= max_width {
                        line = candidate;
                        continue;
                    }
                    if !line.is_empty() && !line.ends_with(' ') || line.ends_with(' ') {
                        if !line.trim().is_empty() {
                            lines.push(line.trim_end().to_string());
                        }
                        line.clear();
                    }
                    if measure(word) <= max_width {
                        line = word.to_string();
                    } else {
                        let mut piece = String::new();
                        for ch in word.chars() {
                            let next = format!("{piece}{ch}");
                            if !piece.is_empty() && measure(&next) > max_width {
                                lines.push(piece);
                                piece = ch.to_string();
                            } else {
                                piece = next;
                            }
                        }
                        line = piece;
                    }
                }
            }
        }
        if !line.trim().is_empty() {
            lines.push(line.trim_end().to_string());
        }
    }
    lines
}

#[must_use]
pub fn wrap_ui(text: &str, max_width: i32, style: UiTextStyle) -> Vec<String> {
    wrap_text(text, max_width, |value| style.text_width(value))
}

enum Token<'a> {
    Space,
    Word(&'a str),
}

fn tokens(text: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if ch.is_ascii_whitespace() {
            if let Some(from) = start.take() {
                out.push(Token::Word(&text[from..index]));
            }
            out.push(Token::Space);
        } else if ch.is_ascii() {
            if start.is_none() {
                start = Some(index);
            }
        } else {
            if let Some(from) = start.take() {
                out.push(Token::Word(&text[from..index]));
            }
            let end = index + ch.len_utf8();
            out.push(Token::Word(&text[index..end]));
        }
    }
    if let Some(from) = start {
        out.push(Token::Word(&text[from..]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::wrap_text;

    fn width(text: &str) -> i32 {
        text.chars()
            .map(|ch| if ch.is_ascii() { 8 } else { 16 })
            .sum()
    }

    #[test]
    fn mixed_lines_stay_within_width() {
        let lines = wrap_text("你好hello world", 40, width);
        assert!(lines.iter().all(|line| width(line) <= 40));
        assert!(lines.iter().any(|line| line.contains("hello")));
    }

    #[test]
    fn ascii_words_are_not_split() {
        let lines = wrap_text("hello world", 48, width);
        assert_eq!(lines, vec!["hello".to_string(), "world".to_string()]);
        assert!(lines
            .iter()
            .all(|line| !line.contains("hell ") && line != "hel"));
    }

    #[test]
    fn overlong_ascii_word_breaks_by_character() {
        let lines = wrap_text("abcdef", 24, width);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| width(line) <= 24));
        assert_eq!(lines.concat(), "abcdef");
    }

    #[test]
    fn cjk_breaks_between_characters() {
        let lines = wrap_text("中文测试", 32, width);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| width(line) <= 32));
    }
}
