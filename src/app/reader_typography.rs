//! Reader-specific body typography.
//!
//! Reader pages deliberately use an independent font preference boundary so
//! global UI typography remains stable. Built-in Latin strikes cover ASCII.
//! CJK is drawn through the SD TTF/Unifont fallback at the selected pixel size.

use embedded_graphics::pixelcolor::BinaryColor;

use super::{
    display::{UiFontFamily, UiFontSize},
    reader_atkinson_next_assets::{ATKINSON_NEXT_LARGE, ATKINSON_NEXT_MEDIUM, ATKINSON_NEXT_SMALL},
    reader_literata_assets::{LITERATA_LARGE, LITERATA_MEDIUM, LITERATA_SMALL},
    reader_serif_assets::{SERIF_LARGE, SERIF_MEDIUM, SERIF_SMALL},
    typography::{style_for, UiTextRole, UiTextStyle},
};
use crate::reader::{BookFont, BookFontSize, ReadingTheme};

/// Resolve one Reader body strike without affecting global UI preferences.
#[must_use]
pub const fn reader_body_style(
    family: BookFont,
    size: BookFontSize,
    _theme: ReadingTheme,
) -> UiTextStyle {
    let (style, scale) = latin_style(family, size);
    style.with_scale(scale).with_cjk_px(size.pixels())
}

#[must_use]
const fn latin_style(family: BookFont, size: BookFontSize) -> (UiTextStyle, u8) {
    let strike = nearest_latin_strike(size);
    let scale = latin_scale(size);
    let style = match family {
        BookFont::Inter => style_for(
            UiFontFamily::Inter,
            ui_profile(strike),
            ui_role(strike),
            BinaryColor::On,
        ),
        BookFont::AtkinsonHyperlegible => {
            UiTextStyle::new(atkinson_next_font(strike), BinaryColor::On)
        }
        BookFont::Serif | BookFont::CjkUnifont | BookFont::SdCjk => {
            UiTextStyle::new(serif_font(strike), BinaryColor::On)
        }
        BookFont::Literata => UiTextStyle::new(literata_font(strike), BinaryColor::On),
    };
    (style, scale)
}

#[derive(Clone, Copy)]
enum LatinStrike {
    Small,
    Medium,
    Large,
}

#[must_use]
const fn nearest_latin_strike(size: BookFontSize) -> LatinStrike {
    match size {
        BookFontSize::Px16 | BookFontSize::Px20 => LatinStrike::Small,
        BookFontSize::Px24 => LatinStrike::Medium,
        BookFontSize::Px32 => LatinStrike::Large,
        BookFontSize::Px48 | BookFontSize::Px72 => LatinStrike::Medium,
    }
}

#[must_use]
const fn latin_scale(size: BookFontSize) -> u8 {
    match size {
        BookFontSize::Px48 => 2,
        BookFontSize::Px72 => 3,
        _ => 1,
    }
}

#[must_use]
const fn ui_profile(strike: LatinStrike) -> UiFontSize {
    match strike {
        LatinStrike::Small => UiFontSize::Compact,
        LatinStrike::Medium => UiFontSize::Standard,
        LatinStrike::Large => UiFontSize::Large,
    }
}

#[must_use]
const fn ui_role(_strike: LatinStrike) -> UiTextRole {
    UiTextRole::Body
}

#[must_use]
const fn atkinson_next_font(strike: LatinStrike) -> &'static super::typography::BitmapFont {
    match strike {
        LatinStrike::Small => &ATKINSON_NEXT_SMALL,
        LatinStrike::Medium => &ATKINSON_NEXT_MEDIUM,
        LatinStrike::Large => &ATKINSON_NEXT_LARGE,
    }
}

#[must_use]
const fn serif_font(strike: LatinStrike) -> &'static super::typography::BitmapFont {
    match strike {
        LatinStrike::Small => &SERIF_SMALL,
        LatinStrike::Medium => &SERIF_MEDIUM,
        LatinStrike::Large => &SERIF_LARGE,
    }
}

#[must_use]
const fn literata_font(strike: LatinStrike) -> &'static super::typography::BitmapFont {
    match strike {
        LatinStrike::Small => &LITERATA_SMALL,
        LatinStrike::Medium => &LITERATA_MEDIUM,
        LatinStrike::Large => &LITERATA_LARGE,
    }
}

#[cfg(test)]
mod tests {
    use super::reader_body_style;
    use crate::reader::{BookFont, BookFontSize, ReadingTheme};

    #[test]
    fn resolves_all_reader_body_profiles() {
        for family in [
            BookFont::Inter,
            BookFont::AtkinsonHyperlegible,
            BookFont::Serif,
            BookFont::Literata,
            BookFont::CjkUnifont,
            BookFont::SdCjk,
        ] {
            for size in BookFontSize::ALL {
                let style = reader_body_style(family, size, ReadingTheme::Classic);
                assert!(style.line_height() > 0);
                assert_eq!(style.cjk_px(), size.pixels());
            }
        }
    }
}
