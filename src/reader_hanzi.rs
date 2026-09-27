//! Display-time Traditional/Simplified conversion.
//!
//! The tables are the first candidate from OpenCC `STCharacters.txt` and
//! `TSCharacters.txt` (Apache-2.0, <https://github.com/BYVoid/OpenCC>), packed
//! as sorted little-endian `(source, target)` code points in `.rodata`. Lookup
//! is a binary search. The tables are not copied into PSRAM. Conversion happens
//! while a page is drawn and does not change byte offsets. A target the
//! embedded GB2312 face cannot draw is left as the original character.
//!
//! The selected script is part of the layout cache fingerprint, so a script
//! change names a different TXT or EPUB chapter cache.

use crate::fonts;

/// Simplified to traditional, first OpenCC candidate.
static SIMP_TO_TRAD: &[u8] = include_bytes!("reader_hanzi_s2t.bin");
/// Traditional to simplified, first OpenCC candidate.
static TRAD_TO_SIMP: &[u8] = include_bytes!("reader_hanzi_t2s.bin");

/// Display script. Original leaves the book text unchanged.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChineseScript {
    #[default]
    Original,
    Simplified,
    Traditional,
}

impl ChineseScript {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Simplified => "Simplified",
            Self::Traditional => "Traditional",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Simplified => "simplified",
            Self::Traditional => "traditional",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Original => Self::Simplified,
            Self::Simplified => Self::Traditional,
            Self::Traditional => Self::Original,
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "original" | "off" | "none" => Ok(Self::Original),
            "simplified" | "simp" | "zh-cn" => Ok(Self::Simplified),
            "traditional" | "trad" | "zh-tw" => Ok(Self::Traditional),
            other => Err(format!("unsupported chinese value {other:?}")),
        }
    }
}

/// Convert one line for display. Unmapped characters, including Latin, stay.
#[must_use]
pub fn convert(text: &str, script: ChineseScript) -> String {
    match script {
        ChineseScript::Original => text.to_string(),
        ChineseScript::Simplified => map_chars(text, TRAD_TO_SIMP),
        ChineseScript::Traditional => map_chars(text, SIMP_TO_TRAD),
    }
}

fn map_chars(text: &str, table: &[u8]) -> String {
    text.chars()
        .map(|character| map_one(character, table))
        .collect()
}

fn map_one(character: char, table: &[u8]) -> char {
    let Some(target) = lookup(table, character as u32) else {
        return character;
    };
    match char::from_u32(target) {
        Some(next) if fonts::gb2312_contains(next) => next,
        _ => character,
    }
}

fn lookup(table: &[u8], source: u32) -> Option<u32> {
    let records = table.len() / 8;
    let mut low = 0usize;
    let mut high = records;
    while low < high {
        let mid = low + (high - low) / 2;
        let offset = mid * 8;
        let key = read_u32(table, offset);
        match key.cmp(&source) {
            core::cmp::Ordering::Less => low = mid + 1,
            core::cmp::Ordering::Greater => high = mid,
            core::cmp::Ordering::Equal => return Some(read_u32(table, offset + 4)),
        }
    }
    None
}

fn read_u32(table: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        table[offset],
        table[offset + 1],
        table[offset + 2],
        table[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::{convert, lookup, read_u32, ChineseScript, SIMP_TO_TRAD, TRAD_TO_SIMP};

    fn assert_sorted(table: &[u8]) {
        assert_eq!(table.len() % 8, 0);
        let records = table.len() / 8;
        for index in 1..records {
            let previous = read_u32(table, (index - 1) * 8);
            let current = read_u32(table, index * 8);
            assert!(previous < current);
        }
    }

    #[test]
    fn opencc_tables_are_sorted_and_cover_common_characters() {
        assert!(SIMP_TO_TRAD.len() / 8 >= 3_800);
        assert!(TRAD_TO_SIMP.len() / 8 >= 3_200);
        assert_sorted(SIMP_TO_TRAD);
        assert_sorted(TRAD_TO_SIMP);
        assert_eq!(lookup(SIMP_TO_TRAD, '国' as u32), Some('國' as u32));
        assert_eq!(lookup(SIMP_TO_TRAD, '后' as u32), Some('後' as u32));
        assert_eq!(lookup(SIMP_TO_TRAD, '干' as u32), Some('幹' as u32));
        assert_eq!(lookup(TRAD_TO_SIMP, '國' as u32), Some('国' as u32));
        assert_eq!(lookup(TRAD_TO_SIMP, '後' as u32), Some('后' as u32));
        let supplementary = (0..SIMP_TO_TRAD.len() / 8)
            .map(|index| read_u32(SIMP_TO_TRAD, index * 8))
            .find(|code| *code > 0xFFFF);
        let supplementary = supplementary.expect("OpenCC extension characters");
        assert!(lookup(SIMP_TO_TRAD, supplementary).is_some());
    }

    #[test]
    fn conversion_keeps_characters_the_font_cannot_draw() {
        assert!(!crate::fonts::gb2312_contains('國'));
        assert_eq!(convert("国", ChineseScript::Traditional), "国");
        assert_eq!(convert("国ABC", ChineseScript::Traditional), "国ABC");
        let traditional = convert("后", ChineseScript::Traditional);
        if crate::fonts::gb2312_contains('後') {
            assert_eq!(traditional, "後");
            assert_eq!(convert("後", ChineseScript::Simplified), "后");
        } else {
            assert_eq!(traditional, "后");
        }
        assert_eq!(traditional.chars().count(), 1);
        assert_eq!(convert("ABC 中", ChineseScript::Traditional), "ABC 中");
        assert_eq!(convert("后", ChineseScript::Original), "后");
        assert_eq!(ChineseScript::Original.next(), ChineseScript::Simplified);
        assert_eq!(
            ChineseScript::parse("zh-tw").unwrap(),
            ChineseScript::Traditional
        );
    }
}
