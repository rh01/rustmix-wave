//! Lookup-key folding shared with `tools/lexicon/rmxlex/normalize.py`.
//!
//! The PC writer runs Unicode NFKC. Firmware queries only need the subset that
//! NFKC would apply to typed input: fullwidth ASCII, ASCII case, katakana to
//! hiragana, stripped separators, and pinyin tone letters. No normalization
//! crate is linked.

const STRIP: &[char] = &[' ', '-', '\'', '·', '.'];

/// Fold `text` into a dictionary key. The result matches the Python writer for
/// every row in `tests/fixtures/lexicon/normalize_vectors.tsv`.
#[must_use]
pub fn normalize_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        let ch = fold_fullwidth(ch);
        let ch = fold_katakana(ch);
        let ch = fold_tone(ch);
        if STRIP.contains(&ch) {
            continue;
        }
        out.push(ch);
    }
    out
}

fn fold_fullwidth(ch: char) -> char {
    let code = ch as u32;
    if (0xFF01..=0xFF5E).contains(&code) {
        char::from_u32(code - 0xFEE0).unwrap_or(ch)
    } else if ch == '\u{3000}' {
        ' '
    } else {
        ch
    }
}

fn fold_katakana(ch: char) -> char {
    let code = ch as u32;
    if (0x30A1..=0x30F6).contains(&code) {
        char::from_u32(code - 0x60).unwrap_or(ch)
    } else {
        ch
    }
}

fn fold_tone(ch: char) -> char {
    match ch {
        'ā' | 'á' | 'ǎ' | 'à' | 'Ā' | 'Á' | 'Ǎ' | 'À' => 'a',
        'ē' | 'é' | 'ě' | 'è' | 'Ē' | 'É' | 'Ě' | 'È' => 'e',
        'ī' | 'í' | 'ǐ' | 'ì' | 'Ī' | 'Í' | 'Ǐ' | 'Ì' => 'i',
        'ō' | 'ó' | 'ǒ' | 'ò' | 'Ō' | 'Ó' | 'Ǒ' | 'Ò' => 'o',
        'ū' | 'ú' | 'ǔ' | 'ù' | 'Ū' | 'Ú' | 'Ǔ' | 'Ù' => 'u',
        'ǖ' | 'ǘ' | 'ǚ' | 'ǜ' | 'ü' | 'Ǖ' | 'Ǘ' | 'Ǚ' | 'Ǜ' | 'Ü' => 'v',
        'A'..='Z' => ch.to_ascii_lowercase(),
        _ => ch,
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_key;

    #[test]
    fn shared_vectors_match_python() {
        let text = include_str!("../../tests/fixtures/lexicon/normalize_vectors.tsv");
        let mut count = 0;
        for line in text.lines().filter(|line| !line.is_empty()) {
            let (source, expected) = line.split_once('\t').expect("vector row");
            assert_eq!(normalize_key(source), expected, "{source}");
            count += 1;
        }
        assert!(count >= 40);
    }

    #[test]
    fn strips_separators_and_folds_kana() {
        assert_eq!(normalize_key("Don't"), "dont");
        assert_eq!(normalize_key("ネコ"), "ねこ");
        assert_eq!(normalize_key("nǐ hǎo"), "nihao");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(normalize_key(""), "");
        assert_eq!(normalize_key("   -"), "");
    }
}
