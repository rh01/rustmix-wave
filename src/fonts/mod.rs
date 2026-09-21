//! Unicode / CJK text support for Reader pages, titles and filenames.
//!
//! Latin UI and Reader strikes stay in flash. CJK prefers an SD-card TTF/OTF
//! face rasterized into a PSRAM glyph cache and falls back to the embedded
//! GNU Unifont GB2312 16x16 subset (scaled to the active pixel size).

use std::sync::{Mutex, OnceLock};

use embedded_graphics::{
    pixelcolor::BinaryColor,
    prelude::{DrawTarget, Pixel, Point},
};

/// Half-open clip rectangle used by mixed CJK/Latin drawing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ClipRect {
    #[must_use]
    pub const fn contains(self, point: Point) -> bool {
        point.x >= self.left && point.x < self.right && point.y >= self.top && point.y < self.bottom
    }
}

mod cache;
mod sd_ttf;
mod unifont;

pub use sd_ttf::SD_FONT_DIRECTORIES;
pub use unifont::pack_glyph_count;

use cache::{CacheKey, GlyphCache};
use sd_ttf::LoadedSdFace;

/// Reader book-body pixel sizes. These are the only accepted steps.
pub const READER_FONT_SIZE_STEPS: [u8; 6] = [16, 20, 24, 32, 48, 72];

const UI_CJK_MIN_PX: u8 = 16;
const UI_CJK_MAX_PX: u8 = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FaceId {
    Unifont,
    Sd(u8),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SdFontFace {
    pub index: u8,
    pub file_name: String,
    pub label: String,
    pub bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RasterGlyph {
    pub width: u8,
    pub height: u8,
    pub advance: u8,
    pub left: i8,
    pub top: i8,
    pub bitmap: Vec<u8>,
}

impl RasterGlyph {
    #[must_use]
    pub fn stride(&self) -> usize {
        (usize::from(self.width) + 7) / 8
    }

    #[must_use]
    pub fn bit(&self, x: u8, y: u8) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        let index = usize::from(y) * self.stride() + usize::from(x) / 8;
        self.bitmap
            .get(index)
            .is_some_and(|byte| byte & (0x80 >> (x % 8)) != 0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FontEngineStatus {
    pub unifont_glyphs: usize,
    pub unifont_bytes: usize,
    pub sd_faces: usize,
    pub cache_entries: usize,
    pub cache_bytes: usize,
}

struct FontEngine {
    sd_faces: Vec<LoadedSdFace>,
    cache: GlyphCache,
    preferred_sd: Option<u8>,
    notes: Vec<String>,
}

static ENGINE: OnceLock<Mutex<FontEngine>> = OnceLock::new();

fn engine() -> &'static Mutex<FontEngine> {
    ENGINE.get_or_init(|| {
        Mutex::new(FontEngine {
            sd_faces: Vec::new(),
            cache: GlyphCache::default(),
            preferred_sd: None,
            notes: Vec::new(),
        })
    })
}

/// Scan SD font directories and retain up to four PSRAM-sized TTF/OTF faces.
pub fn init_from_sd(extra_roots: &[&std::path::Path]) -> FontEngineStatus {
    let (faces, notes) = sd_ttf::scan_and_load(extra_roots);
    let mut guard = engine().lock().unwrap_or_else(|poison| poison.into_inner());
    guard.cache.clear();
    guard.sd_faces = faces;
    guard.notes = notes;
    if guard
        .preferred_sd
        .is_some_and(|index| usize::from(index) >= guard.sd_faces.len())
    {
        guard.preferred_sd = None;
    }
    status_locked(&guard)
}

pub fn set_preferred_sd_file(file_name: Option<&str>) {
    let mut guard = engine().lock().unwrap_or_else(|poison| poison.into_inner());
    guard.preferred_sd = file_name.and_then(|wanted| {
        guard
            .sd_faces
            .iter()
            .find(|face| face.file_name().eq_ignore_ascii_case(wanted))
            .map(|face| face.info.index)
    });
}

#[must_use]
pub fn sd_faces() -> Vec<SdFontFace> {
    let guard = engine().lock().unwrap_or_else(|poison| poison.into_inner());
    guard
        .sd_faces
        .iter()
        .map(|face| face.info.clone())
        .collect()
}

#[must_use]
pub fn status() -> FontEngineStatus {
    let guard = engine().lock().unwrap_or_else(|poison| poison.into_inner());
    status_locked(&guard)
}

#[must_use]
pub fn load_notes() -> Vec<String> {
    let guard = engine().lock().unwrap_or_else(|poison| poison.into_inner());
    guard.notes.clone()
}

fn status_locked(engine: &FontEngine) -> FontEngineStatus {
    FontEngineStatus {
        unifont_glyphs: unifont::pack_glyph_count(),
        unifont_bytes: unifont::pack_glyph_count() * 32 + 8,
        sd_faces: engine.sd_faces.len(),
        cache_entries: engine.cache.len(),
        cache_bytes: engine.cache.bytes(),
    }
}

#[must_use]
pub fn is_cjk_codepoint(character: char) -> bool {
    matches!(
        character as u32,
        0x2E80..=0x2EFF
            | 0x2F00..=0x2FDF
            | 0x3000..=0x303F
            | 0x3040..=0x30FF
            | 0x3100..=0x312F
            | 0x31A0..=0x31BF
            | 0x31C0..=0x31EF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE1F
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFFEF
            | 0x20000..=0x2FA1F
    )
}

#[must_use]
pub fn is_preserved_book_character(character: char) -> bool {
    character == '\n'
        || character == '\r'
        || character == '\t'
        || character == ' '
        || character.is_ascii_graphic()
        || is_cjk_codepoint(character)
        || matches!(character as u32, 0x2010..=0x2027)
}

#[must_use]
pub fn clamp_reader_px(px: u8) -> u8 {
    READER_FONT_SIZE_STEPS
        .iter()
        .copied()
        .min_by_key(|step| step.abs_diff(px))
        .unwrap_or(24)
}

#[must_use]
pub fn ui_cjk_px(line_height: u8) -> u8 {
    line_height.clamp(UI_CJK_MIN_PX, UI_CJK_MAX_PX)
}

#[must_use]
pub fn unicode_advance(character: char, px: u8, latin_fallback: u8) -> i32 {
    if character == '\n' || character == '\r' {
        0
    } else if can_render_unicode(character) {
        i32::from(px)
    } else {
        i32::from(latin_fallback)
    }
}

#[must_use]
pub fn can_render_unicode(character: char) -> bool {
    if character.is_ascii() {
        return false;
    }
    is_cjk_codepoint(character)
        || matches!(character as u32, 0x2010..=0x2027)
        || unifont::contains(character)
}

/// Measure mixed Latin + CJK text. ASCII uses `latin_advance`; everything else
/// uses the CJK cell size when a fallback glyph exists.
#[must_use]
pub fn measure_mixed(text: &str, px: u8, latin_advance: impl Fn(char) -> u8) -> i32 {
    text.chars()
        .filter(|character| *character != '\n')
        .map(|character| {
            if character.is_ascii() {
                i32::from(latin_advance(character))
            } else {
                unicode_advance(character, px, latin_advance('?'))
            }
        })
        .sum()
}

#[must_use]
pub fn truncate_to_width(
    text: &str,
    max_chars: usize,
    max_px: i32,
    px: u8,
    latin_advance: impl Fn(char) -> u8,
) -> String {
    if text.chars().count() <= max_chars && measure_mixed(text, px, &latin_advance) <= max_px {
        return text.into();
    }
    let ellipsis = "...";
    let ellipsis_px = measure_mixed(ellipsis, px, &latin_advance);
    let mut output = String::new();
    let mut width = 0;
    let mut count = 0usize;
    for character in text.chars() {
        let advance = if character.is_ascii() {
            i32::from(latin_advance(character))
        } else {
            unicode_advance(character, px, latin_advance('?'))
        };
        if count + 1 > max_chars.saturating_sub(3) || width + advance + ellipsis_px > max_px {
            output.push_str(ellipsis);
            return output;
        }
        output.push(character);
        width += advance;
        count += 1;
    }
    output
}

pub fn draw_mixed_char<D>(
    display: &mut D,
    baseline: Point,
    character: char,
    px: u8,
    color: BinaryColor,
    bounds: Option<ClipRect>,
) -> Result<Option<u8>, D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    if character.is_ascii() {
        return Ok(None);
    }
    if let Some(glyph) = rasterize(character, px) {
        blit_glyph(display, baseline, &glyph, color, 1, bounds)?;
        return Ok(Some(glyph.advance.max(1)));
    }
    if let Some(bitmap) = unifont::native_bitmap(character) {
        blit_unifont(display, baseline, bitmap, px, color, bounds)?;
        return Ok(Some(px.max(1)));
    }
    Ok(None)
}

fn rasterize(character: char, px: u8) -> Option<RasterGlyph> {
    let mut guard = engine().lock().ok()?;
    let face_id = guard.preferred_sd.map(FaceId::Sd).or_else(|| {
        guard
            .sd_faces
            .first()
            .map(|face| FaceId::Sd(face.info.index))
    });
    let Some(FaceId::Sd(index)) = face_id else {
        return None;
    };
    let key = CacheKey {
        face: FaceId::Sd(index),
        px,
        character,
    };
    if let Some(glyph) = guard.cache.get(key) {
        return Some(glyph);
    }
    let glyph = guard
        .sd_faces
        .iter()
        .find(|face| face.info.index == index)?
        .rasterize(character, px)?;
    guard.cache.insert(key, glyph.clone());
    Some(glyph)
}

fn blit_unifont<D>(
    display: &mut D,
    baseline: Point,
    bitmap: &[u8],
    px: u8,
    color: BinaryColor,
    bounds: Option<ClipRect>,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let px = px.max(1);
    let top = 2 - i32::from(px);
    for y in 0..px {
        let src_y = (u16::from(y) * 16 / u16::from(px)) as u8;
        for x in 0..px {
            let src_x = (u16::from(x) * 16 / u16::from(px)) as u8;
            if !unifont::bit_at(bitmap, src_x, src_y) {
                continue;
            }
            let point = Point::new(baseline.x + i32::from(x), baseline.y + top + i32::from(y));
            if bounds.map_or(true, |clip| clip.contains(point)) {
                display.draw_iter(core::iter::once(Pixel(point, color)))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn blit_glyph<D>(
    display: &mut D,
    baseline: Point,
    glyph: &RasterGlyph,
    color: BinaryColor,
    scale: u8,
    bounds: Option<ClipRect>,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    let scale = scale.max(1);
    for y in 0..glyph.height {
        for x in 0..glyph.width {
            if !glyph.bit(x, y) {
                continue;
            }
            for dy in 0..scale {
                for dx in 0..scale {
                    let point = Point::new(
                        baseline.x
                            + i32::from(glyph.left) * i32::from(scale)
                            + i32::from(x) * i32::from(scale)
                            + i32::from(dx),
                        baseline.y
                            + i32::from(glyph.top) * i32::from(scale)
                            + i32::from(y) * i32::from(scale)
                            + i32::from(dy),
                    );
                    if bounds.map_or(true, |clip| clip.contains(point)) {
                        display.draw_iter(core::iter::once(Pixel(point, color)))?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_reader_px, is_cjk_codepoint, is_preserved_book_character, measure_mixed,
        pack_glyph_count, truncate_to_width, READER_FONT_SIZE_STEPS,
    };

    #[test]
    fn reader_size_steps_are_exact() {
        assert_eq!(READER_FONT_SIZE_STEPS, [16, 20, 24, 32, 48, 72]);
        assert_eq!(clamp_reader_px(23), 24);
        assert_eq!(clamp_reader_px(70), 72);
    }

    #[test]
    fn preserves_cjk_and_measures_wide_cells() {
        assert!(is_cjk_codepoint('中'));
        assert!(is_preserved_book_character('汉'));
        assert!(pack_glyph_count() > 6000);
        let width = measure_mixed("中A文", 16, |ch| if ch == 'A' { 8 } else { 10 });
        assert_eq!(width, 16 + 8 + 16);
    }

    #[test]
    fn truncates_mixed_titles_on_pixel_budget() {
        let text = "中文文件名很长ABCDEFG";
        let clipped = truncate_to_width(text, 40, 48, 16, |_| 8);
        assert!(clipped.ends_with("..."));
        assert!(clipped.chars().count() < text.chars().count());
    }
}
