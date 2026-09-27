//! Display-time Traditional/Simplified conversion.
//!
//! The mapping is a compact flash table of one-to-one pairs. It is not copied
//! into PSRAM. Conversion happens while a page is drawn, so TXT and EPUB byte
//! offsets and layout caches stay unchanged. The embedded face is a GB2312
//! subset, so pairs whose traditional form is outside that face are omitted
//! rather than drawn as a missing glyph.

use crate::fonts;

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

/// Simplified character, then its traditional counterpart.
///
/// The embedded Unifont is a GB2312 subset. Most traditional code points
/// (國, 書, 學, …) are absent, so those pairs are not stored: converting them
/// would draw a missing glyph. Each pair below is present on both sides.
const MAP: &[(char, char)] = &[
    ('于', '於'),
    ('伙', '夥'),
    ('后', '後'),
    ('干', '乾'),
    ('折', '摺'),
    ('征', '徵'),
];

/// Convert one line for display. Unmapped characters, including Latin, stay.
#[must_use]
pub fn convert(text: &str, script: ChineseScript) -> String {
    match script {
        ChineseScript::Original => text.to_string(),
        ChineseScript::Simplified => map_chars(text, false),
        ChineseScript::Traditional => map_chars(text, true),
    }
}

fn map_chars(text: &str, to_traditional: bool) -> String {
    text.chars()
        .map(|character| map_one(character, to_traditional))
        .collect()
}

fn map_one(character: char, to_traditional: bool) -> char {
    let mapped = if to_traditional {
        MAP.iter()
            .find(|(simplified, _)| *simplified == character)
            .map(|(_, traditional)| *traditional)
    } else {
        MAP.iter()
            .find(|(_, traditional)| *traditional == character)
            .map(|(simplified, _)| *simplified)
    };
    match mapped {
        Some(next) if fonts::gb2312_contains(next) => next,
        _ => character,
    }
}

#[cfg(test)]
mod tests {
    use super::{convert, ChineseScript, MAP};

    #[test]
    fn conversion_is_one_to_one_and_gb2312_renderable() {
        assert!(MAP.len() >= 6, "mapping table should stay useful");
        let mut missing = String::new();
        for (simplified, traditional) in MAP {
            assert_ne!(simplified, traditional);
            if !crate::fonts::gb2312_contains(*simplified) {
                missing.push_str(&format!("S:{simplified} "));
            }
            if !crate::fonts::gb2312_contains(*traditional) {
                missing.push_str(&format!("T:{traditional} "));
            }
        }
        assert!(missing.is_empty(), "outside GB2312 Unifont: {missing}");
        for (simplified, traditional) in MAP {
            let simplified_text = simplified.to_string();
            let traditional_text = traditional.to_string();
            assert_eq!(
                convert(&simplified_text, ChineseScript::Traditional),
                traditional_text
            );
            assert_eq!(
                convert(&traditional_text, ChineseScript::Simplified),
                simplified_text
            );
            assert_eq!(
                convert(&simplified_text, ChineseScript::Original),
                simplified_text
            );
            assert_eq!(
                convert(&simplified_text, ChineseScript::Traditional)
                    .chars()
                    .count(),
                1
            );
        }
        assert_eq!(convert("ABC 中", ChineseScript::Traditional), "ABC 中");
        assert_eq!(ChineseScript::Original.next(), ChineseScript::Simplified);
        assert_eq!(
            ChineseScript::parse("zh-tw").unwrap(),
            ChineseScript::Traditional
        );
    }
}
