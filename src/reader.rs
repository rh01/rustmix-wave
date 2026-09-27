//! Offline Reader state, TXT / EPUB pagination and Reader-owned persistence.
//!
//! EPUB page anchors are stored in a paged SD index (RAM fallback when the card
//! cannot be written) so long CJK books open before indexing finishes. Hitting
//! the safety cap keeps the current page readable and switches progress to an
//! approximation instead of refusing the book.
// rustmix-wave=epub-paged-anchor-index-ready

use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use crate::{
    buttons::ButtonEvent,
    epub::{open_epub_on_worker, read_epub_title_on_worker, EpubDocument, EpubTocEntry},
};

/// SD-card library owned by the Reader subsystem.
pub const READER_BOOKS_DIRECTORY: &str = "/sdcard/RUSTMIX/BOOKS";
/// SD-card state directory owned by the Reader subsystem.
pub const READER_STATE_DIRECTORY: &str = "/sdcard/RUSTMIX/READER";
/// Persistent last-read state file.
pub const READER_STATE_FILE: &str = "STATE.TXT";
/// Persistent per-book last-position map.
pub const READER_POSITIONS_FILE: &str = "POSITS.TXT";
/// Legacy long-name per-book positions file accepted read-only for migration.
pub const LEGACY_READER_POSITIONS_FILE: &str = "POSITIONS.TXT";
/// Persistent recent-book list.
pub const READER_RECENT_FILE: &str = "RECENT.TXT";
/// Persistent bookmark list.
pub const READER_BOOKMARKS_FILE: &str = "MARKS.TXT";
/// Persistent Reader-specific preferences.
pub const READER_PREFS_FILE: &str = "PREFS.TXT";
/// SD-backed TXT anchor-cache directory.
pub const READER_CACHE_DIRECTORY: &str = "CACHE";
/// Number of text lines rendered on one portrait Reader page.
pub const READER_LINES_PER_PAGE: usize = 22;
/// Maximum wrapped characters per line for the current Reader body profile.
pub const READER_CHARS_PER_LINE: usize = 43;
/// Nearby page cache retained in RAM while one book is open.
pub const READER_NEARBY_PAGE_CACHE: usize = 8;
/// Maximum bytes read while generating a single page.
pub const READER_PAGE_READ_BYTES: usize = 16 * 1024;
/// Maximum library rows retained for the embedded product UI.
pub const READER_LIBRARY_LIMIT: usize = 128;
/// Maximum per-book last-position records retained on removable storage.
pub const READER_POSITION_LIMIT: usize = 64;
/// Maximum recent-book records retained on removable storage.
pub const READER_RECENT_LIMIT: usize = 16;
/// Maximum bookmark records retained on removable storage.
pub const READER_BOOKMARK_LIMIT: usize = 128;
/// Maximum page anchors written into one TXT anchor-cache file.
///
/// This bounds the text cache on SD. It does not refuse to open a TXT book,
/// and a cache that fills the file is treated as incomplete so reading can
/// continue past the persisted window.
pub const READER_CACHE_OFFSET_LIMIT: usize = 4096;
/// Persist an anchor-cache checkpoint after this many newly indexed pages.
pub const READER_CACHE_CHECKPOINT_PAGES: usize = 4;
/// Safety ceiling for EPUB page anchors in the SD page index.
///
/// Realistic novels, including long CJK books at 16 px, stay far below this.
/// Hostile input stops here and remains readable with approximate progress.
pub const READER_EPUB_PAGE_ANCHOR_LIMIT: usize = 262_144;
/// Byte cap for one EPUB page-index file, including its header and chapter
/// directory. Indexing stops at whichever of the page ceiling or this byte
/// cap is reached first.
pub const READER_EPUB_ANCHOR_INDEX_BYTES_LIMIT: usize =
    8 * 1024 + READER_EPUB_PAGE_ANCHOR_LIMIT * 8;
/// Anchors kept in RAM when the SD page index cannot be created.
pub const READER_EPUB_RAM_FALLBACK_ANCHORS: usize = 16_384;
/// Reading-window anchors kept beside the open page. The full index is not
/// stored in this window.
pub const READER_EPUB_READING_WINDOW: usize = 64;
/// EPUB anchors recorded on each background tick after the current page is open.
pub const READER_EPUB_BACKGROUND_INDEX_PAGES: usize = 16;
/// Number of EPUB page anchors generated before briefly blocking the current
/// task. The pause lets the ESP-IDF idle task feed its watchdog while large
/// chapters are indexed for chapter-relative totals.
pub const READER_EPUB_INDEX_YIELD_EVERY_PAGES: usize = 4;
/// Cooperative pause used during bounded EPUB chapter pagination.
pub const READER_EPUB_INDEX_YIELD_MILLIS: u64 = 1;

const READER_PERSISTENCE_VERSION: &str = "1";
const READER_CACHE_VERSION: &str = "3";
const READER_PREFS_VERSION: &str = "1";
const CACHE_FNV_OFFSET: u64 = 0xcbf29ce484222325;
const CACHE_FNV_PRIME: u64 = 0x100000001b3;

/// Reader-supported content types. TXT and bounded reflowable EPUB are active.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookFormat {
    Text,
    Epub,
}

impl BookFormat {
    #[must_use]
    pub const fn badge(self) -> &'static str {
        match self {
            Self::Text => "TXT",
            Self::Epub => "EPUB",
        }
    }

    #[must_use]
    const fn marker(self) -> &'static str {
        match self {
            Self::Text => "txt",
            Self::Epub => "epub",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "txt" => Some(Self::Text),
            "epub" => Some(Self::Epub),
            _ => None,
        }
    }
}

/// One Reader library row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderBook {
    pub path: String,
    pub title: String,
    pub format: BookFormat,
    pub size_bytes: u64,
    pub modified_seconds: u64,
}

/// Chapter-relative EPUB page presentation retained with bookmarks so MARKS.TXT
/// remains useful after restart and before the matching book is reopened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderChapterPageLabel {
    pub chapter_number: usize,
    pub page_number: usize,
    pub page_count: usize,
    /// Set when `page_count` is estimated or still unknown. Unknown totals
    /// render as `n+`; estimated totals render as `n/~m`.
    pub approximate: bool,
}

impl ReaderChapterPageLabel {
    #[must_use]
    pub fn page_text(&self) -> String {
        if self.approximate {
            if self.page_count == 0 {
                format!("{}+", self.page_number.max(1))
            } else {
                format!(
                    "{}/~{}",
                    self.page_number.max(1),
                    self.page_count.max(self.page_number)
                )
            }
        } else {
            format!("{}/{}", self.page_number, self.page_count.max(1))
        }
    }
}

/// Stable logical reading position used by STATE.TXT, RECENT.TXT and
/// MARKS.TXT. TXT byte offsets remain valid independently of generated UI page
/// labels. EPUB reuses this byte-offset boundary against its flattened text buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderLocation {
    pub path: String,
    pub title: String,
    pub format: BookFormat,
    pub size_bytes: u64,
    pub modified_seconds: u64,
    pub page_index: usize,
    pub byte_offset: u64,
    pub epub_chapter: Option<ReaderChapterPageLabel>,
}

impl ReaderLocation {
    #[must_use]
    pub fn as_book(&self) -> ReaderBook {
        ReaderBook {
            path: self.path.clone(),
            title: self.title.clone(),
            format: self.format,
            size_bytes: self.size_bytes,
            modified_seconds: self.modified_seconds,
        }
    }

    #[must_use]
    fn matches_book(&self, book: &ReaderBook) -> bool {
        self.path == book.path
            && self.size_bytes == book.size_bytes
            && self.modified_seconds == book.modified_seconds
            && self.format == book.format
    }

    #[must_use]
    fn same_position(&self, other: &Self) -> bool {
        self.path == other.path && self.byte_offset == other.byte_offset
    }
}

/// One list row rendered by Recent, Books, Files or Bookmarks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderLibraryEntry {
    pub book: ReaderBook,
    pub location: Option<ReaderLocation>,
}

/// Reader Library tab model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReaderLibraryTab {
    Recent,
    #[default]
    Books,
    Files,
    Bookmarks,
}

impl ReaderLibraryTab {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Recent => "Recent",
            Self::Books => "Books",
            Self::Files => "Files",
            Self::Bookmarks => "Bookmarks",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Recent => Self::Books,
            Self::Books => Self::Files,
            Self::Files => Self::Bookmarks,
            Self::Bookmarks => Self::Recent,
        }
    }
}

/// Text decoding mode detected when a TXT book is opened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextEncoding {
    Utf8,
    Utf8Bom,
    Windows1252,
}

impl TextEncoding {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8 BOM",
            Self::Windows1252 => "WIN-1252",
        }
    }
}

/// E-paper-friendly Reader page theme.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReadingTheme {
    #[default]
    Classic,
    HighContrast,
}

impl ReadingTheme {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::HighContrast => "High Contrast",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::HighContrast => "high-contrast",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Classic => Self::HighContrast,
            Self::HighContrast => Self::Classic,
        }
    }

    #[must_use]
    pub const fn previous(self) -> Self {
        self.next()
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "classic" => Ok(Self::Classic),
            "high-contrast" | "high_contrast" => Ok(Self::HighContrast),
            other => Err(format!("unsupported theme value {other:?}")),
        }
    }
}

/// Reader-page orientation independent from the portrait system UI.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReaderOrientation {
    #[default]
    Portrait,
    Landscape,
}

impl ReaderOrientation {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Portrait => "Portrait",
            Self::Landscape => "Landscape",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Portrait => "portrait",
            Self::Landscape => "landscape",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Portrait => Self::Landscape,
            Self::Landscape => Self::Portrait,
        }
    }

    #[must_use]
    pub const fn previous(self) -> Self {
        self.next()
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "portrait" => Ok(Self::Portrait),
            "landscape" => Ok(Self::Landscape),
            other => Err(format!("unsupported orientation value {other:?}")),
        }
    }
}

/// Reader-specific book font size in exact e-paper pixels.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BookFontSize {
    Px16,
    Px20,
    #[default]
    Px24,
    Px32,
    Px48,
    Px72,
}

impl BookFontSize {
    pub const ALL: [Self; 6] = [
        Self::Px16,
        Self::Px20,
        Self::Px24,
        Self::Px32,
        Self::Px48,
        Self::Px72,
    ];

    #[must_use]
    pub const fn pixels(self) -> u8 {
        match self {
            Self::Px16 => 16,
            Self::Px20 => 20,
            Self::Px24 => 24,
            Self::Px32 => 32,
            Self::Px48 => 48,
            Self::Px72 => 72,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Px16 => "16 px",
            Self::Px20 => "20 px",
            Self::Px24 => "24 px",
            Self::Px32 => "32 px",
            Self::Px48 => "48 px",
            Self::Px72 => "72 px",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Px16 => "16",
            Self::Px20 => "20",
            Self::Px24 => "24",
            Self::Px32 => "32",
            Self::Px48 => "48",
            Self::Px72 => "72",
        }
    }

    pub fn from_pixels(px: u8) -> Result<Self, String> {
        match crate::fonts::clamp_reader_px(px) {
            16 => Ok(Self::Px16),
            20 => Ok(Self::Px20),
            24 => Ok(Self::Px24),
            32 => Ok(Self::Px32),
            48 => Ok(Self::Px48),
            72 => Ok(Self::Px72),
            other => Err(format!("unsupported book_font_size value {other}")),
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Px16 => Self::Px20,
            Self::Px20 => Self::Px24,
            Self::Px24 => Self::Px32,
            Self::Px32 => Self::Px48,
            Self::Px48 => Self::Px72,
            Self::Px72 => Self::Px16,
        }
    }

    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::Px16 => Self::Px72,
            Self::Px20 => Self::Px16,
            Self::Px24 => Self::Px20,
            Self::Px32 => Self::Px24,
            Self::Px48 => Self::Px32,
            Self::Px72 => Self::Px48,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "16" | "small" => Ok(Self::Px16),
            "20" => Ok(Self::Px20),
            "24" | "medium" => Ok(Self::Px24),
            "32" | "large" => Ok(Self::Px32),
            "48" | "xlarge" | "extra-large" | "extra_large" => Ok(Self::Px48),
            "72" => Ok(Self::Px72),
            other => Err(format!("unsupported book_font_size value {other:?}")),
        }
    }
}

/// Reader-specific body font family. Built-in Latin strikes remain ASCII-only;
/// CJK uses SD TTF/OTF when present and the embedded Unifont GB2312 fallback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BookFont {
    Inter,
    AtkinsonHyperlegible,
    #[default]
    Serif,
    Literata,
    CjkUnifont,
    SdCjk,
}

impl BookFont {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Inter => "Inter",
            Self::AtkinsonHyperlegible => "Atkinson",
            Self::Serif => "Serif",
            Self::Literata => "Literata",
            Self::CjkUnifont => "CJK Unifont",
            Self::SdCjk => "SD CJK",
        }
    }

    #[must_use]
    pub fn display_label(self, sd_file: Option<&str>) -> String {
        match (self, sd_file) {
            (Self::SdCjk, Some(name)) => name
                .rsplit_once('.')
                .map(|(stem, _)| stem.to_string())
                .unwrap_or_else(|| name.to_string()),
            _ => self.label().to_string(),
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Inter => "inter",
            Self::AtkinsonHyperlegible => "atkinson-hyperlegible",
            Self::Serif => "serif",
            Self::Literata => "literata",
            Self::CjkUnifont => "cjk-unifont",
            Self::SdCjk => "sd-cjk",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        let trimmed = value.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("sd:") || lower.starts_with("sd-cjk") {
            return Ok(Self::SdCjk);
        }
        match lower.as_str() {
            "inter" => Ok(Self::Inter),
            "atkinson" | "atkinson-hyperlegible" | "atkinson_hyperlegible" => {
                Ok(Self::AtkinsonHyperlegible)
            }
            "serif" | "dejavu-serif" => Ok(Self::Serif),
            "literata" => Ok(Self::Literata),
            "cjk" | "cjk-unifont" | "unifont" | "gb2312" => Ok(Self::CjkUnifont),
            other => Err(format!("unsupported book_font value {other:?}")),
        }
    }
}

/// Reader paragraph alignment. Justified is the default e-book presentation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ParagraphAlignment {
    #[default]
    Justified,
    Left,
    Center,
    Right,
}

impl ParagraphAlignment {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Justified => "Justified",
            Self::Left => "Left",
            Self::Center => "Center",
            Self::Right => "Right",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Justified => "justified",
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Justified => Self::Left,
            Self::Left => Self::Center,
            Self::Center => Self::Right,
            Self::Right => Self::Justified,
        }
    }

    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::Justified => Self::Right,
            Self::Left => Self::Justified,
            Self::Center => Self::Left,
            Self::Right => Self::Center,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "justified" | "justify" => Ok(Self::Justified),
            "left" => Ok(Self::Left),
            "center" | "centred" => Ok(Self::Center),
            "right" => Ok(Self::Right),
            other => Err(format!("unsupported paragraph_alignment value {other:?}")),
        }
    }
}

/// Layout dimensions affecting TXT pagination and cache fingerprints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReaderLayout {
    pub chars_per_line: usize,
    pub lines_per_page: usize,
    pub max_line_width_px: i32,
    pub ascii_advance_px: i32,
    pub font_size_px: u8,
    pub orientation: ReaderOrientation,
    pub font_size: BookFontSize,
    pub book_font: BookFont,
    pub paragraph_alignment: ParagraphAlignment,
}

/// Reader-owned preference file persisted as `/RUSTMIX/READER/PREFS.TXT`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReaderPreferences {
    pub theme: ReadingTheme,
    pub orientation: ReaderOrientation,
    pub font_size: BookFontSize,
    pub book_font: BookFont,
    pub paragraph_alignment: ParagraphAlignment,
    pub show_progress: bool,
    sd_cjk_file: [u8; 13],
}

impl Default for ReaderPreferences {
    fn default() -> Self {
        Self {
            theme: ReadingTheme::Classic,
            orientation: ReaderOrientation::Portrait,
            font_size: BookFontSize::Px24,
            book_font: BookFont::Serif,
            paragraph_alignment: ParagraphAlignment::Justified,
            show_progress: true,
            sd_cjk_file: [0; 13],
        }
    }
}

impl ReaderPreferences {
    #[must_use]
    pub fn sd_cjk_file_name(&self) -> Option<&str> {
        let end = self
            .sd_cjk_file
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.sd_cjk_file.len());
        if end == 0 {
            None
        } else {
            core::str::from_utf8(&self.sd_cjk_file[..end]).ok()
        }
    }

    pub fn set_sd_cjk_file_name(&mut self, name: Option<&str>) {
        self.sd_cjk_file = [0; 13];
        let Some(name) = name else {
            return;
        };
        let bytes = name.as_bytes();
        let len = bytes.len().min(self.sd_cjk_file.len());
        self.sd_cjk_file[..len].copy_from_slice(&bytes[..len]);
    }

    pub fn apply_parsed_book_font(&mut self, font: BookFont, raw: &str) {
        self.book_font = font;
        if font == BookFont::SdCjk {
            let file = raw
                .trim()
                .split_once(':')
                .map(|(_, name)| name.trim())
                .filter(|name| !name.is_empty());
            self.set_sd_cjk_file_name(file);
        } else {
            self.set_sd_cjk_file_name(None);
        }
    }

    #[must_use]
    pub fn nvs_face_marker(&self) -> String {
        if self.book_font == BookFont::SdCjk {
            if let Some(name) = self.sd_cjk_file_name() {
                return format!("sd:{name}");
            }
        }
        self.book_font.marker().to_string()
    }

    #[must_use]
    pub fn layout(self) -> ReaderLayout {
        let px = self.font_size.pixels();
        let (width_px, height_px) = match self.orientation {
            ReaderOrientation::Portrait => (432, 594),
            ReaderOrientation::Landscape => (752, 300),
        };
        let line_step = i32::from(px) + i32::from(px) / 4 + 2;
        let lines_per_page = (height_px / line_step).max(4) as usize;
        let ascii_advance_px = match self.book_font {
            BookFont::Serif | BookFont::Literata => (i32::from(px) * 10 / 20).max(6),
            BookFont::CjkUnifont | BookFont::SdCjk => (i32::from(px) / 2).max(6),
            _ => (i32::from(px) * 11 / 20).max(6),
        };
        let chars_per_line = (width_px / ascii_advance_px).max(8) as usize;
        ReaderLayout {
            chars_per_line,
            lines_per_page,
            max_line_width_px: width_px,
            ascii_advance_px,
            font_size_px: px,
            orientation: self.orientation,
            font_size: self.font_size,
            book_font: self.book_font,
            paragraph_alignment: self.paragraph_alignment,
        }
    }

    #[must_use]
    pub fn serialized(self) -> String {
        let show_progress = if self.show_progress { "true" } else { "false" };
        format!(
            "version={}\ntheme={}\norientation={}\nfont_size={}\nbook_font={}\nparagraph_alignment={}\nshow_progress={}\n",
            READER_PREFS_VERSION,
            self.theme.marker(),
            self.orientation.marker(),
            self.font_size.marker(),
            self.nvs_face_marker(),
            self.paragraph_alignment.marker(),
            show_progress,
        )
    }

    fn parse(text: &str) -> Result<Self, String> {
        let mut prefs = Self::default();
        let mut version = None;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| "Reader preference line must contain '='".to_string())?;
            match key.trim() {
                "version" => version = Some(value.trim().to_string()),
                "theme" => prefs.theme = ReadingTheme::parse(value)?,
                "orientation" => prefs.orientation = ReaderOrientation::parse(value)?,
                "font_size" => prefs.font_size = BookFontSize::parse(value)?,
                "book_font" => {
                    let parsed = BookFont::parse(value)?;
                    prefs.apply_parsed_book_font(parsed, value);
                }
                "paragraph_alignment" => {
                    prefs.paragraph_alignment = ParagraphAlignment::parse(value)?
                }
                "show_progress" => {
                    prefs.show_progress = match value.trim() {
                        "true" => true,
                        "false" => false,
                        _ => return Err("show_progress must be true or false".into()),
                    }
                }
                other => return Err(format!("unsupported Reader preference key {other:?}")),
            }
        }
        if version.as_deref() != Some(READER_PREFS_VERSION) {
            return Err("unsupported Reader preference version".into());
        }
        Ok(prefs)
    }
}

/// Coarse stages used by the e-paper loading screen. The runtime advances only
/// at meaningful boundaries so progress remains visible without excessive
/// refreshes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReaderLoadingStage {
    OpeningFile,
    InspectingEpubArchive,
    ReadingEpubPackage,
    LoadingEpubSpine,
    DetectingEncoding,
    LoadingSavedPosition,
    UpdatingLayout,
    BuildingFirstPage,
    IndexingNearbyPages,
    Ready,
    UnsupportedEpub,
    Failed,
}

impl ReaderLoadingStage {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpeningFile => "Opening file",
            Self::InspectingEpubArchive => "Inspecting EPUB archive",
            Self::ReadingEpubPackage => "Reading EPUB package",
            Self::LoadingEpubSpine => "Loading EPUB spine",
            Self::DetectingEncoding => "Detecting text encoding",
            Self::LoadingSavedPosition => "Loading saved position",
            Self::UpdatingLayout => "Updating layout cache",
            Self::BuildingFirstPage => "Building first page",
            Self::IndexingNearbyPages => "Caching nearby pages",
            Self::Ready => "Ready",
            Self::UnsupportedEpub => "Unsupported EPUB",
            Self::Failed => "Unable to open book",
        }
    }

    #[must_use]
    pub const fn progress(self) -> u8 {
        match self {
            Self::OpeningFile => 10,
            Self::InspectingEpubArchive => 20,
            Self::ReadingEpubPackage => 32,
            Self::LoadingEpubSpine => 44,
            Self::DetectingEncoding => 25,
            Self::LoadingSavedPosition => 40,
            Self::UpdatingLayout => 45,
            Self::BuildingFirstPage => 55,
            Self::IndexingNearbyPages => 80,
            Self::Ready => 100,
            Self::UnsupportedEpub | Self::Failed => 100,
        }
    }
}

/// Pending staged book open retained while the loading screen is visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingReaderOpen {
    pub book: ReaderBook,
    pub stage: ReaderLoadingStage,
    pub encoding: Option<TextEncoding>,
    pub epub_document: Option<EpubDocument>,
    pub resume: Option<ReaderLocation>,
    pub message: String,
}

/// One wrapped Reader line. `paragraph_end` prevents Justified rendering from
/// stretching the final line of a paragraph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderPageLine {
    pub text: String,
    pub paragraph_end: bool,
}

/// One cached portrait page and its byte anchor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderCachedPage {
    /// Absolute book-page index, independent of a cache-recovery base offset.
    pub page_index: usize,
    pub byte_offset: u64,
    pub next_byte_offset: u64,
    pub lines: Vec<ReaderPageLine>,
}

/// SD-backed page-anchor cache. The cache is intentionally text-based and
/// bounded so corrupt records can be rejected without blocking book opening.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ReaderAnchorCache {
    fingerprint: u64,
    base_page: usize,
    offsets: Vec<u64>,
    indexed_through: u64,
    complete: bool,
}

/// One EPUB chapter's layout-specific page accounting. Anchors themselves live
/// in the SD page index; this record only keeps the chapter range and how many
/// pages have been recorded for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderEpubChapterPages {
    pub chapter_number: usize,
    pub text_offset: u64,
    pub text_end_offset: u64,
    pub first_page: Option<usize>,
    pub indexed_pages: usize,
    pub complete: bool,
}

/// Active Reader session. Generated page anchors and nearby rendered pages remain
/// bounded in RAM and are rebuilt lazily when the reader advances.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderSession {
    pub book: ReaderBook,
    pub encoding: TextEncoding,
    pub epub_document: Option<EpubDocument>,
    pub layout: ReaderLayout,
    /// Local index within page_offsets.
    pub current_page: usize,
    /// Absolute page index represented by page_offsets[0]. Normally zero. A
    /// non-zero value is allowed when STATE.TXT survives but a cache is absent.
    pub page_number_base: usize,
    pub page_offsets: Vec<u64>,
    pub indexed_through: u64,
    pub index_complete: bool,
    /// The safety cap was reached. Reading continues with approximate progress.
    pub index_truncated: bool,
    /// The reading window's absolute page numbers match the page index.
    pub epub_page_numbers_exact: bool,
    pub cache: Vec<ReaderCachedPage>,
    pub epub_chapter_pages: Vec<ReaderEpubChapterPages>,
    epub_anchor_index: Option<crate::epub_page_index::EpubAnchorIndex>,
}

impl ReaderSession {
    #[must_use]
    pub fn current_absolute_page(&self) -> usize {
        self.page_number_base.saturating_add(self.current_page)
    }

    #[must_use]
    pub fn source_size_bytes(&self) -> u64 {
        self.epub_document
            .as_ref()
            .map_or(self.book.size_bytes, EpubDocument::text_size_bytes)
    }

    #[must_use]
    pub fn content_badge(&self) -> &'static str {
        self.book.format.badge()
    }

    #[must_use]
    pub fn toc_entries(&self) -> &[EpubTocEntry] {
        self.epub_document
            .as_ref()
            .map_or(&[], |document| document.toc.as_slice())
    }

    #[must_use]
    pub fn current_cached_page(&self) -> Option<&ReaderCachedPage> {
        let absolute = self.current_absolute_page();
        self.cache.iter().find(|page| page.page_index == absolute)
    }

    #[must_use]
    pub fn current_location(&self) -> ReaderLocation {
        let byte_offset = self
            .page_offsets
            .get(self.current_page)
            .copied()
            .or_else(|| self.current_cached_page().map(|page| page.byte_offset))
            .unwrap_or(0);
        ReaderLocation {
            path: self.book.path.clone(),
            title: self.book.title.clone(),
            format: self.book.format,
            size_bytes: self.book.size_bytes,
            modified_seconds: self.book.modified_seconds,
            page_index: self.current_absolute_page(),
            byte_offset,
            epub_chapter: self.epub_chapter_page_label_for_offset(byte_offset),
        }
    }

    #[must_use]
    pub fn indexed_page_count(&self) -> usize {
        self.epub_anchor_index
            .as_ref()
            .map_or(self.page_offsets.len(), |index| index.page_count())
    }

    #[must_use]
    pub fn epub_index_on_sd(&self) -> bool {
        self.epub_anchor_index
            .as_ref()
            .is_some_and(|index| index.on_sd())
    }

    #[must_use]
    pub fn progress_percent(&self) -> u8 {
        let source_size = self.source_size_bytes();
        if source_size == 0 {
            return 100;
        }
        if self.index_truncated {
            let offset = self
                .page_offsets
                .get(self.current_page)
                .copied()
                .unwrap_or(self.indexed_through);
            return ((offset.saturating_mul(100) / source_size).min(100)) as u8;
        }
        ((self.indexed_through.saturating_mul(100) / source_size).min(100)) as u8
    }

    #[must_use]
    pub fn page_label(&self) -> String {
        let current = self.current_absolute_page() + 1;
        if self.index_truncated {
            let approx = self.approximate_total_pages().max(current);
            format!("{current}/~{approx}")
        } else if self.index_complete {
            let total = self.known_page_total().max(current);
            format!("{current}/{total}")
        } else {
            format!("{current}+")
        }
    }

    #[must_use]
    fn known_page_total(&self) -> usize {
        let window = self
            .page_number_base
            .saturating_add(self.page_offsets.len());
        self.epub_anchor_index
            .as_ref()
            .map(|index| index.page_count().max(window))
            .unwrap_or(window)
    }

    #[must_use]
    fn approximate_total_pages(&self) -> usize {
        let indexed_pages = self.indexed_page_count();
        let through = self.indexed_through.max(1);
        let source = self.source_size_bytes().max(1);
        if indexed_pages == 0 {
            return self.current_absolute_page() + 1;
        }
        let scaled = (indexed_pages as u64).saturating_mul(source) / through;
        usize::try_from(scaled.max(indexed_pages as u64)).unwrap_or(usize::MAX)
    }

    /// Product-facing page label. TXT keeps the accepted book-relative label;
    /// EPUB uses a chapter-relative label as requested by the Reader UI.
    #[must_use]
    pub fn display_page_label(&self) -> String {
        self.current_epub_chapter_page_label().map_or_else(
            || format!("PAGE {}", self.page_label()),
            |chapter| {
                format!(
                    "CH {}  PAGE {}",
                    chapter.chapter_number,
                    chapter.page_text()
                )
            },
        )
    }

    #[must_use]
    pub fn current_epub_chapter_page_label(&self) -> Option<ReaderChapterPageLabel> {
        let offset = self
            .page_offsets
            .get(self.current_page)
            .copied()
            .or_else(|| self.current_cached_page().map(|page| page.byte_offset))?;
        self.epub_chapter_page_label_for_offset(offset)
    }

    #[must_use]
    pub fn epub_chapter_page_label_for_offset(
        &self,
        offset: u64,
    ) -> Option<ReaderChapterPageLabel> {
        let source_size = self.source_size_bytes();
        let (chapter_number, text_offset, text_end_offset, first_page, indexed_pages, complete) = {
            let chapter = self.epub_chapter_pages.iter().find(|chapter| {
                offset >= chapter.text_offset
                    && (offset < chapter.text_end_offset
                        || (offset == chapter.text_end_offset
                            && chapter.text_end_offset == source_size))
            })?;
            (
                chapter.chapter_number,
                chapter.text_offset,
                chapter.text_end_offset,
                chapter.first_page,
                chapter.indexed_pages,
                chapter.complete,
            )
        };
        let located = self
            .epub_anchor_index
            .as_ref()
            .and_then(|index| index.page_containing(offset));
        let indexed_total = self.indexed_page_count();
        let through = self.indexed_through;
        if let (Some(page), Some(first)) = (located, first_page) {
            let page_number = page.saturating_sub(first).saturating_add(1);
            if complete {
                return Some(ReaderChapterPageLabel {
                    chapter_number,
                    page_number,
                    page_count: indexed_pages.max(1),
                    approximate: false,
                });
            }
            if self.index_truncated {
                let (_, estimated) = estimate_chapter_pages(
                    text_offset,
                    text_end_offset,
                    offset,
                    indexed_total,
                    through,
                );
                return Some(ReaderChapterPageLabel {
                    chapter_number,
                    page_number,
                    page_count: estimated.max(page_number),
                    approximate: true,
                });
            }
            return Some(ReaderChapterPageLabel {
                chapter_number,
                page_number,
                page_count: 0,
                approximate: true,
            });
        }
        if self.index_truncated {
            let (page_number, page_count) = estimate_chapter_pages(
                text_offset,
                text_end_offset,
                offset,
                indexed_total,
                through,
            );
            return Some(ReaderChapterPageLabel {
                chapter_number,
                page_number,
                page_count,
                approximate: true,
            });
        }
        Some(ReaderChapterPageLabel {
            chapter_number,
            page_number: 1,
            page_count: 0,
            approximate: true,
        })
    }

    fn push_cached_page(&mut self, page: ReaderCachedPage) {
        if let Some(existing) = self
            .cache
            .iter_mut()
            .find(|cached| cached.page_index == page.page_index)
        {
            *existing = page;
            return;
        }
        self.cache.push(page);
        self.cache.sort_by_key(|page| page.page_index);
        while self.cache.len() > READER_NEARBY_PAGE_CACHE {
            let current = self.current_absolute_page();
            let remove = if current.saturating_sub(self.cache[0].page_index)
                > self
                    .cache
                    .last()
                    .map_or(0, |page| page.page_index.saturating_sub(current))
            {
                0
            } else {
                self.cache.len() - 1
            };
            self.cache.remove(remove);
        }
    }

    fn ensure_page_cached(&mut self, local_page_index: usize) -> Result<(), String> {
        let absolute = self.page_number_base.saturating_add(local_page_index);
        if self.cache.iter().any(|page| page.page_index == absolute) {
            return Ok(());
        }
        let offset = *self
            .page_offsets
            .get(local_page_index)
            .ok_or_else(|| "page anchor is not indexed yet".to_string())?;
        let page = read_reader_page(
            &self.book,
            self.encoding,
            self.layout,
            self.epub_document.as_ref(),
            offset,
            absolute,
        )?;
        self.push_cached_page(page);
        Ok(())
    }

    fn index_one_page(&mut self) -> Result<bool, String> {
        if self.index_complete {
            return Ok(false);
        }
        let absolute_page = self
            .page_number_base
            .saturating_add(self.page_offsets.len());
        let offset = self.indexed_through;
        let source_size = self.source_size_bytes();
        if offset >= source_size {
            self.index_complete = true;
            return Ok(false);
        }
        let page = read_reader_page(
            &self.book,
            self.encoding,
            self.layout,
            self.epub_document.as_ref(),
            offset,
            absolute_page,
        )?;
        if page.next_byte_offset <= offset {
            self.index_complete = true;
            return Ok(false);
        }
        self.page_offsets.push(offset);
        self.indexed_through = page.next_byte_offset;
        self.index_complete = self.indexed_through >= source_size;
        self.push_cached_page(page);
        Ok(true)
    }

    pub fn next_page(&mut self) -> Result<(), String> {
        if self.epub_anchor_index.is_some() {
            return self.next_epub_page();
        }
        let target = self.current_page.saturating_add(1);
        while target >= self.page_offsets.len() && !self.index_complete {
            self.index_one_page()?;
        }
        if target < self.page_offsets.len() {
            self.current_page = target;
            self.ensure_page_cached(target)?;
        }
        Ok(())
    }

    pub fn previous_page(&mut self) -> Result<(), String> {
        if self.epub_anchor_index.is_some() {
            return self.previous_epub_page();
        }
        if self.current_page > 0 {
            self.current_page -= 1;
            self.ensure_page_cached(self.current_page)?;
        }
        Ok(())
    }

    fn next_epub_page(&mut self) -> Result<(), String> {
        let target = self.current_page.saturating_add(1);
        if target < self.page_offsets.len() {
            self.current_page = target;
            self.ensure_page_cached(target)?;
            return Ok(());
        }
        let current_offset = self
            .page_offsets
            .get(self.current_page)
            .copied()
            .unwrap_or(0);
        if let Some(next_offset) = self.indexed_successor(current_offset) {
            self.push_reading_anchor(next_offset);
            self.ensure_page_cached(self.current_page)?;
            return Ok(());
        }
        let next_start = self.current_page_end_offset()?;
        if next_start >= self.source_size_bytes() {
            return Ok(());
        }
        let absolute = self
            .page_number_base
            .saturating_add(self.page_offsets.len());
        let document = self
            .epub_document
            .as_ref()
            .ok_or_else(|| "EPUB document is unavailable".to_string())?;
        let page = read_epub_page(document, self.layout, next_start, absolute)?;
        if page.next_byte_offset <= next_start && page.byte_offset == current_offset {
            return Ok(());
        }
        self.push_reading_anchor(page.byte_offset);
        self.push_cached_page(page);
        Ok(())
    }

    fn previous_epub_page(&mut self) -> Result<(), String> {
        if self.current_page > 0 {
            self.current_page -= 1;
            self.ensure_page_cached(self.current_page)?;
            return Ok(());
        }
        let front = self.page_offsets.first().copied().unwrap_or(0);
        let Some(page) = self
            .epub_anchor_index
            .as_ref()
            .and_then(|index| index.page_containing(front))
        else {
            return Ok(());
        };
        if page == 0 {
            return Ok(());
        }
        let Some(offset) = self
            .epub_anchor_index
            .as_ref()
            .and_then(|index| index.anchor(page - 1))
        else {
            return Ok(());
        };
        self.page_offsets.insert(0, offset);
        self.page_number_base = page - 1;
        self.epub_page_numbers_exact = true;
        while self.page_offsets.len() > READER_EPUB_READING_WINDOW {
            self.page_offsets.pop();
        }
        self.current_page = 0;
        self.ensure_page_cached(0)?;
        Ok(())
    }

    fn indexed_successor(&self, offset: u64) -> Option<u64> {
        let index = self.epub_anchor_index.as_ref()?;
        let page = index.page_containing(offset)?;
        let anchor = index.anchor(page)?;
        if anchor != offset {
            return None;
        }
        index.anchor(page + 1)
    }

    fn current_page_end_offset(&mut self) -> Result<u64, String> {
        let absolute = self.current_absolute_page();
        if let Some(page) = self.cache.iter().find(|page| page.page_index == absolute) {
            return Ok(page.next_byte_offset);
        }
        self.ensure_page_cached(self.current_page)?;
        self.cache
            .iter()
            .find(|page| page.page_index == self.current_absolute_page())
            .map(|page| page.next_byte_offset)
            .ok_or_else(|| "current page is not cached".to_string())
    }

    fn push_reading_anchor(&mut self, offset: u64) {
        self.page_offsets.push(offset);
        self.current_page = self.page_offsets.len() - 1;
        while self.page_offsets.len() > READER_EPUB_READING_WINDOW {
            self.page_offsets.remove(0);
            self.page_number_base = self.page_number_base.saturating_add(1);
            self.current_page = self.current_page.saturating_sub(1);
        }
    }

    fn index_epub_batch(&mut self, budget: usize) -> Result<usize, String> {
        if self.epub_anchor_index.is_none() {
            return Ok(0);
        }
        let layout = self.layout;
        let mut added = 0_usize;
        let mut dirty = false;
        while added < budget {
            let step = {
                let index = self.epub_anchor_index.as_ref().unwrap();
                if index.is_finished() {
                    None
                } else {
                    let document = self
                        .epub_document
                        .as_ref()
                        .ok_or_else(|| "EPUB document is unavailable".to_string())?;
                    let source_size = document.text_size_bytes();
                    let start = index.next_anchor();
                    if start >= source_size {
                        Some(None)
                    } else {
                        let chapter_end = document
                            .chapter_for_offset(start)
                            .map_or(source_size, |chapter| chapter.text_end_offset);
                        let page = read_epub_page_until(document, layout, start, 0, chapter_end)?;
                        Some(Some((start, page.next_byte_offset)))
                    }
                }
            };
            let Some(step) = step else {
                break;
            };
            let Some((start, next)) = step else {
                if let Some(index) = self.epub_anchor_index.as_mut() {
                    index.mark_complete();
                    dirty = true;
                }
                break;
            };
            if next <= start {
                let source_size = self.source_size_bytes();
                if let Some(index) = self.epub_anchor_index.as_mut() {
                    if next >= source_size {
                        index.mark_complete();
                    } else {
                        index.mark_truncated();
                    }
                    dirty = true;
                }
                break;
            }
            let status = self
                .epub_anchor_index
                .as_mut()
                .unwrap()
                .append(start, next)?;
            dirty = true;
            added += 1;
            if matches!(
                status,
                crate::epub_page_index::AppendStatus::Complete
                    | crate::epub_page_index::AppendStatus::Truncated
            ) {
                break;
            }
            if added % READER_EPUB_INDEX_YIELD_EVERY_PAGES == 0 {
                std::thread::sleep(Duration::from_millis(READER_EPUB_INDEX_YIELD_MILLIS));
            }
        }
        if dirty {
            let sync = self
                .epub_anchor_index
                .as_ref()
                .is_some_and(|index| index.is_finished() || index.page_count() % 64 == 0);
            if let Some(index) = self.epub_anchor_index.as_mut() {
                if let Err(error) = index.flush(sync) {
                    index.mark_truncated();
                    index.warning = Some(error);
                }
            }
        }
        self.sync_epub_index_flags();
        self.reconcile_epub_reading_page();
        Ok(added)
    }

    fn sync_epub_index_flags(&mut self) {
        let Some(index) = self.epub_anchor_index.as_ref() else {
            return;
        };
        self.indexed_through = index.next_anchor();
        self.index_complete = index.is_complete();
        self.index_truncated = index.is_truncated();
        self.epub_chapter_pages = index
            .chapters()
            .iter()
            .map(|chapter| ReaderEpubChapterPages {
                chapter_number: chapter.chapter_number,
                text_offset: chapter.text_offset,
                text_end_offset: chapter.text_end_offset,
                first_page: chapter.first_page,
                indexed_pages: chapter.page_count,
                complete: chapter.complete,
            })
            .collect();
    }

    fn reconcile_epub_reading_page(&mut self) {
        let Some(front) = self.page_offsets.first().copied() else {
            return;
        };
        let Some(page) = self
            .epub_anchor_index
            .as_ref()
            .and_then(|index| index.page_containing(front))
        else {
            return;
        };
        let Some(anchor) = self
            .epub_anchor_index
            .as_ref()
            .and_then(|index| index.anchor(page))
        else {
            return;
        };
        if anchor != front {
            return;
        }
        let old_base = self.page_number_base;
        if old_base != page {
            let delta = page as isize - old_base as isize;
            for cached in &mut self.cache {
                let adjusted = cached.page_index as isize + delta;
                if adjusted >= 0 {
                    cached.page_index = adjusted as usize;
                }
            }
            self.page_number_base = page;
        }
        self.epub_page_numbers_exact = true;
    }

    #[must_use]
    fn anchor_cache(&self) -> Option<ReaderAnchorCache> {
        if self.book.format != BookFormat::Text {
            return None;
        }
        Some(ReaderAnchorCache {
            fingerprint: book_fingerprint(&self.book, self.layout),
            base_page: self.page_number_base,
            offsets: self.page_offsets.clone(),
            indexed_through: self.indexed_through,
            complete: self.index_complete,
        })
    }
}

/// Reader Options action rows. Editable values live on the separate
/// Reading Preferences editor so menu controls match the rest of the firmware.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReaderOption {
    Bookmark,
    Bookmarks,
    TableOfContents,
    ReadingPreferences,
    ClearGhosting,
    GoToLibrary,
    GoHome,
}

impl ReaderOption {
    pub const ALL: [Self; 7] = [
        Self::Bookmark,
        Self::Bookmarks,
        Self::TableOfContents,
        Self::ReadingPreferences,
        Self::ClearGhosting,
        Self::GoToLibrary,
        Self::GoHome,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bookmark => "Add / Remove Bookmark",
            Self::Bookmarks => "Bookmarks",
            Self::TableOfContents => "Table of Contents",
            Self::ReadingPreferences => "Reading Preferences",
            Self::ClearGhosting => "Clear Ghosting",
            Self::GoToLibrary => "Go to Library",
            Self::GoHome => "Go Home",
        }
    }

    #[must_use]
    pub const fn badge(self) -> &'static str {
        match self {
            Self::Bookmark => "TOGGLE",
            Self::Bookmarks => "LIST",
            Self::TableOfContents => "NONE",
            Self::ReadingPreferences => ">>>",
            Self::ClearGhosting => "RUN",
            Self::GoToLibrary | Self::GoHome => ">>>",
        }
    }
}

/// Reading Preferences editor rows. UP/DOWN changes the active value and
/// SELECT advances to the next row, matching the firmware editor convention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadingPreference {
    ReadingTheme,
    Orientation,
    BookFontSize,
    BookFont,
    ParagraphAlignment,
    ShowProgress,
}

impl ReadingPreference {
    pub const ALL: [Self; 6] = [
        Self::BookFontSize,
        Self::BookFont,
        Self::ReadingTheme,
        Self::Orientation,
        Self::ParagraphAlignment,
        Self::ShowProgress,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReadingTheme => "Reading Theme",
            Self::Orientation => "Orientation",
            Self::BookFontSize => "Book Font Size",
            Self::BookFont => "Book Font",
            Self::ParagraphAlignment => "Paragraph Alignment",
            Self::ShowProgress => "Show Progress",
        }
    }
}

/// Coarse background tick result used by main.rs to refresh only meaningful
/// loading-screen transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReaderTickOutcome {
    None,
    LoadingStageChanged,
    FirstPageReady,
    BackgroundCacheAdvanced,
    Failed,
}

/// Non-fatal Reader persistence startup report.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReaderPersistenceReport {
    pub state_loaded: bool,
    pub preferences_loaded: bool,
    pub position_count: usize,
    pub recent_count: usize,
    pub bookmark_count: usize,
    pub warning: Option<String>,
}

/// Hardware-independent Reader UI state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderUiState {
    pub books_root: String,
    pub state_root: String,
    pub books: Vec<ReaderBook>,
    pub positions: Vec<ReaderLocation>,
    pub recent: Vec<ReaderLocation>,
    pub bookmarks: Vec<ReaderLocation>,
    pub resume: Option<ReaderLocation>,
    pub preferences: ReaderPreferences,
    pub library_error: Option<String>,
    pub persistence_warning: Option<String>,
    pub library_tab: ReaderLibraryTab,
    /// Row zero is the explicit tab-control row; book rows begin at one.
    pub library_selected: usize,
    pub bookmarks_selected: usize,
    pub toc_selected: usize,
    pub loading: Option<PendingReaderOpen>,
    pub session: Option<ReaderSession>,
    pub options_selected: usize,
    pub preferences_selected: usize,
    preferences_layout_dirty: bool,
    pub last_message: Option<String>,
    persistence_event: Option<String>,
    last_persistence_event: Option<String>,
    clear_ghost_requested: bool,
    pub sd_cjk_faces: Vec<crate::fonts::SdFontFace>,
    epub_page_anchor_limit: usize,
    epub_index_bytes_limit: usize,
}

impl Default for ReaderUiState {
    fn default() -> Self {
        Self {
            books_root: READER_BOOKS_DIRECTORY.into(),
            state_root: READER_STATE_DIRECTORY.into(),
            books: Vec::new(),
            positions: Vec::new(),
            recent: Vec::new(),
            bookmarks: Vec::new(),
            resume: None,
            preferences: ReaderPreferences::default(),
            library_error: None,
            persistence_warning: None,
            library_tab: ReaderLibraryTab::default(),
            library_selected: 0,
            bookmarks_selected: 0,
            toc_selected: 0,
            loading: None,
            session: None,
            options_selected: 0,
            preferences_selected: 0,
            preferences_layout_dirty: false,
            last_message: None,
            persistence_event: None,
            last_persistence_event: None,
            clear_ghost_requested: false,
            sd_cjk_faces: Vec::new(),
            epub_page_anchor_limit: READER_EPUB_PAGE_ANCHOR_LIMIT,
            epub_index_bytes_limit: READER_EPUB_ANCHOR_INDEX_BYTES_LIMIT,
        }
    }
}

impl ReaderUiState {
    pub fn refresh_font_catalog(&mut self) {
        self.sd_cjk_faces = crate::fonts::sd_faces();
        if self.preferences.book_font == BookFont::SdCjk {
            let wanted = self.preferences.sd_cjk_file_name();
            let matched = wanted.and_then(|name| {
                self.sd_cjk_faces
                    .iter()
                    .find(|face| face.file_name.eq_ignore_ascii_case(name))
            });
            if let Some(face) = matched {
                self.preferences.set_sd_cjk_file_name(Some(&face.file_name));
                crate::fonts::set_preferred_sd_file(Some(&face.file_name));
            } else if self.sd_cjk_faces.is_empty() {
                self.preferences.book_font = BookFont::CjkUnifont;
                self.preferences.set_sd_cjk_file_name(None);
                crate::fonts::set_preferred_sd_file(None);
            } else {
                let face = &self.sd_cjk_faces[0];
                self.preferences.set_sd_cjk_file_name(Some(&face.file_name));
                crate::fonts::set_preferred_sd_file(Some(&face.file_name));
            }
        } else {
            crate::fonts::set_preferred_sd_file(self.preferences.sd_cjk_file_name());
        }
    }

    #[must_use]
    pub fn book_font_display_label(&self) -> String {
        self.preferences
            .book_font
            .display_label(self.preferences.sd_cjk_file_name())
    }

    #[must_use]
    pub fn with_books_root(root: impl Into<String>) -> Self {
        Self {
            books_root: root.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_roots(books_root: impl Into<String>, state_root: impl Into<String>) -> Self {
        Self {
            books_root: books_root.into(),
            state_root: state_root.into(),
            ..Self::default()
        }
    }

    #[cfg(test)]
    fn set_epub_index_limits(&mut self, pages: usize, bytes: usize) {
        self.epub_page_anchor_limit = pages.max(1);
        self.epub_index_bytes_limit = bytes.max(crate::epub_page_index::ANCHOR_BASE + 8);
    }

    /// Load persisted state without making startup dependent on removable
    /// storage. Corrupt records are ignored and reported as a warning.
    pub fn load_persistent_state(&mut self) -> ReaderPersistenceReport {
        let mut warnings = Vec::new();
        let preferences_loaded = match load_preferences(&self.preferences_path()) {
            Ok(Some(preferences)) => {
                self.preferences = preferences;
                true
            }
            Ok(None) => false,
            Err(error) => {
                warnings.push(format!("PREFS.TXT: {error}"));
                false
            }
        };
        if !preferences_loaded {
            let _ = crate::reader_nvs::load_reader_preferences_overlay(&mut self.preferences);
        }
        self.refresh_font_catalog();
        self.resume = match load_location_record(&self.state_path()) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!("STATE.TXT: {error}"));
                None
            }
        };
        self.positions = match self.load_positions_with_legacy_migration() {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!("POSITS.TXT: {error}"));
                Vec::new()
            }
        };
        self.recent = match load_location_list(&self.recent_path(), READER_RECENT_LIMIT) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!("RECENT.TXT: {error}"));
                Vec::new()
            }
        };
        self.bookmarks = match load_location_list(&self.bookmarks_path(), READER_BOOKMARK_LIMIT) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!("MARKS.TXT: {error}"));
                Vec::new()
            }
        };
        self.bookmarks_selected = self
            .bookmarks_selected
            .min(self.bookmarks.len().saturating_sub(1));
        let warning = if warnings.is_empty() {
            None
        } else {
            Some(warnings.join("; "))
        };
        self.persistence_warning = warning.clone();
        ReaderPersistenceReport {
            state_loaded: self.resume.is_some(),
            preferences_loaded,
            position_count: self.positions.len(),
            recent_count: self.recent.len(),
            bookmark_count: self.bookmarks.len(),
            warning,
        }
    }

    pub fn refresh_library(&mut self) {
        match scan_txt_library(&self.books_root) {
            Ok(books) => {
                self.books = books;
                self.library_error = None;
            }
            Err(error) => {
                self.books.clear();
                self.library_error = Some(error);
            }
        }
        self.library_selected = 0;
    }

    #[must_use]
    pub fn can_continue(&self) -> bool {
        self.session.is_some() || self.resume.is_some() || !self.recent.is_empty()
    }

    pub fn request_continue(&mut self) -> bool {
        let Some(location) = self.resume.clone().or_else(|| self.recent.first().cloned()) else {
            return false;
        };
        self.request_open_book(location.as_book(), Some(location));
        true
    }

    #[must_use]
    pub fn visible_entries(&self) -> Vec<ReaderLibraryEntry> {
        match self.library_tab {
            ReaderLibraryTab::Recent => self
                .recent
                .iter()
                .cloned()
                .map(|location| ReaderLibraryEntry {
                    book: location.as_book(),
                    location: Some(location),
                })
                .collect(),
            ReaderLibraryTab::Books | ReaderLibraryTab::Files => self
                .books
                .iter()
                .cloned()
                .map(|book| ReaderLibraryEntry {
                    location: self.saved_position_for_book(&book),
                    book,
                })
                .collect(),
            ReaderLibraryTab::Bookmarks => self
                .bookmarks
                .iter()
                .cloned()
                .map(|location| ReaderLibraryEntry {
                    book: location.as_book(),
                    location: Some(location),
                })
                .collect(),
        }
    }

    #[must_use]
    pub fn library_row_count(&self) -> usize {
        self.visible_entries().len().saturating_add(1)
    }

    pub fn apply_library_button(&mut self, event: ButtonEvent) -> bool {
        let count = self.library_row_count().max(1);
        match event {
            ButtonEvent::Up => {
                self.library_selected = self.library_selected.checked_sub(1).unwrap_or(count - 1);
                false
            }
            ButtonEvent::Down => {
                self.library_selected = (self.library_selected + 1) % count;
                false
            }
            ButtonEvent::Select if self.library_selected == 0 => {
                self.library_tab = self.library_tab.next();
                self.library_selected = 0;
                false
            }
            ButtonEvent::Select => self.request_open_visible(self.library_selected - 1),
        }
    }

    pub fn apply_bookmarks_button(&mut self, event: ButtonEvent) -> bool {
        if self.bookmarks.is_empty() {
            return false;
        }
        match event {
            ButtonEvent::Up => {
                self.bookmarks_selected = self
                    .bookmarks_selected
                    .checked_sub(1)
                    .unwrap_or(self.bookmarks.len() - 1);
                false
            }
            ButtonEvent::Down => {
                self.bookmarks_selected = (self.bookmarks_selected + 1) % self.bookmarks.len();
                false
            }
            ButtonEvent::Select => self.request_open_bookmark(self.bookmarks_selected),
        }
    }

    pub fn request_open_visible(&mut self, visible_index: usize) -> bool {
        let Some(entry) = self.visible_entries().get(visible_index).cloned() else {
            return false;
        };
        let resume = entry
            .location
            .or_else(|| self.saved_position_for_book(&entry.book))
            .or_else(|| {
                self.resume
                    .clone()
                    .filter(|location| location.matches_book(&entry.book))
            });
        self.request_open_book(entry.book, resume);
        true
    }

    #[must_use]
    fn saved_position_for_book(&self, book: &ReaderBook) -> Option<ReaderLocation> {
        self.positions
            .iter()
            .find(|location| location.matches_book(book))
            .cloned()
    }

    pub fn request_open_bookmark(&mut self, bookmark_index: usize) -> bool {
        let Some(location) = self.bookmarks.get(bookmark_index).cloned() else {
            return false;
        };
        self.request_open_book(location.as_book(), Some(location));
        true
    }

    fn request_open_book(&mut self, book: ReaderBook, resume: Option<ReaderLocation>) {
        self.release_active_session_for_open();
        self.loading = Some(PendingReaderOpen {
            book,
            stage: ReaderLoadingStage::OpeningFile,
            encoding: None,
            epub_document: None,
            resume,
            message: "Preparing reader...".into(),
        });
    }

    /// Persist and drop the previous session before a new book is parsed. EPUB
    /// documents retain flattened text and chapter anchors in RAM; keeping the
    /// old document alive while allocating the next parser-worker stack can
    /// exhaust the embedded heap after repeated book switches.
    fn release_active_session_for_open(&mut self) {
        if self.session.is_none() {
            return;
        }
        self.persist_current_session_best_effort();
        self.session = None;
        log::info!("rustmix-wave=reader-session-memory-release status=completed reason=book-open");
    }

    fn request_layout_rebuild(&mut self) -> bool {
        if self.session.is_none() {
            self.persist_preferences_best_effort();
            return false;
        }
        self.persist_current_session_best_effort();
        let Some(mut session) = self.session.take() else {
            return false;
        };
        let book = session.book.clone();
        let encoding = session.encoding;
        let resume = session.current_location();
        self.loading = Some(PendingReaderOpen {
            book,
            stage: ReaderLoadingStage::UpdatingLayout,
            encoding: Some(encoding),
            epub_document: session.epub_document.take(),
            resume: Some(resume),
            message: "Rebuilding the current page first...".into(),
        });
        log::info!(
            "rustmix-wave=reader-session-memory-release status=completed reason=layout-rebuild"
        );
        self.persist_preferences_best_effort();
        true
    }

    pub fn cancel_loading(&mut self) {
        self.loading = None;
        self.last_message = Some("Book opening cancelled".into());
    }

    #[must_use]
    pub fn loading_stage(&self) -> Option<ReaderLoadingStage> {
        self.loading.as_ref().map(|loading| loading.stage)
    }

    /// True while a book is still opening or the nearby-page index is incomplete.
    #[must_use]
    pub fn needs_background_tick(&self) -> bool {
        if self.loading.is_some() {
            return true;
        }
        self.session.as_ref().is_some_and(|session| {
            session.cache.len() < READER_NEARBY_PAGE_CACHE && !session.index_complete
        })
    }

    /// Flush the open page, bookmarks, and preferences before MCU deep sleep.
    pub fn persist_before_sleep(&mut self) {
        self.persist_current_session_best_effort();
        self.persist_bookmarks_best_effort();
        self.persist_preferences_best_effort();
    }

    pub fn tick(&mut self) -> ReaderTickOutcome {
        if let Some(mut loading) = self.loading.take() {
            let outcome = match loading.stage {
                ReaderLoadingStage::OpeningFile => {
                    loading.stage = match loading.book.format {
                        BookFormat::Text => ReaderLoadingStage::DetectingEncoding,
                        BookFormat::Epub => ReaderLoadingStage::InspectingEpubArchive,
                    };
                    loading.message = loading.stage.label().into();
                    ReaderTickOutcome::LoadingStageChanged
                }
                ReaderLoadingStage::InspectingEpubArchive => {
                    match open_epub_on_worker(&loading.book.path) {
                        Ok(document) => {
                            loading.message = format!(
                                "{} spine items / {} TOC entries",
                                document.spine_count,
                                document.toc.len()
                            );
                            loading.epub_document = Some(document);
                            loading.stage = ReaderLoadingStage::ReadingEpubPackage;
                            ReaderTickOutcome::LoadingStageChanged
                        }
                        Err(error) => {
                            loading.stage = ReaderLoadingStage::Failed;
                            loading.message = error;
                            ReaderTickOutcome::Failed
                        }
                    }
                }
                ReaderLoadingStage::ReadingEpubPackage => {
                    loading.stage = ReaderLoadingStage::LoadingEpubSpine;
                    loading.message = "EPUB package and navigation ready".into();
                    ReaderTickOutcome::LoadingStageChanged
                }
                ReaderLoadingStage::LoadingEpubSpine => {
                    loading.stage = if loading.resume.is_some() {
                        ReaderLoadingStage::LoadingSavedPosition
                    } else {
                        ReaderLoadingStage::BuildingFirstPage
                    };
                    loading.message = "Reflowable EPUB text ready".into();
                    ReaderTickOutcome::LoadingStageChanged
                }
                ReaderLoadingStage::DetectingEncoding => {
                    match detect_txt_encoding(&loading.book.path) {
                        Ok(encoding) => {
                            loading.encoding = Some(encoding);
                            loading.stage = if loading.resume.is_some() {
                                ReaderLoadingStage::LoadingSavedPosition
                            } else {
                                ReaderLoadingStage::BuildingFirstPage
                            };
                            loading.message = format!("{} detected", encoding.label());
                            ReaderTickOutcome::LoadingStageChanged
                        }
                        Err(error) => {
                            loading.stage = ReaderLoadingStage::Failed;
                            loading.message = error;
                            ReaderTickOutcome::Failed
                        }
                    }
                }
                ReaderLoadingStage::LoadingSavedPosition => {
                    loading.stage = ReaderLoadingStage::BuildingFirstPage;
                    loading.message = "Resume anchor ready".into();
                    ReaderTickOutcome::LoadingStageChanged
                }
                ReaderLoadingStage::UpdatingLayout => {
                    loading.stage = ReaderLoadingStage::BuildingFirstPage;
                    loading.message = "Layout cache update ready".into();
                    ReaderTickOutcome::LoadingStageChanged
                }
                ReaderLoadingStage::BuildingFirstPage => {
                    let encoding = loading.encoding.unwrap_or(TextEncoding::Utf8);
                    let session = match loading.book.format {
                        BookFormat::Text => {
                            self.open_txt_session(&loading.book, encoding, loading.resume.as_ref())
                        }
                        BookFormat::Epub => loading
                            .epub_document
                            .take()
                            .ok_or_else(|| "EPUB document is not staged".to_string())
                            .and_then(|document| {
                                self.open_epub_session(
                                    &loading.book,
                                    document,
                                    loading.resume.as_ref(),
                                )
                            }),
                    };
                    match session {
                        Ok(session) => {
                            self.session = Some(session);
                            self.last_message =
                                Some("Saved position ready; caching continues lazily".into());
                            self.persist_current_session_best_effort();
                            ReaderTickOutcome::FirstPageReady
                        }
                        Err(error) => {
                            loading.stage = ReaderLoadingStage::Failed;
                            loading.message = error;
                            ReaderTickOutcome::Failed
                        }
                    }
                }
                ReaderLoadingStage::UnsupportedEpub | ReaderLoadingStage::Failed => {
                    self.loading = Some(loading);
                    return ReaderTickOutcome::None;
                }
                ReaderLoadingStage::IndexingNearbyPages | ReaderLoadingStage::Ready => {
                    ReaderTickOutcome::None
                }
            };
            if !matches!(outcome, ReaderTickOutcome::FirstPageReady) {
                self.loading = Some(loading);
            }
            return outcome;
        }

        let (outcome, checkpoint) = if let Some(session) = self.session.as_mut() {
            if session.epub_anchor_index.is_some() {
                match session.index_epub_batch(READER_EPUB_BACKGROUND_INDEX_PAGES) {
                    Ok(0) => (ReaderTickOutcome::None, false),
                    Ok(_) => {
                        let checkpoint = session.index_complete
                            || session.index_truncated
                            || session.indexed_page_count() % 64 == 0;
                        (ReaderTickOutcome::BackgroundCacheAdvanced, checkpoint)
                    }
                    Err(error) => {
                        if let Some(index) = session.epub_anchor_index.as_mut() {
                            index.mark_truncated();
                            let _ = index.flush(false);
                        }
                        session.sync_epub_index_flags();
                        self.last_message = Some(error);
                        (ReaderTickOutcome::None, true)
                    }
                }
            } else if session.cache.len() < READER_NEARBY_PAGE_CACHE && !session.index_complete {
                match session.index_one_page() {
                    Ok(true) => (
                        ReaderTickOutcome::BackgroundCacheAdvanced,
                        session.page_offsets.len() % READER_CACHE_CHECKPOINT_PAGES == 0
                            || session.index_complete,
                    ),
                    Ok(false) => (ReaderTickOutcome::None, session.index_complete),
                    Err(error) => {
                        self.last_message = Some(error);
                        return ReaderTickOutcome::Failed;
                    }
                }
            } else {
                (ReaderTickOutcome::None, false)
            }
        } else {
            (ReaderTickOutcome::None, false)
        };
        if checkpoint {
            self.persist_anchor_cache_best_effort();
        }
        outcome
    }

    pub fn previous_page(&mut self) {
        if let Some(session) = self.session.as_mut() {
            if let Err(error) = session.previous_page() {
                self.last_message = Some(error);
                return;
            }
            self.persist_current_session_best_effort();
        }
    }

    pub fn next_page(&mut self) {
        if let Some(session) = self.session.as_mut() {
            if let Err(error) = session.next_page() {
                self.last_message = Some(error);
                return;
            }
            self.persist_current_session_best_effort();
        }
    }

    pub fn cycle_option_previous(&mut self) {
        self.options_selected = self
            .options_selected
            .checked_sub(1)
            .unwrap_or(ReaderOption::ALL.len() - 1);
    }

    pub fn cycle_option_next(&mut self) {
        self.options_selected = (self.options_selected + 1) % ReaderOption::ALL.len();
    }

    #[must_use]
    pub fn selected_option(&self) -> ReaderOption {
        ReaderOption::ALL[self.options_selected]
    }

    /// Resolve a bookmark's user-facing page label against the active layout
    /// when nearby anchors are available. The persisted byte offset remains the
    /// canonical bookmark authority; the stored page index is a safe fallback.
    #[must_use]
    pub fn bookmark_display_page(&self, bookmark: &ReaderLocation) -> usize {
        self.session
            .as_ref()
            .filter(|session| bookmark.matches_book(&session.book))
            .and_then(|session| {
                if let Some(page) = session
                    .epub_anchor_index
                    .as_ref()
                    .and_then(|index| index.page_containing(bookmark.byte_offset))
                {
                    return Some(page.saturating_add(1));
                }
                session
                    .page_offsets
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, offset)| **offset <= bookmark.byte_offset)
                    .map(|(index, _)| {
                        session
                            .page_number_base
                            .saturating_add(index)
                            .saturating_add(1)
                    })
            })
            .unwrap_or_else(|| bookmark.page_index.saturating_add(1))
    }

    /// Resolve an EPUB bookmark against the active layout when possible and
    /// otherwise use the persisted chapter-relative fallback stored in MARKS.TXT.
    #[must_use]
    pub fn bookmark_display_chapter_page(
        &self,
        bookmark: &ReaderLocation,
    ) -> Option<ReaderChapterPageLabel> {
        if bookmark.format != BookFormat::Epub {
            return None;
        }
        self.session
            .as_ref()
            .filter(|session| bookmark.matches_book(&session.book))
            .and_then(|session| session.epub_chapter_page_label_for_offset(bookmark.byte_offset))
            .or_else(|| bookmark.epub_chapter.clone())
    }

    #[must_use]
    pub fn has_structured_toc(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| !session.toc_entries().is_empty())
    }

    #[must_use]
    pub fn toc_entries(&self) -> &[EpubTocEntry] {
        self.session
            .as_ref()
            .map_or(&[], ReaderSession::toc_entries)
    }

    pub fn apply_toc_button(&mut self, event: ButtonEvent) -> bool {
        let count = self.toc_entries().len();
        if count == 0 {
            return false;
        }
        match event {
            ButtonEvent::Up => {
                self.toc_selected = self.toc_selected.checked_sub(1).unwrap_or(count - 1);
                false
            }
            ButtonEvent::Down => {
                self.toc_selected = (self.toc_selected + 1) % count;
                false
            }
            ButtonEvent::Select => self.open_selected_toc_entry(),
        }
    }

    fn open_selected_toc_entry(&mut self) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        let Some(entry) = session
            .epub_document
            .as_ref()
            .and_then(|document| document.toc.get(self.toc_selected))
            .cloned()
        else {
            return false;
        };
        let page = {
            let Some(document) = session.epub_document.as_ref() else {
                return false;
            };
            read_epub_page(document, session.layout, entry.text_offset, 0)
                .map(|page| (page, document.text_size_bytes()))
        };
        session.current_page = 0;
        session.page_number_base = 0;
        session.page_offsets = vec![entry.text_offset];
        session.epub_page_numbers_exact = false;
        session.cache.clear();
        session.reconcile_epub_reading_page();
        match page {
            Ok((mut page, _source_size)) => {
                if session.page_offsets.first().copied() != Some(page.byte_offset) {
                    session.page_offsets = vec![page.byte_offset];
                    session.current_page = 0;
                    session.page_number_base = 0;
                    session.epub_page_numbers_exact = false;
                    session.reconcile_epub_reading_page();
                }
                page.page_index = session.current_absolute_page();
                session.push_cached_page(page);
                self.last_message = Some(format!("TOC: {}", entry.label));
                self.persist_current_session_best_effort();
                true
            }
            Err(error) => {
                self.last_message = Some(error);
                false
            }
        }
    }

    #[must_use]
    pub fn current_page_is_bookmarked(&self) -> bool {
        let Some(location) = self.session.as_ref().map(ReaderSession::current_location) else {
            return false;
        };
        self.bookmarks
            .iter()
            .any(|bookmark| bookmark.same_position(&location))
    }

    pub fn toggle_current_bookmark(&mut self) {
        let Some(location) = self.session.as_ref().map(ReaderSession::current_location) else {
            self.last_message = Some("Open a Reader page before adding a bookmark".into());
            return;
        };
        if let Some(index) = self
            .bookmarks
            .iter()
            .position(|bookmark| bookmark.same_position(&location))
        {
            self.bookmarks.remove(index);
            self.bookmarks_selected = self
                .bookmarks_selected
                .min(self.bookmarks.len().saturating_sub(1));
            self.last_message = Some("Bookmark removed".into());
        } else {
            self.bookmarks.insert(0, location);
            self.bookmarks.truncate(READER_BOOKMARK_LIMIT);
            self.bookmarks_selected = 0;
            self.last_message = Some("Bookmark saved".into());
        }
        self.persist_bookmarks_best_effort();
    }

    pub fn begin_preferences_edit(&mut self) {
        self.preferences_selected = 0;
        self.preferences_layout_dirty = false;
    }

    pub fn cycle_preference_previous(&mut self) {
        self.preferences_selected = self
            .preferences_selected
            .checked_sub(1)
            .unwrap_or(ReadingPreference::ALL.len() - 1);
    }

    pub fn cycle_preference_next(&mut self) {
        self.preferences_selected = (self.preferences_selected + 1) % ReadingPreference::ALL.len();
    }

    #[must_use]
    pub fn selected_preference(&self) -> ReadingPreference {
        ReadingPreference::ALL[self.preferences_selected]
    }

    /// Apply one Settings-style SELECT action to the highlighted preference.
    /// Redraw-only settings persist immediately in place. Layout-sensitive
    /// settings persist immediately and request a staged current-page rebuild.
    #[must_use]
    pub fn activate_selected_preference(&mut self) -> bool {
        let layout_sensitive = match self.selected_preference() {
            ReadingPreference::ReadingTheme => {
                self.preferences.theme = self.preferences.theme.next();
                self.last_message =
                    Some(format!("Reading theme: {}", self.preferences.theme.label()));
                self.persist_preferences_best_effort();
                self.request_clear_ghosting();
                false
            }
            ReadingPreference::Orientation => {
                self.preferences.orientation = self.preferences.orientation.next();
                self.last_message = Some(format!(
                    "Orientation: {}",
                    self.preferences.orientation.label()
                ));
                true
            }
            ReadingPreference::BookFontSize => {
                self.preferences.font_size = self.preferences.font_size.next();
                self.last_message = Some(format!(
                    "Book font size: {}",
                    self.preferences.font_size.label()
                ));
                true
            }
            ReadingPreference::BookFont => {
                self.cycle_book_font_choice();
                self.last_message = Some(format!("Book font: {}", self.book_font_display_label()));
                true
            }
            ReadingPreference::ParagraphAlignment => {
                self.preferences.paragraph_alignment = self.preferences.paragraph_alignment.next();
                self.last_message = Some(format!(
                    "Paragraph alignment: {}",
                    self.preferences.paragraph_alignment.label()
                ));
                true
            }
            ReadingPreference::ShowProgress => {
                self.preferences.show_progress = !self.preferences.show_progress;
                self.last_message = Some(format!(
                    "Show progress: {}",
                    if self.preferences.show_progress {
                        "On"
                    } else {
                        "Off"
                    }
                ));
                self.persist_preferences_best_effort();
                false
            }
        };
        if layout_sensitive {
            self.request_layout_rebuild()
        } else {
            false
        }
    }

    /// Finish the Settings-style editor. SELECT already persists changes and
    /// launches any required staged rebuild, so BOOT simply returns to options.
    pub fn finish_preferences_edit(&mut self) -> bool {
        self.preferences_layout_dirty = false;
        false
    }

    pub fn cycle_reading_theme(&mut self) {
        self.preferences.theme = self.preferences.theme.next();
        self.last_message = Some(format!("Reading theme: {}", self.preferences.theme.label()));
        self.persist_preferences_best_effort();
        self.request_clear_ghosting();
    }

    pub fn cycle_orientation(&mut self) -> bool {
        self.preferences.orientation = self.preferences.orientation.next();
        self.last_message = Some(format!(
            "Orientation: {}",
            self.preferences.orientation.label()
        ));
        self.request_layout_rebuild()
    }

    pub fn cycle_book_font_size(&mut self) -> bool {
        self.preferences.font_size = self.preferences.font_size.next();
        self.last_message = Some(format!(
            "Book font size: {}",
            self.preferences.font_size.label()
        ));
        self.request_layout_rebuild()
    }

    pub fn cycle_book_font(&mut self) -> bool {
        self.cycle_book_font_choice();
        self.last_message = Some(format!("Book font: {}", self.book_font_display_label()));
        self.request_layout_rebuild()
    }

    /// Cycle the shared book font size and persist it without reopening a TXT/EPUB.
    ///
    /// WeRead reads the same preferences. An open local session keeps its page
    /// cache and adopts the new layout metrics for later rebuilds.
    pub fn cycle_shared_book_font_size(&mut self) {
        self.preferences.font_size = self.preferences.font_size.next();
        self.last_message = Some(format!(
            "Book font size: {}",
            self.preferences.font_size.label()
        ));
        self.persist_shared_typography();
    }

    /// Cycle the shared book face, including SD faces under `/RUSTMIX/FONTS`.
    pub fn cycle_shared_book_font(&mut self) {
        self.refresh_font_catalog();
        self.cycle_book_font_choice();
        self.last_message = Some(format!("Book font: {}", self.book_font_display_label()));
        self.persist_shared_typography();
    }

    fn persist_shared_typography(&mut self) {
        self.persist_preferences_best_effort();
        if let Some(session) = self.session.as_mut() {
            session.layout = self.preferences.layout();
        }
    }

    fn cycle_book_font_choice(&mut self) {
        let next = match self.preferences.book_font {
            BookFont::Inter => BookFont::AtkinsonHyperlegible,
            BookFont::AtkinsonHyperlegible => BookFont::Serif,
            BookFont::Serif => BookFont::Literata,
            BookFont::Literata => BookFont::CjkUnifont,
            BookFont::CjkUnifont if self.sd_cjk_faces.is_empty() => BookFont::Inter,
            BookFont::CjkUnifont => BookFont::SdCjk,
            BookFont::SdCjk => {
                let current = self.preferences.sd_cjk_file_name();
                let index = current
                    .and_then(|name| {
                        self.sd_cjk_faces
                            .iter()
                            .position(|face| face.file_name.eq_ignore_ascii_case(name))
                    })
                    .unwrap_or(0);
                if index + 1 < self.sd_cjk_faces.len() {
                    self.preferences
                        .set_sd_cjk_file_name(Some(&self.sd_cjk_faces[index + 1].file_name));
                    crate::fonts::set_preferred_sd_file(self.preferences.sd_cjk_file_name());
                    self.preferences.book_font = BookFont::SdCjk;
                    return;
                }
                BookFont::Inter
            }
        };
        if next == BookFont::SdCjk {
            if let Some(face) = self.sd_cjk_faces.first() {
                self.preferences.set_sd_cjk_file_name(Some(&face.file_name));
            }
        } else if next != BookFont::SdCjk {
            self.preferences.set_sd_cjk_file_name(None);
        }
        self.preferences.book_font = next;
        crate::fonts::set_preferred_sd_file(self.preferences.sd_cjk_file_name());
    }

    pub fn toggle_show_progress(&mut self) {
        self.preferences.show_progress = !self.preferences.show_progress;
        self.last_message = Some(format!(
            "Show progress: {}",
            if self.preferences.show_progress {
                "On"
            } else {
                "Off"
            }
        ));
        self.persist_preferences_best_effort();
    }

    pub fn request_clear_ghosting(&mut self) {
        self.clear_ghost_requested = true;
        self.last_message = Some("Global ghost-clearing refresh requested".into());
    }

    #[must_use]
    pub fn take_clear_ghost_request(&mut self) -> bool {
        core::mem::take(&mut self.clear_ghost_requested)
    }

    #[must_use]
    pub fn take_persistence_event(&mut self) -> Option<String> {
        self.persistence_event.take()
    }

    #[must_use]
    fn state_path(&self) -> PathBuf {
        Path::new(&self.state_root).join(READER_STATE_FILE)
    }

    #[must_use]
    fn positions_path(&self) -> PathBuf {
        Path::new(&self.state_root).join(READER_POSITIONS_FILE)
    }

    #[must_use]
    fn legacy_positions_path(&self) -> PathBuf {
        Path::new(&self.state_root).join(LEGACY_READER_POSITIONS_FILE)
    }

    fn load_positions_with_legacy_migration(&mut self) -> Result<Vec<ReaderLocation>, String> {
        let positions = self.positions_path();
        let positions_backup = with_extension(&positions, "BAK");
        if positions.exists() || positions_backup.exists() {
            return load_location_list(&positions, READER_POSITION_LIMIT);
        }

        let legacy = self.legacy_positions_path();
        let legacy_backup = with_extension(&legacy, "BAK");
        if !legacy.exists() && !legacy_backup.exists() {
            return Ok(Vec::new());
        }

        let migrated = load_location_list(&legacy, READER_POSITION_LIMIT)?;
        if !migrated.is_empty() {
            if let Err(error) = atomic_replace_text(&positions, &serialize_location_list(&migrated))
            {
                self.persistence_warning = Some(format!(
                    "legacy POSITIONS.TXT loaded; POSITS.TXT migration deferred: {error}"
                ));
            }
        }
        Ok(migrated)
    }

    #[must_use]
    fn recent_path(&self) -> PathBuf {
        Path::new(&self.state_root).join(READER_RECENT_FILE)
    }

    #[must_use]
    fn bookmarks_path(&self) -> PathBuf {
        Path::new(&self.state_root).join(READER_BOOKMARKS_FILE)
    }

    #[must_use]
    fn preferences_path(&self) -> PathBuf {
        Path::new(&self.state_root).join(READER_PREFS_FILE)
    }

    #[must_use]
    fn cache_directory(&self) -> PathBuf {
        Path::new(&self.state_root).join(READER_CACHE_DIRECTORY)
    }

    #[must_use]
    fn cache_file_name_for(book: &ReaderBook, layout: ReaderLayout) -> String {
        format!("{:08X}.CCH", book_fingerprint(book, layout) as u32)
    }

    #[must_use]
    fn cache_path_for(&self, book: &ReaderBook, layout: ReaderLayout) -> PathBuf {
        self.cache_directory()
            .join(Self::cache_file_name_for(book, layout))
    }

    fn open_txt_session(
        &mut self,
        book: &ReaderBook,
        encoding: TextEncoding,
        requested: Option<&ReaderLocation>,
    ) -> Result<ReaderSession, String> {
        let cached = match load_anchor_cache(
            &self.cache_path_for(book, self.preferences.layout()),
            book,
            self.preferences.layout(),
        ) {
            Ok(value) => value,
            Err(error) => {
                self.persistence_warning = Some(format!("TXT cache ignored: {error}"));
                None
            }
        };
        let (page_number_base, page_offsets, current_page, indexed_through, index_complete) =
            if let Some(cache) = cached {
                let saved = requested.filter(|location| location.matches_book(book));
                if let Some(location) = saved {
                    if let Some(index) = location
                        .page_index
                        .checked_sub(cache.base_page)
                        .filter(|index| *index < cache.offsets.len())
                    {
                        (
                            cache.base_page,
                            cache.offsets,
                            index,
                            cache.indexed_through,
                            cache.complete,
                        )
                    } else {
                        let offset = location.byte_offset.min(book.size_bytes);
                        (location.page_index, vec![offset], 0, offset, false)
                    }
                } else {
                    (
                        cache.base_page,
                        cache.offsets,
                        0,
                        cache.indexed_through,
                        cache.complete,
                    )
                }
            } else if let Some(location) = requested.filter(|location| location.matches_book(book))
            {
                (
                    location.page_index,
                    vec![location.byte_offset.min(book.size_bytes)],
                    0,
                    location.byte_offset.min(book.size_bytes),
                    false,
                )
            } else {
                (0, vec![0], 0, 0, false)
            };
        let offset = page_offsets.get(current_page).copied().unwrap_or(0);
        let absolute_page = page_number_base.saturating_add(current_page);
        let layout = self.preferences.layout();
        let page = read_txt_page(book, encoding, layout, offset, absolute_page)?;
        let indexed_through = indexed_through.max(page.next_byte_offset);
        let index_complete = index_complete || indexed_through >= book.size_bytes;
        Ok(ReaderSession {
            book: book.clone(),
            encoding,
            epub_document: None,
            layout,
            current_page,
            page_number_base,
            page_offsets,
            indexed_through,
            index_complete,
            index_truncated: false,
            epub_page_numbers_exact: false,
            cache: vec![page],
            epub_chapter_pages: Vec::new(),
            epub_anchor_index: None,
        })
    }

    fn open_epub_session(
        &mut self,
        book: &ReaderBook,
        document: EpubDocument,
        requested: Option<&ReaderLocation>,
    ) -> Result<ReaderSession, String> {
        let source_size = document.text_size_bytes();
        if source_size == 0 || document.text.trim().is_empty() {
            return Err("EPUB chapter pagination produced no readable pages".into());
        }
        let layout = self.preferences.layout();
        let requested = requested.filter(|location| location.matches_book(book));
        let saved_page = requested.map_or(0, |location| location.page_index);
        let requested_offset = requested
            .map(|location| location.byte_offset.min(source_size))
            .unwrap_or(0);
        let shells: Vec<crate::epub_page_index::IndexedChapter> = document
            .chapters
            .iter()
            .map(|chapter| crate::epub_page_index::IndexedChapter {
                chapter_number: chapter.number,
                text_offset: chapter.text_offset,
                text_end_offset: chapter.text_end_offset,
                first_page: None,
                page_count: 0,
                complete: false,
            })
            .collect();
        let mut index = crate::epub_page_index::EpubAnchorIndex::open_or_create(
            &self.epub_index_path_for(book, layout),
            book_fingerprint(book, layout),
            source_size,
            &shells,
            crate::epub_page_index::IndexLimits {
                page_limit: self.epub_page_anchor_limit,
                bytes_limit: self.epub_index_bytes_limit,
                ram_fallback_limit: READER_EPUB_RAM_FALLBACK_ANCHORS,
            },
        );
        if let Some(warning) = index.warning.clone() {
            self.persistence_warning = Some(format!("EPUB page index: {warning}"));
        }
        let located = index.page_containing(requested_offset);
        let (page_number_base, offset, exact) = if let Some(page_index) = located {
            let anchor = index.anchor(page_index).unwrap_or(requested_offset);
            (page_index, anchor, index.anchor(page_index) == Some(anchor))
        } else {
            (saved_page, requested_offset, false)
        };
        let page = read_epub_page(&document, layout, offset, page_number_base)?;
        let mut session_book = book.clone();
        if !document.title.trim().is_empty() {
            session_book.title = document.title.clone();
        }
        let _ = index.flush(false);
        log::info!(
            "rustmix-wave=epub-open status=first-page-ready indexed-pages={} complete={} truncated={} offset={}",
            index.page_count(),
            index.is_complete(),
            index.is_truncated(),
            page.byte_offset
        );
        let mut session = ReaderSession {
            book: session_book,
            encoding: TextEncoding::Utf8,
            epub_document: Some(document),
            layout,
            current_page: 0,
            page_number_base,
            page_offsets: vec![page.byte_offset],
            indexed_through: index.next_anchor(),
            index_complete: index.is_complete(),
            index_truncated: index.is_truncated(),
            epub_page_numbers_exact: exact,
            cache: vec![page],
            epub_chapter_pages: Vec::new(),
            epub_anchor_index: Some(index),
        };
        session.sync_epub_index_flags();
        Ok(session)
    }

    #[must_use]
    fn epub_index_path_for(&self, book: &ReaderBook, layout: ReaderLayout) -> PathBuf {
        self.cache_directory()
            .join(format!("{:08X}.EPI", book_fingerprint(book, layout) as u32))
    }

    fn persist_current_session_best_effort(&mut self) {
        let Some(location) = self.session.as_ref().map(ReaderSession::current_location) else {
            return;
        };
        self.resume = Some(location.clone());
        self.positions.retain(|entry| entry.path != location.path);
        self.positions.insert(0, location.clone());
        self.positions.truncate(READER_POSITION_LIMIT);
        self.recent.retain(|entry| entry.path != location.path);
        self.recent.insert(0, location);
        self.recent.truncate(READER_RECENT_LIMIT);
        let mut errors = Vec::new();
        if let Some(location) = self.resume.as_ref() {
            if let Err(error) =
                atomic_replace_text(&self.state_path(), &serialize_location(location))
            {
                errors.push(format!("STATE.TXT: {error}"));
            }
        }
        if let Err(error) = atomic_replace_text(
            &self.positions_path(),
            &serialize_location_list(&self.positions),
        ) {
            errors.push(format!("POSITS.TXT: {error}"));
        }
        if let Err(error) =
            atomic_replace_text(&self.recent_path(), &serialize_location_list(&self.recent))
        {
            errors.push(format!("RECENT.TXT: {error}"));
        }
        if let Err(error) = self.persist_anchor_cache() {
            errors.push(format!("CACHE: {error}"));
        }
        self.finish_persistence("state-positions-recent-cache", errors);
    }

    fn persist_bookmarks_best_effort(&mut self) {
        let mut errors = Vec::new();
        if let Err(error) = atomic_replace_text(
            &self.bookmarks_path(),
            &serialize_location_list(&self.bookmarks),
        ) {
            errors.push(format!("MARKS.TXT: {error}"));
        }
        self.finish_persistence("bookmarks", errors);
    }

    fn persist_anchor_cache_best_effort(&mut self) {
        let mut errors = Vec::new();
        if let Err(error) = self.persist_anchor_cache() {
            errors.push(format!("CACHE: {error}"));
        }
        self.finish_persistence("anchor-cache", errors);
    }

    fn persist_anchor_cache(&mut self) -> Result<(), String> {
        if let Some(session) = self.session.as_mut() {
            if let Some(index) = session.epub_anchor_index.as_mut() {
                index.flush(true)?;
            }
        }
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        let Some(cache) = session.anchor_cache() else {
            return Ok(());
        };
        atomic_replace_text(
            &self.cache_path_for(&session.book, session.layout),
            &serialize_anchor_cache(&cache),
        )
    }

    fn persist_preferences_best_effort(&mut self) {
        let mut errors = Vec::new();
        if let Err(error) =
            atomic_replace_text(&self.preferences_path(), &self.preferences.serialized())
        {
            errors.push(format!("PREFS.TXT: {error}"));
        }
        crate::reader_nvs::save_reader_preferences(&self.preferences);
        self.finish_persistence("preferences", errors);
    }

    fn finish_persistence(&mut self, scope: &str, errors: Vec<String>) {
        let event = if errors.is_empty() {
            format!("status=saved scope={scope}")
        } else {
            let warning = errors.join("; ");
            self.persistence_warning = Some(warning.clone());
            format!("status=degraded scope={scope} error={warning}")
        };
        if self.last_persistence_event.as_deref() != Some(event.as_str()) {
            self.last_persistence_event = Some(event.clone());
            self.persistence_event = Some(event);
        }
    }
}

/// Scan one bounded Reader library. TXT and EPUB/EPU rows open through the
/// shared staged Reader architecture.
pub fn scan_txt_library(root: impl AsRef<Path>) -> Result<Vec<ReaderBook>, String> {
    let root = root.as_ref();
    let mut books = Vec::new();
    let entries =
        fs::read_dir(root).map_err(|error| format!("Books folder unavailable: {error}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(format) = book_format_from_path(&path) else {
            continue;
        };
        let metadata = entry.metadata().ok();
        let size_bytes = metadata.as_ref().map_or(0, |meta| meta.len());
        let modified_seconds = metadata
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs());
        let fallback_title = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("Untitled book")
            .to_string();
        let title = if format == BookFormat::Epub {
            read_epub_title_on_worker(&path)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(fallback_title)
        } else {
            fallback_title
        };
        books.push(ReaderBook {
            path: path.to_string_lossy().into_owned(),
            title,
            format,
            size_bytes,
            modified_seconds,
        });
        if books.len() >= READER_LIBRARY_LIMIT {
            break;
        }
    }
    books.sort_by(|left, right| left.title.to_lowercase().cmp(&right.title.to_lowercase()));
    Ok(books)
}

#[must_use]
pub fn book_format_from_path(path: &Path) -> Option<BookFormat> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "txt" => Some(BookFormat::Text),
        "epub" | "epu" => Some(BookFormat::Epub),
        _ => None,
    }
}

pub fn detect_txt_encoding(path: impl AsRef<Path>) -> Result<TextEncoding, String> {
    let mut file = File::open(path.as_ref()).map_err(|error| format!("Open failed: {error}"))?;
    let mut sample = vec![0_u8; 4096];
    let read = file
        .read(&mut sample)
        .map_err(|error| format!("Read failed: {error}"))?;
    sample.truncate(read);
    if sample.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok(TextEncoding::Utf8Bom);
    }
    match std::str::from_utf8(&sample) {
        Ok(_) => Ok(TextEncoding::Utf8),
        Err(error) if error.error_len().is_none() => Ok(TextEncoding::Utf8),
        Err(_) => Ok(TextEncoding::Windows1252),
    }
}

fn read_reader_page(
    book: &ReaderBook,
    encoding: TextEncoding,
    layout: ReaderLayout,
    epub_document: Option<&EpubDocument>,
    byte_offset: u64,
    page_index: usize,
) -> Result<ReaderCachedPage, String> {
    match book.format {
        BookFormat::Text => read_txt_page(book, encoding, layout, byte_offset, page_index),
        BookFormat::Epub => read_epub_page(
            epub_document.ok_or_else(|| "EPUB document is unavailable".to_string())?,
            layout,
            byte_offset,
            page_index,
        ),
    }
}

fn estimate_chapter_pages(
    text_offset: u64,
    text_end_offset: u64,
    offset: u64,
    indexed_pages: usize,
    indexed_through: u64,
) -> (usize, usize) {
    let chapter_len = text_end_offset.saturating_sub(text_offset).max(1);
    let into_chapter = offset.saturating_sub(text_offset).min(chapter_len);
    if indexed_pages == 0 || indexed_through == 0 {
        return (1, 0);
    }
    let pages = (chapter_len.saturating_mul(indexed_pages as u64) / indexed_through).max(1);
    let number = (into_chapter.saturating_mul(indexed_pages as u64) / indexed_through)
        .saturating_add(1)
        .min(pages);
    (
        usize::try_from(number).unwrap_or(usize::MAX),
        usize::try_from(pages).unwrap_or(usize::MAX),
    )
}

fn read_epub_page(
    document: &EpubDocument,
    layout: ReaderLayout,
    byte_offset: u64,
    page_index: usize,
) -> Result<ReaderCachedPage, String> {
    let chapter_end = document
        .chapter_for_offset(byte_offset)
        .map_or(document.text_size_bytes(), |chapter| {
            chapter.text_end_offset
        });
    read_epub_page_until(document, layout, byte_offset, page_index, chapter_end)
}

fn read_epub_page_until(
    document: &EpubDocument,
    layout: ReaderLayout,
    byte_offset: u64,
    page_index: usize,
    text_end_offset: u64,
) -> Result<ReaderCachedPage, String> {
    let start = usize::try_from(byte_offset)
        .map_err(|_| "EPUB byte offset exceeds platform range".to_string())?
        .min(document.text.len());
    let bounded_end = usize::try_from(text_end_offset)
        .map_err(|_| "EPUB chapter end exceeds platform range".to_string())?
        .min(document.text.len());
    let end = start
        .saturating_add(READER_PAGE_READ_BYTES)
        .min(bounded_end);
    let bytes = document.text.as_bytes();
    let start = next_utf8_boundary(bytes, start);
    let end = previous_utf8_boundary(bytes, end).max(start);
    let decoded = decode_with_offsets(&bytes[start..end], TextEncoding::Utf8, start as u64);
    let normalized = normalize_decoded(&decoded);
    let (lines, consumed) = paginate_decoded(&normalized, layout);
    let next_byte_offset = consumed.max(start as u64).min(text_end_offset);
    Ok(ReaderCachedPage {
        page_index,
        byte_offset: start as u64,
        next_byte_offset,
        lines,
    })
}

fn next_utf8_boundary(bytes: &[u8], mut offset: usize) -> usize {
    while offset < bytes.len() && offset > 0 && bytes[offset] & 0xC0 == 0x80 {
        offset += 1;
    }
    offset.min(bytes.len())
}

fn previous_utf8_boundary(bytes: &[u8], mut offset: usize) -> usize {
    offset = offset.min(bytes.len());
    while offset > 0 && offset < bytes.len() && bytes[offset] & 0xC0 == 0x80 {
        offset -= 1;
    }
    offset
}

fn read_txt_page(
    book: &ReaderBook,
    encoding: TextEncoding,
    layout: ReaderLayout,
    byte_offset: u64,
    page_index: usize,
) -> Result<ReaderCachedPage, String> {
    let mut file = File::open(&book.path).map_err(|error| format!("Open failed: {error}"))?;
    file.seek(SeekFrom::Start(byte_offset))
        .map_err(|error| format!("Seek failed: {error}"))?;
    let mut bytes = vec![0_u8; READER_PAGE_READ_BYTES];
    let read = file
        .read(&mut bytes)
        .map_err(|error| format!("Read failed: {error}"))?;
    bytes.truncate(read);
    let skip_bom = byte_offset == 0 && bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
    let base = byte_offset + if skip_bom { 3 } else { 0 };
    let decoded = decode_with_offsets(&bytes[if skip_bom { 3 } else { 0 }..], encoding, base);
    let normalized = normalize_decoded(&decoded);
    let (lines, consumed) = paginate_decoded(&normalized, layout);
    let next_byte_offset = consumed.max(base).min(book.size_bytes);
    Ok(ReaderCachedPage {
        page_index,
        byte_offset,
        next_byte_offset,
        lines,
    })
}

fn decode_with_offsets(bytes: &[u8], encoding: TextEncoding, base: u64) -> Vec<(char, u64)> {
    match encoding {
        TextEncoding::Windows1252 => bytes
            .iter()
            .enumerate()
            .map(|(index, byte)| (decode_windows_1252(*byte), base + index as u64 + 1))
            .collect(),
        TextEncoding::Utf8 | TextEncoding::Utf8Bom => {
            let valid = match std::str::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) => std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or(""),
            };
            valid
                .char_indices()
                .map(|(index, character)| {
                    (character, base + index as u64 + character.len_utf8() as u64)
                })
                .collect()
        }
    }
}

fn normalize_decoded(decoded: &[(char, u64)]) -> Vec<(char, u64)> {
    let mut normalized = Vec::new();
    for (index, (character, next_offset)) in decoded.iter().copied().enumerate() {
        if character == '_' {
            let previous = index
                .checked_sub(1)
                .and_then(|value| decoded.get(value))
                .map(|value| value.0);
            let next = decoded.get(index + 1).map(|value| value.0);
            let word_internal =
                previous.is_some_and(is_word_character) && next.is_some_and(is_word_character);
            let repeated_separator = previous == Some('_') || next == Some('_');

            // Project Gutenberg TXT files often wrap emphasis across multiple
            // source lines: `_first line ... last line_`. Remove each bounded
            // delimiter independently so closing markers after punctuation do
            // not leak into rendered pages. Keep filename-style word_internal
            // underscores and repeated separator rows intact.
            if !word_internal && !repeated_separator {
                continue;
            }
        }
        push_normalized_character(&mut normalized, character, next_offset);
    }
    normalized
}

fn push_normalized_character(output: &mut Vec<(char, u64)>, character: char, next_offset: u64) {
    if crate::fonts::is_cjk_codepoint(character) {
        output.push((character, next_offset));
        return;
    }
    let replacement: &str = match character {
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{00AB}' | '\u{00BB}' => "\"",
        '\u{2018}' | '\u{2019}' | '\u{201A}' => "'",
        '\u{2014}' => "--",
        '\u{2013}' => "-",
        '\u{2026}' => "...",
        '\u{00A0}' => " ",
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => "e",
        'à' | 'á' | 'â' | 'ä' | 'À' | 'Á' | 'Â' | 'Ä' => "a",
        'ç' | 'Ç' => "c",
        'ï' | 'î' | 'í' | 'ì' | 'Ï' | 'Î' | 'Í' | 'Ì' => "i",
        'ô' | 'ö' | 'ó' | 'ò' | 'Ô' | 'Ö' | 'Ó' | 'Ò' => "o",
        'ù' | 'û' | 'ü' | 'ú' | 'Ù' | 'Û' | 'Ü' | 'Ú' => "u",
        'ñ' | 'Ñ' => "n",
        value
            if value == '\n'
                || value == '\r'
                || value == '\t'
                || value.is_ascii_graphic()
                || value == ' ' =>
        {
            output.push((value, next_offset));
            return;
        }
        _ => "?",
    };
    for value in replacement.chars() {
        output.push((value, next_offset));
    }
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric()
}

fn paginate_decoded(decoded: &[(char, u64)], layout: ReaderLayout) -> (Vec<ReaderPageLine>, u64) {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0i32;
    let mut consumed = decoded
        .first()
        .map_or(0, |(_, offset)| offset.saturating_sub(1));
    for (character, next_offset) in decoded.iter().copied() {
        let character = match character {
            '\r' => continue,
            '\n' => {
                lines.push(ReaderPageLine {
                    text: core::mem::take(&mut line),
                    paragraph_end: true,
                });
                line_width = 0;
                consumed = next_offset;
                if lines.len() >= layout.lines_per_page {
                    break;
                }
                continue;
            }
            value if value.is_control() => ' ',
            value => value,
        };
        let advance = if character.is_ascii() {
            if character == ' ' {
                layout.ascii_advance_px
            } else {
                layout.ascii_advance_px
            }
        } else {
            crate::fonts::unicode_advance(
                character,
                layout.font_size_px,
                layout.ascii_advance_px as u8,
            )
        };
        if !line.is_empty() && line_width + advance > layout.max_line_width_px {
            lines.push(ReaderPageLine {
                text: core::mem::take(&mut line),
                paragraph_end: false,
            });
            line_width = 0;
            if lines.len() >= layout.lines_per_page {
                break;
            }
        }
        if character.is_whitespace() {
            if !line.is_empty() && !line.ends_with(' ') {
                line.push(' ');
                line_width += advance;
            }
        } else {
            line.push(character);
            line_width += advance;
        }
        consumed = next_offset;
    }
    if lines.len() < layout.lines_per_page && (!line.is_empty() || lines.is_empty()) {
        lines.push(ReaderPageLine {
            text: line,
            paragraph_end: true,
        });
    }
    (lines, consumed)
}

/// Paginate plain text with the Reader line breaker used by TXT and EPUB pages.
///
/// `max_pages` is a hard cap so a hostile or huge chapter cannot grow without
/// bound. The function stops early if a page does not advance.
#[must_use]
pub fn paginate_plain_text(
    text: &str,
    layout: ReaderLayout,
    max_pages: usize,
) -> Vec<Vec<ReaderPageLine>> {
    let bytes = text.as_bytes();
    let mut offset = 0u64;
    let mut pages = Vec::new();
    let limit = max_pages.min(4_096);
    while (offset as usize) < bytes.len() && pages.len() < limit {
        let start = offset as usize;
        let end = previous_utf8_boundary(
            bytes,
            start
                .saturating_add(READER_PAGE_READ_BYTES)
                .min(bytes.len()),
        )
        .max(start);
        if end == start {
            break;
        }
        let decoded = decode_with_offsets(&bytes[start..end], TextEncoding::Utf8, start as u64);
        let normalized = normalize_decoded(&decoded);
        let (lines, consumed) = paginate_decoded(&normalized, layout);
        if consumed <= offset {
            break;
        }
        offset = consumed.min(bytes.len() as u64);
        pages.push(lines);
    }
    if pages.is_empty() {
        pages.push(vec![ReaderPageLine {
            text: String::new(),
            paragraph_end: true,
        }]);
    }
    pages
}

fn decode_windows_1252(byte: u8) -> char {
    match byte {
        0x80 => '€',
        0x82 => '‚',
        0x83 => 'ƒ',
        0x84 => '„',
        0x85 => '…',
        0x86 => '†',
        0x87 => '‡',
        0x88 => 'ˆ',
        0x89 => '‰',
        0x8A => 'Š',
        0x8B => '‹',
        0x8C => 'Œ',
        0x8E => 'Ž',
        0x91 => '‘',
        0x92 => '’',
        0x93 => '“',
        0x94 => '”',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        0x98 => '˜',
        0x99 => '™',
        0x9A => 'š',
        0x9B => '›',
        0x9C => 'œ',
        0x9E => 'ž',
        0x9F => 'Ÿ',
        value => char::from(value),
    }
}

fn book_fingerprint(book: &ReaderBook, layout: ReaderLayout) -> u64 {
    let mut hash = CACHE_FNV_OFFSET;
    fn feed(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(CACHE_FNV_PRIME);
        }
    }
    feed(&mut hash, book.path.as_bytes());
    feed(&mut hash, &book.size_bytes.to_le_bytes());
    feed(&mut hash, &book.modified_seconds.to_le_bytes());
    feed(&mut hash, book.format.marker().as_bytes());
    feed(&mut hash, &layout.lines_per_page.to_le_bytes());
    feed(&mut hash, &layout.chars_per_line.to_le_bytes());
    feed(&mut hash, &layout.max_line_width_px.to_le_bytes());
    feed(&mut hash, &layout.font_size_px.to_le_bytes());
    feed(&mut hash, layout.orientation.marker().as_bytes());
    feed(&mut hash, layout.font_size.marker().as_bytes());
    feed(&mut hash, layout.book_font.marker().as_bytes());
    feed(&mut hash, layout.paragraph_alignment.marker().as_bytes());
    feed(&mut hash, READER_CACHE_VERSION.as_bytes());
    hash
}

fn serialize_location(location: &ReaderLocation) -> String {
    format!(
        "version={}\npath={}\ntitle={}\nformat={}\nsize={}\nmodified={}\npage={}\noffset={}\nchapter={}\nchapter_page={}\nchapter_pages={}\n",
        READER_PERSISTENCE_VERSION,
        escape_field(&location.path),
        escape_field(&location.title),
        location.format.marker(),
        location.size_bytes,
        location.modified_seconds,
        location.page_index,
        location.byte_offset,
        optional_usize(location.epub_chapter.as_ref().map(|chapter| chapter.chapter_number)),
        optional_usize(location.epub_chapter.as_ref().map(|chapter| chapter.page_number)),
        optional_usize(location.epub_chapter.as_ref().map(|chapter| chapter.page_count))
    )
}

fn parse_location_record(text: &str) -> Result<ReaderLocation, String> {
    let mut version = None;
    let mut path = None;
    let mut title = None;
    let mut format = None;
    let mut size = None;
    let mut modified = None;
    let mut page = None;
    let mut offset = None;
    let mut chapter = None;
    let mut chapter_page = None;
    let mut chapter_pages = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "version" => version = Some(value),
            "path" => path = Some(unescape_field(value)?),
            "title" => title = Some(unescape_field(value)?),
            "format" => format = BookFormat::parse(value),
            "size" => size = value.parse().ok(),
            "modified" => modified = value.parse().ok(),
            "page" => page = value.parse().ok(),
            "offset" => offset = value.parse().ok(),
            "chapter" => chapter = parse_optional_usize(value),
            "chapter_page" => chapter_page = parse_optional_usize(value),
            "chapter_pages" => chapter_pages = parse_optional_usize(value),
            _ => {}
        }
    }
    if version != Some(READER_PERSISTENCE_VERSION) {
        return Err("unsupported persistence version".into());
    }
    Ok(ReaderLocation {
        path: path.ok_or_else(|| "missing path".to_string())?,
        title: title.ok_or_else(|| "missing title".to_string())?,
        format: format.ok_or_else(|| "missing format".to_string())?,
        size_bytes: size.ok_or_else(|| "missing size".to_string())?,
        modified_seconds: modified.unwrap_or(0),
        page_index: page.ok_or_else(|| "missing page".to_string())?,
        byte_offset: offset.ok_or_else(|| "missing offset".to_string())?,
        epub_chapter: chapter_page_label(chapter, chapter_page, chapter_pages),
    })
}

fn serialize_location_list(locations: &[ReaderLocation]) -> String {
    let mut output = format!("version={}\n", READER_PERSISTENCE_VERSION);
    for location in locations {
        output.push_str("entry=");
        output.push_str(&serialize_location_fields(location));
        output.push('\n');
    }
    output
}

fn serialize_location_fields(location: &ReaderLocation) -> String {
    [
        escape_field(&location.path),
        escape_field(&location.title),
        location.format.marker().into(),
        location.size_bytes.to_string(),
        location.modified_seconds.to_string(),
        location.page_index.to_string(),
        location.byte_offset.to_string(),
        optional_usize(
            location
                .epub_chapter
                .as_ref()
                .map(|chapter| chapter.chapter_number),
        ),
        optional_usize(
            location
                .epub_chapter
                .as_ref()
                .map(|chapter| chapter.page_number),
        ),
        optional_usize(
            location
                .epub_chapter
                .as_ref()
                .map(|chapter| chapter.page_count),
        ),
    ]
    .join("\t")
}

fn parse_location_fields(value: &str) -> Result<ReaderLocation, String> {
    let fields = split_escaped_tabs(value)?;
    if fields.len() != 7 && fields.len() != 10 {
        return Err("invalid location field count".into());
    }
    let epub_chapter = if fields.len() == 10 {
        chapter_page_label(
            parse_optional_usize(&fields[7]),
            parse_optional_usize(&fields[8]),
            parse_optional_usize(&fields[9]),
        )
    } else {
        None
    };
    Ok(ReaderLocation {
        path: fields[0].clone(),
        title: fields[1].clone(),
        format: BookFormat::parse(&fields[2]).ok_or_else(|| "invalid format".to_string())?,
        size_bytes: fields[3].parse().map_err(|_| "invalid size".to_string())?,
        modified_seconds: fields[4]
            .parse()
            .map_err(|_| "invalid modified time".to_string())?,
        page_index: fields[5].parse().map_err(|_| "invalid page".to_string())?,
        byte_offset: fields[6]
            .parse()
            .map_err(|_| "invalid offset".to_string())?,
        epub_chapter,
    })
}

fn optional_usize(value: Option<usize>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

fn parse_optional_usize(value: &str) -> Option<usize> {
    if value.is_empty() {
        None
    } else {
        value.parse().ok()
    }
}

fn chapter_page_label(
    chapter_number: Option<usize>,
    page_number: Option<usize>,
    page_count: Option<usize>,
) -> Option<ReaderChapterPageLabel> {
    Some(ReaderChapterPageLabel {
        chapter_number: chapter_number?,
        page_number: page_number?,
        page_count: page_count?,
        approximate: false,
    })
}

fn parse_location_list(text: &str, limit: usize) -> Result<Vec<ReaderLocation>, String> {
    let mut version = None;
    let mut output = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("version=") {
            version = Some(value);
        } else if let Some(value) = line.strip_prefix("entry=") {
            if output.len() < limit {
                output.push(parse_location_fields(value)?);
            }
        }
    }
    if version != Some(READER_PERSISTENCE_VERSION) {
        return Err("unsupported persistence version".into());
    }
    Ok(output)
}

fn serialize_anchor_cache(cache: &ReaderAnchorCache) -> String {
    let persisted_complete = cache.complete && cache.offsets.len() < READER_CACHE_OFFSET_LIMIT;
    let mut output = format!(
        "version={}\nfingerprint={:016X}\nbase_page={}\nindexed_through={}\ncomplete={}\n",
        READER_CACHE_VERSION,
        cache.fingerprint,
        cache.base_page,
        cache.indexed_through,
        persisted_complete
    );
    for offset in cache.offsets.iter().take(READER_CACHE_OFFSET_LIMIT) {
        output.push_str(&format!("offset={offset}\n"));
    }
    output
}

fn parse_anchor_cache(
    text: &str,
    book: &ReaderBook,
    layout: ReaderLayout,
) -> Result<ReaderAnchorCache, String> {
    let mut version = None;
    let mut fingerprint = None;
    let mut base_page = None;
    let mut indexed_through: Option<u64> = None;
    let mut complete = None;
    let mut offsets = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "version" => version = Some(value),
            "fingerprint" => fingerprint = u64::from_str_radix(value, 16).ok(),
            "base_page" => base_page = value.parse().ok(),
            "indexed_through" => indexed_through = value.parse().ok(),
            "complete" => complete = value.parse().ok(),
            "offset" if offsets.len() < READER_CACHE_OFFSET_LIMIT => {
                offsets.push(
                    value
                        .parse()
                        .map_err(|_| "invalid cache offset".to_string())?,
                );
            }
            _ => {}
        }
    }
    if version != Some(READER_CACHE_VERSION) {
        return Err("unsupported cache version".into());
    }
    let fingerprint = fingerprint.ok_or_else(|| "missing cache fingerprint".to_string())?;
    if fingerprint != book_fingerprint(book, layout) {
        return Err("cache fingerprint mismatch".into());
    }
    if offsets.is_empty() {
        return Err("cache contains no offsets".into());
    }
    if offsets.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("cache offsets are not strictly increasing".into());
    }
    if offsets.iter().any(|offset| *offset > book.size_bytes) {
        return Err("cache offset exceeds book size".into());
    }
    let mut complete = complete.ok_or_else(|| "missing complete flag".to_string())?;
    if offsets.len() >= READER_CACHE_OFFSET_LIMIT {
        complete = false;
    }
    Ok(ReaderAnchorCache {
        fingerprint,
        base_page: base_page.ok_or_else(|| "missing base page".to_string())?,
        offsets,
        indexed_through: indexed_through
            .ok_or_else(|| "missing indexed offset".to_string())?
            .min(book.size_bytes),
        complete,
    })
}

fn load_preferences(path: &Path) -> Result<Option<ReaderPreferences>, String> {
    load_with_backup(path, ReaderPreferences::parse)
}

fn load_location_record(path: &Path) -> Result<Option<ReaderLocation>, String> {
    load_with_backup(path, parse_location_record)
}

fn load_location_list(path: &Path, limit: usize) -> Result<Vec<ReaderLocation>, String> {
    load_with_backup(path, |text| parse_location_list(text, limit))
        .map(|value| value.unwrap_or_default())
}

fn load_anchor_cache(
    path: &Path,
    book: &ReaderBook,
    layout: ReaderLayout,
) -> Result<Option<ReaderAnchorCache>, String> {
    load_with_backup(path, |text| parse_anchor_cache(text, book, layout))
}

fn load_with_backup<T>(
    path: &Path,
    parser: impl Fn(&str) -> Result<T, String>,
) -> Result<Option<T>, String> {
    let backup = with_extension(path, "BAK");
    let mut errors = Vec::new();
    for candidate in [path.to_path_buf(), backup] {
        if !candidate.exists() {
            continue;
        }
        match fs::read_to_string(&candidate) {
            Ok(text) => match parser(&text) {
                Ok(value) => return Ok(Some(value)),
                Err(error) => errors.push(format!("{}: {error}", candidate.display())),
            },
            Err(error) => errors.push(format!("{}: {error}", candidate.display())),
        }
    }
    if errors.is_empty() {
        Ok(None)
    } else {
        Err(errors.join("; "))
    }
}

fn is_fat83_safe_file_name(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let Some((stem, extension)) = file_name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && stem.len() <= 8
        && !extension.is_empty()
        && extension.len() <= 3
        && stem
            .bytes()
            .chain(extension.bytes())
            .all(|value| value.is_ascii_alphanumeric() || value == b'_')
}

/// Power-safe bounded text replacement for Reader-owned state. The previous
/// primary is retained as .BAK until the new .TMP file has been renamed into
/// place. Readers accept the backup if startup observes an interrupted write.
fn atomic_replace_text(path: &Path, text: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "state path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temp = with_extension(path, "TMP");
    let backup = with_extension(path, "BAK");
    for candidate in [path, temp.as_path(), backup.as_path()] {
        if !is_fat83_safe_file_name(candidate) {
            return Err(format!(
                "Reader state filename is not FAT 8.3 safe: {}",
                candidate.display()
            ));
        }
    }
    let _ = fs::remove_file(&temp);
    let _ = fs::remove_file(&backup);
    {
        let mut file =
            File::create(&temp).map_err(|error| format!("create {}: {error}", temp.display()))?;
        file.write_all(text.as_bytes())
            .map_err(|error| format!("write {}: {error}", temp.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", temp.display()))?;
    }
    if path.exists() {
        fs::rename(path, &backup).map_err(|error| format!("backup {}: {error}", path.display()))?;
    }
    if let Err(error) = fs::rename(&temp, path) {
        if backup.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err(format!("replace {}: {error}", path.display()));
    }
    let _ = fs::remove_file(&backup);
    // ESP-IDF FAT cannot open a directory as a file (`EACCES`). The temp file
    // `sync_all` above is the durability step; the rename is the commit.
    Ok(())
}

fn with_extension(path: &Path, extension: &str) -> PathBuf {
    let mut output = path.to_path_buf();
    output.set_extension(extension);
    output
}

fn escape_field(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\t' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            value => output.push(value),
        }
    }
    output
}

fn unescape_field(value: &str) -> Result<String, String> {
    let mut output = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            match character {
                '\\' => output.push('\\'),
                't' => output.push('\t'),
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                _ => return Err("invalid escape sequence".into()),
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            output.push(character);
        }
    }
    if escaped {
        return Err("trailing escape sequence".into());
    }
    Ok(output)
}

fn split_escaped_tabs(value: &str) -> Result<Vec<String>, String> {
    let mut output = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            current.push('\\');
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '\t' {
            output.push(unescape_field(&current)?);
            current.clear();
        } else {
            current.push(character);
        }
    }
    if escaped {
        return Err("trailing escape sequence".into());
    }
    output.push(unescape_field(&current)?);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use super::{
        atomic_replace_text, book_format_from_path, detect_txt_encoding, is_fat83_safe_file_name,
        load_location_record, normalize_decoded, paginate_decoded, parse_location_fields,
        parse_location_record, scan_txt_library, serialize_location, serialize_location_fields,
        BookFont, BookFontSize, BookFormat, ParagraphAlignment, ReaderBook, ReaderChapterPageLabel,
        ReaderLoadingStage, ReaderLocation, ReaderOrientation, ReaderPreferences, ReaderSession,
        ReaderTickOutcome, ReaderUiState, ReadingPreference, ReadingTheme, TextEncoding,
        LEGACY_READER_POSITIONS_FILE, READER_BOOKMARKS_FILE, READER_CACHE_OFFSET_LIMIT,
        READER_EPUB_ANCHOR_INDEX_BYTES_LIMIT, READER_EPUB_INDEX_YIELD_EVERY_PAGES,
        READER_EPUB_INDEX_YIELD_MILLIS, READER_EPUB_PAGE_ANCHOR_LIMIT, READER_EPUB_READING_WINDOW,
        READER_POSITIONS_FILE, READER_PREFS_FILE, READER_RECENT_FILE, READER_STATE_FILE,
    };
    use crate::buttons::ButtonEvent;

    fn temp_dir(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("rustmix-reader-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn detects_txt_epub_and_short_epu_aliases() {
        assert_eq!(
            book_format_from_path(PathBuf::from("a.TXT").as_path()),
            Some(BookFormat::Text)
        );
        assert_eq!(
            book_format_from_path(PathBuf::from("a.epub").as_path()),
            Some(BookFormat::Epub)
        );
        assert_eq!(
            book_format_from_path(PathBuf::from("a.EPU").as_path()),
            Some(BookFormat::Epub)
        );
    }

    #[test]
    fn detects_utf8_bom_and_windows_1252() {
        let root = temp_dir("encoding");
        let bom = root.join("bom.txt");
        let cp = root.join("cp.txt");
        fs::write(&bom, [0xEF, 0xBB, 0xBF, b'H', b'i']).unwrap();
        fs::write(&cp, [b'H', 0x92, b'i']).unwrap();
        assert_eq!(detect_txt_encoding(&bom).unwrap(), TextEncoding::Utf8Bom);
        assert_eq!(detect_txt_encoding(&cp).unwrap(), TextEncoding::Windows1252);
    }

    #[test]
    fn scans_txt_and_epub_rows_but_ignores_other_files() {
        let root = temp_dir("scan");
        fs::write(root.join("Dracula.txt"), "hello").unwrap();
        fs::write(root.join("Later.epu"), "zip").unwrap();
        fs::write(root.join("ignore.bin"), "no").unwrap();
        let books = scan_txt_library(&root).unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].title, "Dracula");
        assert_eq!(books[1].format, BookFormat::Epub);
    }

    #[test]
    fn opening_txt_is_staged_first_page_first_and_lazy() {
        let root = temp_dir("open");
        let state = temp_dir("open-state");
        fs::write(root.join("Book.txt"), "hello world ".repeat(600)).unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.refresh_library();
        reader.library_selected = 1;
        assert!(reader.apply_library_button(ButtonEvent::Select));
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::FirstPageReady);
        let session = reader.session.as_ref().unwrap();
        assert_eq!(session.current_page, 0);
        assert!(!session.cache.is_empty());
        assert!(session.indexed_through > 0);
    }

    #[test]
    fn persists_continue_recent_bookmarks_and_anchor_cache() {
        let root = temp_dir("persist-books");
        let state = temp_dir("persist-state");
        fs::write(root.join("Dracula.txt"), "Dracula text ".repeat(1000)).unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.refresh_library();
        reader.library_selected = 1;
        assert!(reader.apply_library_button(ButtonEvent::Select));
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::FirstPageReady);
        reader.next_page();
        reader.toggle_current_bookmark();
        assert!(state.join(READER_STATE_FILE).exists());
        assert!(state.join(READER_POSITIONS_FILE).exists());
        assert!(state.join(READER_RECENT_FILE).exists());
        assert!(state.join(READER_BOOKMARKS_FILE).exists());
        assert!(state.join("CACHE").read_dir().unwrap().next().is_some());

        let mut restored = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        let report = restored.load_persistent_state();
        assert!(report.state_loaded);
        assert_eq!(report.recent_count, 1);
        assert_eq!(report.bookmark_count, 1);
        assert!(restored.request_continue());
        assert_eq!(restored.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(restored.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(restored.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(restored.tick(), ReaderTickOutcome::FirstPageReady);
        assert_eq!(
            restored.session.as_ref().unwrap().current_absolute_page(),
            1
        );
    }

    #[test]
    fn invalid_anchor_cache_fingerprint_falls_back_to_saved_offset() {
        let root = temp_dir("fingerprint-books");
        let state = temp_dir("fingerprint-state");
        fs::write(root.join("Book.txt"), "text body ".repeat(1000)).unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.refresh_library();
        reader.library_selected = 1;
        assert!(reader.apply_library_button(ButtonEvent::Select));
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::FirstPageReady);
        reader.next_page();
        let cache = state
            .join("CACHE")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = fs::read_to_string(&cache).unwrap();
        fs::write(
            &cache,
            text.replace("fingerprint=", "fingerprint=0000000000000000#"),
        )
        .unwrap();

        let mut restored = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        restored.load_persistent_state();
        assert!(restored.request_continue());
        assert_eq!(restored.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(restored.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(restored.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(restored.tick(), ReaderTickOutcome::FirstPageReady);
        assert!(restored
            .persistence_warning
            .as_deref()
            .unwrap_or("")
            .contains("TXT cache ignored"));
        assert_eq!(
            restored.session.as_ref().unwrap().current_absolute_page(),
            1
        );
    }

    #[test]
    fn bookmark_toggle_removes_existing_mark() {
        let root = temp_dir("toggle-books");
        let state = temp_dir("toggle-state");
        fs::write(root.join("Book.txt"), "text ".repeat(100)).unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.refresh_library();
        reader.library_selected = 1;
        assert!(reader.apply_library_button(ButtonEvent::Select));
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::FirstPageReady);
        reader.toggle_current_bookmark();
        assert_eq!(reader.bookmarks.len(), 1);
        reader.toggle_current_bookmark();
        assert!(reader.bookmarks.is_empty());
    }

    #[test]
    fn interrupted_atomic_replace_recovers_backup() {
        let root = temp_dir("backup");
        let state = root.join(READER_STATE_FILE);
        atomic_replace_text(
            &state,
            "version=1\npath=a.txt\ntitle=A\nformat=txt\nsize=1\nmodified=0\npage=0\noffset=0\n",
        )
        .unwrap();
        let backup = root.join("STATE.BAK");
        fs::rename(&state, &backup).unwrap();
        let restored = load_location_record(&state).unwrap().unwrap();
        assert_eq!(restored.title, "A");
    }

    #[test]
    fn corrupt_primary_falls_back_to_backup() {
        let root = temp_dir("corrupt");
        let state = root.join(READER_STATE_FILE);
        fs::write(&state, "not-valid").unwrap();
        fs::write(
            root.join("STATE.BAK"),
            "version=1\npath=b.txt\ntitle=B\nformat=txt\nsize=2\nmodified=0\npage=3\noffset=4\n",
        )
        .unwrap();
        let restored = load_location_record(&state).unwrap().unwrap();
        assert_eq!(restored.title, "B");
    }

    #[test]
    fn reader_options_request_manual_clear_ghosting() {
        let mut reader = ReaderUiState::default();
        reader.request_clear_ghosting();
        assert!(reader.take_clear_ghost_request());
        assert!(!reader.take_clear_ghost_request());
    }

    #[test]
    fn normalizes_utf8_punctuation_accents_and_simple_emphasis() {
        let decoded: Vec<(char, u64)> = "“En vérité!” _I_—once…"
            .chars()
            .enumerate()
            .map(|(index, value)| (value, index as u64 + 1))
            .collect();
        let normalized: String = normalize_decoded(&decoded)
            .into_iter()
            .map(|(value, _)| value)
            .collect();
        assert_eq!(normalized, "\"En verite!\" I--once...");
    }

    #[test]
    fn removes_multiline_gutenberg_emphasis_but_preserves_safe_underscores() {
        let decoded: Vec<(char, u64)> =
            "'_You have lost your\ngold pencil-case? Couragez!'_ file_name\n_____"
                .chars()
                .enumerate()
                .map(|(index, value)| (value, index as u64 + 1))
                .collect();
        let normalized: String = normalize_decoded(&decoded)
            .into_iter()
            .map(|(value, _)| value)
            .collect();
        assert_eq!(
            normalized,
            "'You have lost your\ngold pencil-case? Couragez!' file_name\n_____"
        );
    }

    #[test]
    fn cjk_text_is_preserved_and_wraps_on_pixel_width() {
        let decoded: Vec<(char, u64)> = "中文阅读器ABCDEF"
            .chars()
            .enumerate()
            .map(|(index, value)| (value, index as u64 + 1))
            .collect();
        let normalized: String = normalize_decoded(&decoded)
            .into_iter()
            .map(|(value, _)| value)
            .collect();
        assert!(normalized.contains('中'));
        assert!(normalized.contains('文'));
        assert!(!normalized.contains('?'));
        let layout = ReaderPreferences {
            font_size: BookFontSize::Px16,
            book_font: BookFont::CjkUnifont,
            ..ReaderPreferences::default()
        }
        .layout();
        let (lines, _) = paginate_decoded(&normalize_decoded(&decoded), layout);
        assert!(!lines.is_empty());
        assert!(lines.iter().any(|line| line.text.contains('中')));
    }

    #[test]
    fn theme_switch_keeps_layout_geometry_and_cache_fingerprint_inputs_stable() {
        let classic = ReaderPreferences::default();
        let mut contrast = classic;
        contrast.theme = ReadingTheme::HighContrast;
        assert_eq!(classic.layout(), contrast.layout());
    }

    #[test]
    fn reader_font_cycle_preserves_legacy_keys_and_adds_cjk_faces() {
        assert_eq!(
            BookFont::AtkinsonHyperlegible.marker(),
            "atkinson-hyperlegible"
        );
        assert_eq!(BookFont::Serif.marker(), "serif");
        assert_eq!(BookFont::Literata.marker(), "literata");
        assert_eq!(BookFont::CjkUnifont.marker(), "cjk-unifont");
        assert_eq!(BookFont::parse("literata").unwrap(), BookFont::Literata);
        assert_eq!(
            BookFont::parse("cjk-unifont").unwrap(),
            BookFont::CjkUnifont
        );
        assert_eq!(BookFont::parse("sd:NOTOSC.TTF").unwrap(), BookFont::SdCjk);
        assert_eq!(BookFontSize::parse("24").unwrap(), BookFontSize::Px24);
        assert_eq!(BookFontSize::parse("xlarge").unwrap(), BookFontSize::Px48);
        assert_eq!(BookFontSize::Px24.next(), BookFontSize::Px32);
        assert_eq!(
            BookFontSize::ALL.map(BookFontSize::pixels),
            [16, 20, 24, 32, 48, 72]
        );
    }

    #[test]
    fn parses_serializes_and_cycles_reader_preferences() {
        let parsed = ReaderPreferences::parse(
            "version=1\ntheme=high-contrast\norientation=landscape\nfont_size=xlarge\nbook_font=serif\nparagraph_alignment=right\nshow_progress=false\n",
        )
        .unwrap();
        assert_eq!(parsed.theme, ReadingTheme::HighContrast);
        assert_eq!(parsed.orientation, ReaderOrientation::Landscape);
        assert_eq!(parsed.font_size, BookFontSize::Px48);
        assert_eq!(parsed.book_font, BookFont::Serif);
        assert_eq!(parsed.paragraph_alignment, ParagraphAlignment::Right);
        assert!(!parsed.show_progress);
        assert!(parsed.serialized().contains("font_size=48"));
        assert!(parsed.serialized().contains("book_font=serif"));
        assert!(parsed.serialized().contains("paragraph_alignment=right"));
    }

    #[test]
    fn layout_changes_request_first_page_first_rebuild_and_persist_preferences() {
        let root = temp_dir("prefs-books");
        let state = temp_dir("prefs-state");
        fs::write(root.join("Book.txt"), "hello world ".repeat(800)).unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.refresh_library();
        reader.library_selected = 1;
        assert!(reader.apply_library_button(ButtonEvent::Select));
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::FirstPageReady);
        assert!(reader.cycle_book_font_size());
        assert_eq!(
            reader.loading_stage(),
            Some(ReaderLoadingStage::UpdatingLayout)
        );
        assert!(state.join(READER_PREFS_FILE).exists());
    }

    #[test]
    fn books_and_files_reopen_from_per_book_positions_while_bookmarks_remain_explicit() {
        let root = temp_dir("positions-books");
        let state = temp_dir("positions-state");
        fs::write(root.join("A.txt"), "alpha body ".repeat(1200)).unwrap();
        fs::write(root.join("B.txt"), "beta body ".repeat(1200)).unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.refresh_library();
        reader.library_selected = 1;
        assert!(reader.apply_library_button(ButtonEvent::Select));
        for _ in 0..3 {
            reader.tick();
        }
        reader.next_page();
        reader.next_page();
        let saved = reader.session.as_ref().unwrap().current_location();
        assert_eq!(saved.page_index, 2);

        reader.refresh_library();
        reader.library_selected = 1;
        assert!(reader.apply_library_button(ButtonEvent::Select));
        for _ in 0..4 {
            reader.tick();
        }
        assert_eq!(reader.session.as_ref().unwrap().current_absolute_page(), 2);

        let mut explicit = saved.clone();
        explicit.page_index = 1;
        explicit.byte_offset = reader.session.as_ref().unwrap().page_offsets[1];
        reader.bookmarks = vec![explicit];
        assert!(reader.request_open_bookmark(0));
        for _ in 0..4 {
            reader.tick();
        }
        assert_eq!(reader.session.as_ref().unwrap().current_absolute_page(), 1);
    }

    #[test]
    fn paragraph_alignment_defaults_to_justified_and_changes_cache_fingerprint_inputs() {
        let justified = ReaderPreferences::default();
        assert_eq!(justified.paragraph_alignment, ParagraphAlignment::Justified);
        let mut left = justified;
        left.paragraph_alignment = ParagraphAlignment::Left;
        assert_ne!(justified.layout(), left.layout());
    }

    #[test]
    fn preference_editor_uses_move_then_select_change_policy() {
        let mut reader = ReaderUiState::default();
        reader.begin_preferences_edit();
        assert_eq!(
            reader.selected_preference(),
            ReadingPreference::BookFontSize
        );
        reader.cycle_preference_next();
        assert_eq!(reader.selected_preference(), ReadingPreference::BookFont);
        reader.cycle_preference_previous();
        assert_eq!(
            reader.selected_preference(),
            ReadingPreference::BookFontSize
        );
        assert!(!reader.activate_selected_preference());
        assert_eq!(reader.preferences.font_size, BookFontSize::Px32);
    }

    #[test]
    fn reader_owned_runtime_filenames_are_fat83_safe() {
        for name in [
            READER_STATE_FILE,
            READER_POSITIONS_FILE,
            READER_RECENT_FILE,
            READER_BOOKMARKS_FILE,
            READER_PREFS_FILE,
            "ED9B69AF.CCH",
            "ED9B69AF.TMP",
            "ED9B69AF.BAK",
        ] {
            assert!(is_fat83_safe_file_name(Path::new(name)), "{name}");
        }
        assert!(!is_fat83_safe_file_name(Path::new(
            LEGACY_READER_POSITIONS_FILE
        )));
        assert!(!is_fat83_safe_file_name(Path::new("BED9B69AF.CCH")));
    }

    #[test]
    fn cache_filename_uses_exactly_eight_hexadecimal_characters() {
        let root = temp_dir("fat83-cache-books");
        let state = temp_dir("fat83-cache-state");
        fs::write(root.join("Book.txt"), "text body ".repeat(1000)).unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.refresh_library();
        let book = reader.books.first().unwrap();
        let cache = reader.cache_path_for(book, reader.preferences.layout());
        let file = cache.file_name().unwrap().to_str().unwrap();
        assert_eq!(file.len(), 12);
        assert_eq!(&file[8..], ".CCH");
        assert!(file[..8].bytes().all(|value| value.is_ascii_hexdigit()));
        assert!(is_fat83_safe_file_name(&cache));
    }

    #[test]
    fn legacy_positions_file_migrates_to_short_name_safe_primary() {
        let root = temp_dir("legacy-positions-books");
        let state = temp_dir("legacy-positions-state");
        fs::write(root.join("Book.txt"), "text body ".repeat(1000)).unwrap();
        let legacy = state.join(LEGACY_READER_POSITIONS_FILE);
        fs::write(
            &legacy,
            "version=1\nentry=Book.txt\tBook\ttxt\t1000\t0\t3\t42\n",
        )
        .unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        let report = reader.load_persistent_state();
        assert_eq!(report.position_count, 1);
        assert!(state.join(READER_POSITIONS_FILE).exists());
        assert_eq!(reader.positions[0].byte_offset, 42);
    }

    #[test]
    fn fat83_runtime_primary_temp_and_backup_paths_are_safe_without_cache_prefix() {
        let root = temp_dir("fat83-runtime-books");
        let state = temp_dir("fat83-runtime-state");
        fs::write(root.join("Book.txt"), "text body ".repeat(1000)).unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.refresh_library();
        let book = reader.books.first().unwrap();
        let positions = reader.positions_path();
        let cache = reader.cache_path_for(book, reader.preferences.layout());
        for path in [
            positions.clone(),
            super::with_extension(&positions, "TMP"),
            super::with_extension(&positions, "BAK"),
            cache.clone(),
            super::with_extension(&cache, "TMP"),
            super::with_extension(&cache, "BAK"),
        ] {
            assert!(is_fat83_safe_file_name(&path), "{}", path.display());
        }
        let cache_file = cache.file_name().unwrap().to_str().unwrap();
        assert_eq!(&cache_file[8..], ".CCH");
        assert!(
            !cache_file.starts_with('B')
                || cache_file[..8]
                    .bytes()
                    .all(|value| value.is_ascii_hexdigit())
        );
        assert_eq!(cache_file[..8].len(), 8);
    }

    #[test]
    fn bookmark_page_label_uses_active_layout_offsets_and_stored_fallback() {
        let book = ReaderBook {
            path: "Book.txt".into(),
            title: "Book".into(),
            format: BookFormat::Text,
            size_bytes: 1000,
            modified_seconds: 0,
        };
        let bookmark = ReaderLocation {
            path: book.path.clone(),
            title: book.title.clone(),
            format: book.format,
            size_bytes: book.size_bytes,
            modified_seconds: book.modified_seconds,
            page_index: 8,
            byte_offset: 220,
            epub_chapter: None,
        };
        let mut reader = ReaderUiState::default();
        assert_eq!(reader.bookmark_display_page(&bookmark), 9);
        reader.session = Some(ReaderSession {
            book,
            encoding: TextEncoding::Utf8,
            epub_document: None,
            layout: ReaderPreferences::default().layout(),
            current_page: 0,
            page_number_base: 0,
            page_offsets: vec![0, 100, 200, 300],
            indexed_through: 300,
            index_complete: false,
            index_truncated: false,
            epub_page_numbers_exact: false,
            cache: Vec::new(),
            epub_chapter_pages: Vec::new(),
            epub_anchor_index: None,
        });
        assert_eq!(reader.bookmark_display_page(&bookmark), 3);
    }

    #[test]
    fn epub_chapter_labels_use_chapter_relative_page_totals_and_persist() {
        let label = ReaderChapterPageLabel {
            chapter_number: 3,
            page_number: 2,
            page_count: 9,
            approximate: false,
        };
        let location = ReaderLocation {
            path: "book.epub".into(),
            title: "Book title".into(),
            format: BookFormat::Epub,
            size_bytes: 100,
            modified_seconds: 7,
            page_index: 11,
            byte_offset: 55,
            epub_chapter: Some(label.clone()),
        };
        assert_eq!(
            parse_location_record(&serialize_location(&location)).unwrap(),
            location
        );
        assert_eq!(
            parse_location_fields(&serialize_location_fields(&location)).unwrap(),
            location
        );
        assert_eq!(label.page_text(), "2/9");
    }

    #[test]
    fn legacy_location_fields_without_chapter_metadata_remain_readable() {
        let location = parse_location_fields("book.txt\tBook\ttxt\t10\t0\t2\t5").unwrap();
        assert_eq!(location.format, BookFormat::Text);
        assert_eq!(location.epub_chapter, None);
    }

    #[test]
    fn epub_chapter_index_cooperative_yield_policy_is_bounded() {
        assert_eq!(READER_EPUB_INDEX_YIELD_EVERY_PAGES, 4);
        assert_eq!(READER_EPUB_INDEX_YIELD_MILLIS, 1);
    }

    #[test]
    fn opening_another_book_releases_the_active_session_before_loading() {
        let root = temp_dir("release-session-books");
        let state = temp_dir("release-session-state");
        let first = root.join("First.txt");
        let second = root.join("Second.txt");
        fs::write(&first, "first book body ".repeat(100)).unwrap();
        fs::write(&second, "second book body ".repeat(100)).unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.refresh_library();
        let first_path = first.to_string_lossy();
        let second_path = second.to_string_lossy();
        let first_book = reader
            .books
            .iter()
            .find(|book| book.path == first_path.as_ref())
            .unwrap()
            .clone();
        let second_book = reader
            .books
            .iter()
            .find(|book| book.path == second_path.as_ref())
            .unwrap()
            .clone();
        reader.request_open_book(first_book, None);
        while reader.tick() != ReaderTickOutcome::FirstPageReady {}
        assert!(reader.session.is_some());
        reader.request_open_book(second_book, None);
        assert!(reader.session.is_none());
        assert_eq!(
            reader.loading_stage(),
            Some(ReaderLoadingStage::OpeningFile)
        );
    }

    fn push_u16(output: &mut Vec<u8>, value: u16) {
        output.extend(value.to_le_bytes());
    }

    fn push_u32(output: &mut Vec<u8>, value: u32) {
        output.extend(value.to_le_bytes());
    }

    fn stored_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut output = Vec::new();
        let mut central = Vec::new();
        for (name, body) in entries {
            let offset = output.len() as u32;
            push_u32(&mut output, 0x0403_4B50);
            push_u16(&mut output, 20);
            push_u16(&mut output, 0);
            push_u16(&mut output, 0);
            push_u16(&mut output, 0);
            push_u16(&mut output, 0);
            push_u32(&mut output, 0);
            push_u32(&mut output, body.len() as u32);
            push_u32(&mut output, body.len() as u32);
            push_u16(&mut output, name.len() as u16);
            push_u16(&mut output, 0);
            output.extend(name.as_bytes());
            output.extend(body.as_bytes());

            push_u32(&mut central, 0x0201_4B50);
            push_u16(&mut central, 20);
            push_u16(&mut central, 20);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u32(&mut central, 0);
            push_u32(&mut central, body.len() as u32);
            push_u32(&mut central, body.len() as u32);
            push_u16(&mut central, name.len() as u16);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u32(&mut central, 0);
            push_u32(&mut central, offset);
            central.extend(name.as_bytes());
        }
        let central_offset = output.len() as u32;
        let central_size = central.len() as u32;
        output.extend(central);
        push_u32(&mut output, 0x0605_4B50);
        push_u16(&mut output, 0);
        push_u16(&mut output, 0);
        push_u16(&mut output, entries.len() as u16);
        push_u16(&mut output, entries.len() as u16);
        push_u32(&mut output, central_size);
        push_u32(&mut output, central_offset);
        push_u16(&mut output, 0);
        output
    }

    fn cjk_paragraphs(count: usize) -> String {
        let mut body = String::from("<html><body>");
        let glyphs = ['远', '救', '世', '主'];
        for index in 0..count {
            body.push_str("<p>");
            body.push(glyphs[index % glyphs.len()]);
            body.push_str("</p>");
        }
        body.push_str("</body></html>");
        body
    }

    fn write_cjk_epub(directory: &Path, chapter_one: usize, chapter_two: usize) -> PathBuf {
        let path = directory.join("novel.epub");
        let first = cjk_paragraphs(chapter_one);
        let second = cjk_paragraphs(chapter_two);
        let bytes = stored_zip(&[
            (
                "META-INF/container.xml",
                "<container><rootfiles><rootfile full-path='OEBPS/book.opf'/></rootfiles></container>",
            ),
            (
                "OEBPS/book.opf",
                "<package><metadata><dc:title>遥远的救世主</dc:title></metadata><manifest><item id='c1' href='c1.xhtml' media-type='application/xhtml+xml'/><item id='c2' href='c2.xhtml' media-type='application/xhtml+xml'/></manifest><spine><itemref idref='c1'/><itemref idref='c2'/></spine></package>",
            ),
            ("OEBPS/c1.xhtml", first.as_str()),
            ("OEBPS/c2.xhtml", second.as_str()),
        ]);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn open_until_ready(reader: &mut ReaderUiState) {
        for _ in 0..8 {
            let outcome = reader.tick();
            if outcome == ReaderTickOutcome::FirstPageReady {
                return;
            }
            assert_ne!(
                outcome,
                ReaderTickOutcome::Failed,
                "{}",
                reader
                    .loading
                    .as_ref()
                    .map(|loading| loading.message.as_str())
                    .unwrap_or("failed")
            );
        }
        panic!(
            "book did not open: {}",
            reader
                .loading
                .as_ref()
                .map(|loading| loading.message.as_str())
                .unwrap_or("stuck")
        );
    }

    #[test]
    fn long_cjk_epub_opens_before_full_indexing_and_passes_the_old_anchor_cap() {
        assert!(READER_EPUB_PAGE_ANCHOR_LIMIT >= 200_000);
        assert!(
            READER_EPUB_ANCHOR_INDEX_BYTES_LIMIT
                >= crate::epub_page_index::ANCHOR_BASE + READER_EPUB_PAGE_ANCHOR_LIMIT * 8
        );
        let root = temp_dir("long-cjk-books");
        let state = temp_dir("long-cjk-state");
        let mut probe = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        probe.preferences.font_size = BookFontSize::Px16;
        probe.preferences.book_font = BookFont::CjkUnifont;
        let lines = probe.preferences.layout().lines_per_page.max(2);
        let long_paragraphs = lines * 2_200;
        write_cjk_epub(&root, 2, long_paragraphs);

        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.preferences.font_size = BookFontSize::Px16;
        reader.preferences.book_font = BookFont::CjkUnifont;
        reader.refresh_library();
        let book = reader
            .books
            .iter()
            .find(|book| book.format == BookFormat::Epub)
            .unwrap()
            .clone();
        reader.request_open_book(book, None);
        open_until_ready(&mut reader);
        let session = reader.session.as_ref().unwrap();
        assert!(!session.index_complete);
        assert_eq!(session.indexed_page_count(), 0);
        assert!(session.page_offsets.len() <= READER_EPUB_READING_WINDOW);
        let page_text: String = session
            .current_cached_page()
            .unwrap()
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect();
        assert!(
            page_text.chars().any(|character| !character.is_ascii()),
            "{page_text}"
        );
        assert!(!session.display_page_label().contains("Unable"));

        for _ in 0..3 {
            assert_ne!(reader.tick(), ReaderTickOutcome::Failed);
        }
        assert!(reader.session.as_ref().unwrap().indexed_page_count() > 0);
        assert!(!reader.session.as_ref().unwrap().index_complete);

        {
            let session = reader.session.as_mut().unwrap();
            while session.indexed_page_count() <= 4096
                && !session.index_complete
                && !session.index_truncated
            {
                session.index_epub_batch(128).unwrap();
            }
        }
        let session = reader.session.as_ref().unwrap();
        assert!(session.indexed_page_count() > 4096);
        assert!(!session.index_truncated);
        assert!(session.epub_index_on_sd());
        assert!(session.page_offsets.len() <= READER_EPUB_READING_WINDOW);
        assert!(session.epub_chapter_pages.len() >= 2);
        assert!(session.epub_chapter_pages[0].complete);
        assert!(session.epub_chapter_pages[0].indexed_pages < 32);
        assert!(session.epub_chapter_pages[1].indexed_pages > 4096);
        let index_path = session
            .epub_anchor_index
            .as_ref()
            .and_then(|index| index.path())
            .unwrap()
            .to_path_buf();
        let index_len = fs::metadata(&index_path).unwrap().len();
        assert!(index_len > 4096 * 8);
        assert!(index_len <= READER_EPUB_ANCHOR_INDEX_BYTES_LIMIT as u64);
        assert!(is_fat83_safe_file_name(&index_path));
        reader.next_page();
        assert!(reader.session.is_some());
        assert!(!reader
            .last_message
            .as_deref()
            .unwrap_or("")
            .contains("4096"));
    }

    #[test]
    fn epub_anchor_cap_keeps_the_book_open_with_approximate_progress() {
        let root = temp_dir("cap-cjk-books");
        let state = temp_dir("cap-cjk-state");
        write_cjk_epub(&root, 1, 80);
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.preferences.font_size = BookFontSize::Px72;
        reader.preferences.book_font = BookFont::CjkUnifont;
        reader.set_epub_index_limits(5, usize::MAX);
        reader.refresh_library();
        let book = reader.books[0].clone();
        reader.request_open_book(book, None);
        open_until_ready(&mut reader);
        {
            let session = reader.session.as_mut().unwrap();
            for _ in 0..8 {
                if session.index_truncated {
                    break;
                }
                session.index_epub_batch(4).unwrap();
            }
        }
        let session = reader.session.as_ref().unwrap();
        assert!(session.index_truncated);
        assert!(!session.index_complete);
        assert!(session.indexed_page_count() <= 5);
        assert!(session.display_page_label().contains('~') || session.page_label().contains('~'));
        reader.next_page();
        assert!(reader
            .session
            .as_ref()
            .unwrap()
            .current_cached_page()
            .is_some());
        assert_ne!(reader.loading_stage(), Some(ReaderLoadingStage::Failed));
    }

    #[test]
    fn indexed_epub_reopens_from_the_sd_page_index() {
        let root = temp_dir("reopen-cjk-books");
        let state = temp_dir("reopen-cjk-state");
        write_cjk_epub(&root, 4, 8);
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.preferences.font_size = BookFontSize::Px32;
        reader.preferences.book_font = BookFont::CjkUnifont;
        reader.refresh_library();
        let book = reader.books[0].clone();
        reader.request_open_book(book.clone(), None);
        open_until_ready(&mut reader);
        {
            let session = reader.session.as_mut().unwrap();
            for _ in 0..40 {
                if session.index_complete {
                    break;
                }
                session.index_epub_batch(32).unwrap();
            }
        }
        let pages = reader.session.as_ref().unwrap().indexed_page_count();
        assert!(pages > 1);
        assert!(reader.session.as_ref().unwrap().index_complete);

        let mut restored = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        restored.preferences.font_size = BookFontSize::Px32;
        restored.preferences.book_font = BookFont::CjkUnifont;
        restored.refresh_library();
        restored.request_open_book(book, None);
        open_until_ready(&mut restored);
        let session = restored.session.as_ref().unwrap();
        assert_eq!(session.indexed_page_count(), pages);
        assert!(session.index_complete);
        assert!(session.display_page_label().contains('/'));
        assert!(!session.display_page_label().contains('+'));
    }

    #[test]
    fn txt_reader_has_no_page_anchor_refusal_and_resumes_past_the_cache_window() {
        let root = temp_dir("long-txt-books");
        let state = temp_dir("long-txt-state");
        fs::write(root.join("notes.txt"), "中文阅读 ".repeat(4_000)).unwrap();
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.refresh_library();
        reader.library_selected = 1;
        assert!(reader.apply_library_button(ButtonEvent::Select));
        open_until_ready(&mut reader);
        for _ in 0..40 {
            reader.next_page();
        }
        assert!(reader.session.is_some());
        assert!(!reader
            .last_message
            .as_deref()
            .unwrap_or("")
            .contains("page anchor"));

        let cache = state
            .join("CACHE")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = fs::read_to_string(&cache).unwrap();
        let mut kept = Vec::new();
        for line in text.lines() {
            if line.starts_with("offset=") || line.starts_with("complete=") {
                continue;
            }
            kept.push(line.to_string());
        }
        kept.push("complete=true".into());
        for offset in 0..READER_CACHE_OFFSET_LIMIT {
            kept.push(format!("offset={offset}"));
        }
        fs::write(&cache, format!("{}\n", kept.join("\n"))).unwrap();

        let book = reader.session.as_ref().unwrap().book.clone();
        let mut past_window = reader.session.as_ref().unwrap().current_location();
        past_window.page_index = READER_CACHE_OFFSET_LIMIT + 20;
        past_window.byte_offset = 20_000;
        let mut resumed = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        resumed.request_open_book(book.clone(), Some(past_window));
        open_until_ready(&mut resumed);
        let session = resumed.session.as_ref().unwrap();
        assert_eq!(session.current_location().byte_offset, 20_000);
        assert!(!session.index_complete);
        resumed.next_page();
        assert!(resumed.session.is_some());

        let mut inside = reader.session.as_ref().unwrap().current_location();
        inside.page_index = 3;
        inside.byte_offset = 3;
        let mut early = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        early.request_open_book(book, Some(inside));
        open_until_ready(&mut early);
        assert!(!early.session.as_ref().unwrap().index_complete);
        assert_eq!(early.session.as_ref().unwrap().current_absolute_page(), 3);
    }

    #[test]
    fn repeated_degraded_persistence_events_are_suppressed_until_status_changes() {
        let mut reader = ReaderUiState::default();
        reader.finish_persistence("anchor-cache", vec!["CACHE: failed".into()]);
        assert!(reader.take_persistence_event().is_some());
        reader.finish_persistence("anchor-cache", vec!["CACHE: failed".into()]);
        assert!(reader.take_persistence_event().is_none());
        reader.finish_persistence("anchor-cache", Vec::new());
        assert_eq!(
            reader.take_persistence_event().as_deref(),
            Some("status=saved scope=anchor-cache")
        );
    }
}
