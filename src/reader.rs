//! Offline Reader state, TXT / EPUB pagination and Reader-owned persistence.
//!
//! EPUB layout is chapter-on-demand. Opening a book or jumping to a saved
//! offset lays out only the spine item that contains that offset, so a long
//! novel becomes readable before the rest of its chapters are paginated.
//! Neighbouring chapters are laid out when the reader reaches a chapter
//! boundary. Page numbers are chapter-local (`chapter i/N` plus the page
//! within that chapter). The byte offset remains the progress anchor, so
//! older whole-book page indexes are ignored on restore.
// rustmix-wave=epub-chapter-on-demand-ready

use std::{
    cell::Cell,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    ptr::NonNull,
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
/// Safety ceiling for page anchors stored for one EPUB spine item.
///
/// A chapter that reaches the cap stays readable. Further pages in that
/// chapter use an approximate page number instead of refusing the book.
pub const READER_EPUB_PAGE_ANCHOR_LIMIT: usize = 262_144;
/// Byte cap for one chapter anchor cache, including its header. Layout stops
/// at whichever of the page ceiling or this byte cap is reached first.
pub const READER_EPUB_ANCHOR_INDEX_BYTES_LIMIT: usize =
    8 * 1024 + READER_EPUB_PAGE_ANCHOR_LIMIT * 8;
/// EPUB pages laid out on each background tick after the current page is open.
/// Only the current spine item is advanced; the next chapter waits until the
/// reader reaches this chapter's end.
pub const READER_EPUB_BACKGROUND_INDEX_PAGES: usize = 16;
/// Number of EPUB pages laid out before the main loop can poll buttons.
/// The pause also lets the ESP-IDF idle task feed its watchdog.
pub const READER_EPUB_INDEX_YIELD_EVERY_PAGES: usize = 4;
/// Cooperative pause used between EPUB chapter layout batches.
pub const READER_EPUB_INDEX_YIELD_MILLIS: u64 = 1;

const READER_PERSISTENCE_VERSION: &str = "1";
/// Bumped when the cache fingerprint gains a layout preference. Version 3 is
/// the chapter-on-demand baseline (path, size, mtime, line count, line width,
/// font, orientation, alignment). Version 4 adds letter spacing, line and
/// paragraph spacing, margins, indent, justification, and Chinese script.
/// Version 5 adds immersive reading, which changes the content box.
/// Version 6 adds the SD CJK font file name.
const READER_CACHE_VERSION: &str = "6";
/// Page turns between full refreshes when dark mode is on and the cadence is Off.
const DARK_MODE_FULL_REFRESH_TURNS: u8 = 5;
/// How long a long-press status overlay stays on an immersive page.
pub const IMMERSIVE_STATUS_MS: u64 = 2_500;
/// Left and right inset while immersive. Top and bottom stay at the panel edge.
const IMMERSIVE_SIDE_PX: u8 = 8;
const READER_PREFS_VERSION: &str = "2";
const READER_PREFS_VERSION_V1: &str = "1";
const CACHE_FNV_OFFSET: u64 = 0xcbf29ce484222325;
const CACHE_FNV_PRIME: u64 = 0x100000001b3;

std::thread_local! {
    static LAYOUT_BUTTON_POLL: Cell<Option<NonNull<dyn FnMut() -> bool + 'static>>> =
        const { Cell::new(None) };
}

/// Run `body` while EPUB layout batches can poll buttons.
///
/// The main loop installs a check that reports whether a key is down. Layout
/// stops at the next batch boundary so the loop can deliver the press instead
/// of paginating through it.
pub fn with_layout_button_poll<R>(poll: &mut dyn FnMut() -> bool, body: impl FnOnce() -> R) -> R {
    struct Restore(Option<NonNull<dyn FnMut() -> bool + 'static>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LAYOUT_BUTTON_POLL.with(|slot| slot.set(self.0.take()));
        }
    }

    LAYOUT_BUTTON_POLL.with(|slot| {
        let previous = slot.get();
        let poll_ptr = std::ptr::from_mut::<dyn FnMut() -> bool>(poll);
        // SAFETY: `poll` stays borrowed until `body` returns. The `'static`
        // erasure only lets the thread-local hold the fat pointer; `Restore`
        // clears that slot before this function returns, including on panic.
        let poll_ptr = unsafe {
            NonNull::new_unchecked(std::mem::transmute::<
                *mut dyn FnMut() -> bool,
                *mut (dyn FnMut() -> bool + 'static),
            >(poll_ptr))
        };
        slot.set(Some(poll_ptr));
        let _restore = Restore(previous);
        body()
    })
}

fn layout_button_pending() -> bool {
    LAYOUT_BUTTON_POLL.with(|slot| slot.get().is_some_and(|poll| unsafe { (*poll.as_ptr())() }))
}

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
    /// Readable chapters in the book. Zero means a legacy record that did not
    /// store the total; the UI then shows the chapter number alone.
    pub chapter_count: usize,
    pub page_number: usize,
    pub page_count: usize,
    /// Set when `page_count` is estimated or still unknown. Unknown totals
    /// render as `n+`; estimated totals render as `n/~m`.
    pub approximate: bool,
}

impl ReaderChapterPageLabel {
    /// `i/N` when the chapter total is known, otherwise just `i`.
    #[must_use]
    pub fn chapter_text(&self) -> String {
        if self.chapter_count == 0 {
            self.chapter_number.to_string()
        } else {
            format!(
                "{}/{}",
                self.chapter_number,
                self.chapter_count.max(self.chapter_number)
            )
        }
    }

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

/// Extra advance between glyphs, in e-paper pixels. Zero keeps the historical
/// wrap. CJK and Latin both receive the same extra advance.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LetterSpacing {
    #[default]
    Px0,
    Px1,
    Px2,
    Px3,
    Px4,
}

impl LetterSpacing {
    pub const ALL: [Self; 5] = [Self::Px0, Self::Px1, Self::Px2, Self::Px3, Self::Px4];

    #[must_use]
    pub const fn pixels(self) -> u8 {
        match self {
            Self::Px0 => 0,
            Self::Px1 => 1,
            Self::Px2 => 2,
            Self::Px3 => 3,
            Self::Px4 => 4,
        }
    }

    pub fn from_pixels(px: u8) -> Result<Self, String> {
        match px {
            0 => Ok(Self::Px0),
            1 => Ok(Self::Px1),
            2 => Ok(Self::Px2),
            3 => Ok(Self::Px3),
            4 => Ok(Self::Px4),
            other => Err(format!("unsupported letter_spacing value {other}")),
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Px0 => "0 px",
            Self::Px1 => "1 px",
            Self::Px2 => "2 px",
            Self::Px3 => "3 px",
            Self::Px4 => "4 px",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Px0 => "0",
            Self::Px1 => "1",
            Self::Px2 => "2",
            Self::Px3 => "3",
            Self::Px4 => "4",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Px0 => Self::Px1,
            Self::Px1 => Self::Px2,
            Self::Px2 => Self::Px3,
            Self::Px3 => Self::Px4,
            Self::Px4 => Self::Px0,
        }
    }

    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::Px0 => Self::Px4,
            Self::Px1 => Self::Px0,
            Self::Px2 => Self::Px1,
            Self::Px3 => Self::Px2,
            Self::Px4 => Self::Px3,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "0" | "tight" => Ok(Self::Px0),
            "1" | "normal" => Ok(Self::Px1),
            "2" | "loose" => Ok(Self::Px2),
            "3" => Ok(Self::Px3),
            "4" | "extra" => Ok(Self::Px4),
            other => Err(format!("unsupported letter_spacing value {other:?}")),
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

/// Extra leading added on top of the font's built-in line step.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LineSpacing {
    #[default]
    Px0,
    Px4,
    Px8,
    Px12,
}

impl LineSpacing {
    #[must_use]
    pub const fn pixels(self) -> u8 {
        match self {
            Self::Px0 => 0,
            Self::Px4 => 4,
            Self::Px8 => 8,
            Self::Px12 => 12,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Px0 => "0 px",
            Self::Px4 => "4 px",
            Self::Px8 => "8 px",
            Self::Px12 => "12 px",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Px0 => "0",
            Self::Px4 => "4",
            Self::Px8 => "8",
            Self::Px12 => "12",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Px0 => Self::Px4,
            Self::Px4 => Self::Px8,
            Self::Px8 => Self::Px12,
            Self::Px12 => Self::Px0,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "0" => Ok(Self::Px0),
            "4" => Ok(Self::Px4),
            "8" => Ok(Self::Px8),
            "12" => Ok(Self::Px12),
            other => Err(format!("unsupported line_spacing value {other:?}")),
        }
    }
}

/// Blank lines inserted after a paragraph. Counted inside `lines_per_page`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ParagraphSpacing {
    #[default]
    Lines0,
    Lines1,
    Lines2,
}

impl ParagraphSpacing {
    #[must_use]
    pub const fn lines(self) -> u8 {
        match self {
            Self::Lines0 => 0,
            Self::Lines1 => 1,
            Self::Lines2 => 2,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Lines0 => "0",
            Self::Lines1 => "1 line",
            Self::Lines2 => "2 lines",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Lines0 => "0",
            Self::Lines1 => "1",
            Self::Lines2 => "2",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Lines0 => Self::Lines1,
            Self::Lines1 => Self::Lines2,
            Self::Lines2 => Self::Lines0,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "0" => Ok(Self::Lines0),
            "1" => Ok(Self::Lines1),
            "2" => Ok(Self::Lines2),
            other => Err(format!("unsupported paragraph_spacing value {other:?}")),
        }
    }
}

/// Extra inset subtracted from the historical content box, per edge.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PageMargin {
    #[default]
    Px0,
    Px8,
    Px16,
    Px24,
}

impl PageMargin {
    #[must_use]
    pub const fn pixels(self) -> u8 {
        match self {
            Self::Px0 => 0,
            Self::Px8 => 8,
            Self::Px16 => 16,
            Self::Px24 => 24,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Px0 => "0",
            Self::Px8 => "8 px",
            Self::Px16 => "16 px",
            Self::Px24 => "24 px",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Px0 => "0",
            Self::Px8 => "8",
            Self::Px16 => "16",
            Self::Px24 => "24",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Px0 => Self::Px8,
            Self::Px8 => Self::Px16,
            Self::Px16 => Self::Px24,
            Self::Px24 => Self::Px0,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "0" => Ok(Self::Px0),
            "8" => Ok(Self::Px8),
            "16" => Ok(Self::Px16),
            "24" => Ok(Self::Px24),
            other => Err(format!("unsupported margin value {other:?}")),
        }
    }
}

/// How often a page turn asks for a full panel refresh to clear ghosting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FullRefreshEvery {
    #[default]
    Off,
    Turns5,
    Turns10,
    Turns20,
}

impl FullRefreshEvery {
    #[must_use]
    pub const fn turns(self) -> Option<u8> {
        match self {
            Self::Off => None,
            Self::Turns5 => Some(5),
            Self::Turns10 => Some(10),
            Self::Turns20 => Some(20),
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Turns5 => "5",
            Self::Turns10 => "10",
            Self::Turns20 => "20",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Turns5 => "5",
            Self::Turns10 => "10",
            Self::Turns20 => "20",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Turns5,
            Self::Turns5 => Self::Turns10,
            Self::Turns10 => Self::Turns20,
            Self::Turns20 => Self::Off,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "0" => Ok(Self::Off),
            "5" => Ok(Self::Turns5),
            "10" => Ok(Self::Turns10),
            "20" => Ok(Self::Turns20),
            other => Err(format!("unsupported full_refresh value {other:?}")),
        }
    }
}

/// Automatic forward page turn. Off leaves the light-sleep wait unchanged.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AutoPageTurn {
    #[default]
    Off,
    Secs15,
    Secs30,
    Secs60,
    Secs120,
}

impl AutoPageTurn {
    #[must_use]
    pub const fn seconds(self) -> Option<u64> {
        match self {
            Self::Off => None,
            Self::Secs15 => Some(15),
            Self::Secs30 => Some(30),
            Self::Secs60 => Some(60),
            Self::Secs120 => Some(120),
        }
    }

    #[must_use]
    pub const fn millis(self) -> Option<u64> {
        match self.seconds() {
            Some(seconds) => Some(seconds.saturating_mul(1_000)),
            None => None,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Secs15 => "15 s",
            Self::Secs30 => "30 s",
            Self::Secs60 => "60 s",
            Self::Secs120 => "120 s",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Secs15 => "15",
            Self::Secs30 => "30",
            Self::Secs60 => "60",
            Self::Secs120 => "120",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Secs15,
            Self::Secs15 => Self::Secs30,
            Self::Secs30 => Self::Secs60,
            Self::Secs60 => Self::Secs120,
            Self::Secs120 => Self::Off,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "0" => Ok(Self::Off),
            "15" => Ok(Self::Secs15),
            "30" => Ok(Self::Secs30),
            "60" => Ok(Self::Secs60),
            "120" => Ok(Self::Secs120),
            other => Err(format!("unsupported auto_page_turn value {other:?}")),
        }
    }
}

/// One-tap combinations of the layout-affecting reading preferences.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadingPreset {
    Compact,
    Comfortable,
    LargePrint,
}

impl ReadingPreset {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Comfortable => "Comfortable",
            Self::LargePrint => "Large",
        }
    }
}

/// Layout dimensions affecting TXT pagination and both cache fingerprints.
///
/// TXT anchor files and EPUB chapter files both start from [`book_fingerprint`].
/// Fields here are the only layout preferences that hash participates in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReaderLayout {
    pub chars_per_line: usize,
    pub lines_per_page: usize,
    pub max_line_width_px: i32,
    pub ascii_advance_px: i32,
    pub font_size_px: u8,
    pub letter_spacing_px: u8,
    pub line_spacing_px: u8,
    pub paragraph_gap_lines: u8,
    pub margin_top_px: u8,
    pub margin_bottom_px: u8,
    pub margin_left_px: u8,
    pub margin_right_px: u8,
    pub first_line_indent: bool,
    pub justified: bool,
    pub orientation: ReaderOrientation,
    pub font_size: BookFontSize,
    pub book_font: BookFont,
    pub letter_spacing: LetterSpacing,
    pub line_spacing: LineSpacing,
    pub paragraph_spacing: ParagraphSpacing,
    pub paragraph_alignment: ParagraphAlignment,
    pub chinese_script: crate::reader_hanzi::ChineseScript,
    pub immersive: bool,
    pub sd_cjk_file: [u8; 13],
}

impl ReaderLayout {
    /// Glyph advance for TXT, EPUB, and WeRead pagination. Letter spacing is
    /// added for every character, Latin and CJK alike.
    #[must_use]
    pub fn advance_px(self, character: char) -> i32 {
        let base = if character.is_ascii() {
            self.ascii_advance_px
        } else {
            crate::fonts::unicode_advance(character, self.font_size_px, self.ascii_advance_px as u8)
        };
        base.saturating_add(i32::from(self.letter_spacing_px))
    }

    /// Width reserved on the first line of a paragraph: two CJK cells.
    #[must_use]
    pub fn indent_px(self) -> i32 {
        if self.first_line_indent {
            2 * (i32::from(self.font_size_px) + i32::from(self.letter_spacing_px))
        } else {
            0
        }
    }
}

/// Reader-owned preference file persisted as `/RUSTMIX/READER/PREFS.TXT`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReaderPreferences {
    pub theme: ReadingTheme,
    pub orientation: ReaderOrientation,
    pub font_size: BookFontSize,
    pub book_font: BookFont,
    pub letter_spacing: LetterSpacing,
    pub line_spacing: LineSpacing,
    pub paragraph_spacing: ParagraphSpacing,
    pub margin_top: PageMargin,
    pub margin_bottom: PageMargin,
    pub margin_left: PageMargin,
    pub margin_right: PageMargin,
    pub first_line_indent: bool,
    pub justified: bool,
    pub paragraph_alignment: ParagraphAlignment,
    pub chinese_script: crate::reader_hanzi::ChineseScript,
    pub dark_mode: bool,
    pub full_refresh: FullRefreshEvery,
    pub show_progress: bool,
    pub status_page: bool,
    pub status_chapter: bool,
    pub status_time: bool,
    pub status_battery: bool,
    pub swap_page_keys: bool,
    pub long_press_chapter: bool,
    pub auto_page_turn: AutoPageTurn,
    pub immersive: bool,
    sd_cjk_file: [u8; 13],
}

impl Default for ReaderPreferences {
    fn default() -> Self {
        Self {
            theme: ReadingTheme::Classic,
            orientation: ReaderOrientation::Portrait,
            font_size: BookFontSize::Px24,
            book_font: BookFont::Serif,
            letter_spacing: LetterSpacing::Px0,
            line_spacing: LineSpacing::Px0,
            paragraph_spacing: ParagraphSpacing::Lines0,
            margin_top: PageMargin::Px0,
            margin_bottom: PageMargin::Px0,
            margin_left: PageMargin::Px0,
            margin_right: PageMargin::Px0,
            first_line_indent: false,
            justified: true,
            paragraph_alignment: ParagraphAlignment::Justified,
            chinese_script: crate::reader_hanzi::ChineseScript::Original,
            dark_mode: false,
            full_refresh: FullRefreshEvery::Off,
            show_progress: true,
            status_page: true,
            status_chapter: true,
            status_time: false,
            status_battery: false,
            swap_page_keys: false,
            long_press_chapter: false,
            auto_page_turn: AutoPageTurn::Off,
            immersive: false,
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

    /// Alignment actually used while drawing. The justified toggle wins.
    #[must_use]
    pub const fn effective_alignment(self) -> ParagraphAlignment {
        if self.justified {
            ParagraphAlignment::Justified
        } else if matches!(self.paragraph_alignment, ParagraphAlignment::Justified) {
            ParagraphAlignment::Left
        } else {
            self.paragraph_alignment
        }
    }

    /// Display-time script conversion. Page byte offsets are not changed.
    #[must_use]
    pub fn display_line(self, text: &str) -> String {
        crate::reader_hanzi::convert(text, self.chinese_script)
    }

    #[must_use]
    pub fn matching_preset(self) -> Option<ReadingPreset> {
        for preset in [
            ReadingPreset::Compact,
            ReadingPreset::Comfortable,
            ReadingPreset::LargePrint,
        ] {
            if self.matches_preset(preset) {
                return Some(preset);
            }
        }
        None
    }

    fn matches_preset(self, preset: ReadingPreset) -> bool {
        let sample = Self {
            theme: self.theme,
            orientation: self.orientation,
            book_font: self.book_font,
            chinese_script: self.chinese_script,
            dark_mode: self.dark_mode,
            full_refresh: self.full_refresh,
            show_progress: self.show_progress,
            status_page: self.status_page,
            status_chapter: self.status_chapter,
            status_time: self.status_time,
            status_battery: self.status_battery,
            swap_page_keys: self.swap_page_keys,
            long_press_chapter: self.long_press_chapter,
            auto_page_turn: self.auto_page_turn,
            sd_cjk_file: self.sd_cjk_file,
            ..Self::preset_values(preset)
        };
        self == sample
    }

    fn preset_values(preset: ReadingPreset) -> Self {
        let mut prefs = Self::default();
        match preset {
            ReadingPreset::Compact => {
                prefs.font_size = BookFontSize::Px20;
                prefs.letter_spacing = LetterSpacing::Px0;
                prefs.line_spacing = LineSpacing::Px0;
                prefs.paragraph_spacing = ParagraphSpacing::Lines0;
                prefs.margin_top = PageMargin::Px0;
                prefs.margin_bottom = PageMargin::Px0;
                prefs.margin_left = PageMargin::Px0;
                prefs.margin_right = PageMargin::Px0;
                prefs.first_line_indent = false;
                prefs.justified = true;
                prefs.paragraph_alignment = ParagraphAlignment::Justified;
                prefs.immersive = true;
            }
            ReadingPreset::Comfortable => {
                prefs.font_size = BookFontSize::Px24;
                prefs.letter_spacing = LetterSpacing::Px1;
                prefs.line_spacing = LineSpacing::Px4;
                prefs.paragraph_spacing = ParagraphSpacing::Lines1;
                prefs.margin_top = PageMargin::Px8;
                prefs.margin_bottom = PageMargin::Px8;
                prefs.margin_left = PageMargin::Px8;
                prefs.margin_right = PageMargin::Px8;
                prefs.first_line_indent = true;
                prefs.justified = true;
                prefs.paragraph_alignment = ParagraphAlignment::Justified;
            }
            ReadingPreset::LargePrint => {
                prefs.font_size = BookFontSize::Px48;
                prefs.letter_spacing = LetterSpacing::Px2;
                prefs.line_spacing = LineSpacing::Px12;
                prefs.paragraph_spacing = ParagraphSpacing::Lines2;
                prefs.margin_top = PageMargin::Px16;
                prefs.margin_bottom = PageMargin::Px16;
                prefs.margin_left = PageMargin::Px16;
                prefs.margin_right = PageMargin::Px16;
                prefs.first_line_indent = false;
                prefs.justified = false;
                prefs.paragraph_alignment = ParagraphAlignment::Left;
            }
        }
        prefs
    }

    pub fn apply_preset(&mut self, preset: ReadingPreset) {
        let kept = *self;
        *self = Self {
            theme: kept.theme,
            orientation: kept.orientation,
            book_font: kept.book_font,
            chinese_script: kept.chinese_script,
            dark_mode: kept.dark_mode,
            full_refresh: kept.full_refresh,
            show_progress: kept.show_progress,
            status_page: kept.status_page,
            status_chapter: kept.status_chapter,
            status_time: kept.status_time,
            status_battery: kept.status_battery,
            swap_page_keys: kept.swap_page_keys,
            long_press_chapter: kept.long_press_chapter,
            auto_page_turn: kept.auto_page_turn,
            sd_cjk_file: kept.sd_cjk_file,
            ..Self::preset_values(preset)
        };
    }

    pub fn cycle_preset(&mut self) -> ReadingPreset {
        let next = match self.matching_preset() {
            Some(ReadingPreset::Compact) => ReadingPreset::Comfortable,
            Some(ReadingPreset::Comfortable) => ReadingPreset::LargePrint,
            Some(ReadingPreset::LargePrint) | None => ReadingPreset::Compact,
        };
        self.apply_preset(next);
        next
    }

    #[must_use]
    pub fn layout(self) -> ReaderLayout {
        let px = self.font_size.pixels();
        let (
            base_width,
            base_height,
            margin_left_px,
            margin_right_px,
            margin_top_px,
            margin_bottom_px,
        ) = if self.immersive {
            let (width, height) = match self.orientation {
                ReaderOrientation::Portrait => (480, 800),
                ReaderOrientation::Landscape => (800, 480),
            };
            (width, height, IMMERSIVE_SIDE_PX, IMMERSIVE_SIDE_PX, 0, 0)
        } else {
            let (width, height) = match self.orientation {
                ReaderOrientation::Portrait => (432, 594),
                ReaderOrientation::Landscape => (752, 300),
            };
            (
                width,
                height,
                self.margin_left.pixels(),
                self.margin_right.pixels(),
                self.margin_top.pixels(),
                self.margin_bottom.pixels(),
            )
        };
        let width_px =
            (base_width - i32::from(margin_left_px) - i32::from(margin_right_px)).max(80);
        let height_px =
            (base_height - i32::from(margin_top_px) - i32::from(margin_bottom_px)).max(80);
        let line_spacing_px = self.line_spacing.pixels();
        let line_step = i32::from(px) + i32::from(px) / 4 + 2 + i32::from(line_spacing_px);
        let lines_per_page = (height_px / line_step).max(4) as usize;
        let ascii_advance_px = match self.book_font {
            BookFont::Serif | BookFont::Literata => (i32::from(px) * 10 / 20).max(6),
            BookFont::CjkUnifont | BookFont::SdCjk => (i32::from(px) / 2).max(6),
            _ => (i32::from(px) * 11 / 20).max(6),
        };
        let letter_spacing_px = self.letter_spacing.pixels();
        let tracked_ascii = ascii_advance_px
            .saturating_add(i32::from(letter_spacing_px))
            .max(1);
        let chars_per_line = (width_px / tracked_ascii).max(8) as usize;
        ReaderLayout {
            chars_per_line,
            lines_per_page,
            max_line_width_px: width_px,
            ascii_advance_px,
            font_size_px: px,
            letter_spacing_px,
            line_spacing_px,
            paragraph_gap_lines: self.paragraph_spacing.lines(),
            margin_top_px,
            margin_bottom_px,
            margin_left_px,
            margin_right_px,
            first_line_indent: self.first_line_indent,
            justified: self.justified,
            orientation: self.orientation,
            font_size: self.font_size,
            book_font: self.book_font,
            letter_spacing: self.letter_spacing,
            line_spacing: self.line_spacing,
            paragraph_spacing: self.paragraph_spacing,
            paragraph_alignment: self.paragraph_alignment,
            chinese_script: self.chinese_script,
            immersive: self.immersive,
            sd_cjk_file: self.sd_cjk_file,
        }
    }

    /// Full-refresh interval actually used. Dark mode still refreshes when the
    /// user cadence is off, so white-on-black text does not ghost.
    #[must_use]
    pub const fn effective_full_refresh_turns(self) -> Option<u8> {
        if let Some(turns) = self.full_refresh.turns() {
            return Some(turns);
        }
        if self.dark_mode {
            Some(DARK_MODE_FULL_REFRESH_TURNS)
        } else {
            None
        }
    }

    #[must_use]
    pub fn serialized(self) -> String {
        format!(
            "version={}\ntheme={}\norientation={}\nfont_size={}\nbook_font={}\nletter_spacing={}\nline_spacing={}\nparagraph_spacing={}\nmargin_top={}\nmargin_bottom={}\nmargin_left={}\nmargin_right={}\nfirst_line_indent={}\njustified={}\nparagraph_alignment={}\nchinese={}\nimmersive={}\ndark_mode={}\nfull_refresh={}\nshow_progress={}\nstatus_page={}\nstatus_chapter={}\nstatus_time={}\nstatus_battery={}\nswap_page_keys={}\nlong_press_chapter={}\nauto_page_turn={}\n",
            READER_PREFS_VERSION,
            self.theme.marker(),
            self.orientation.marker(),
            self.font_size.marker(),
            self.nvs_face_marker(),
            self.letter_spacing.marker(),
            self.line_spacing.marker(),
            self.paragraph_spacing.marker(),
            self.margin_top.marker(),
            self.margin_bottom.marker(),
            self.margin_left.marker(),
            self.margin_right.marker(),
            bool_marker(self.first_line_indent),
            bool_marker(self.justified),
            self.paragraph_alignment.marker(),
            self.chinese_script.marker(),
            bool_marker(self.immersive),
            bool_marker(self.dark_mode),
            self.full_refresh.marker(),
            bool_marker(self.show_progress),
            bool_marker(self.status_page),
            bool_marker(self.status_chapter),
            bool_marker(self.status_time),
            bool_marker(self.status_battery),
            bool_marker(self.swap_page_keys),
            bool_marker(self.long_press_chapter),
            self.auto_page_turn.marker(),
        )
    }

    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut prefs = Self::default();
        let mut version = None;
        let mut saw_status_page = false;
        let mut saw_status_chapter = false;
        let mut saw_justified = false;
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
                "letter_spacing" => prefs.letter_spacing = LetterSpacing::parse(value)?,
                "line_spacing" => prefs.line_spacing = LineSpacing::parse(value)?,
                "paragraph_spacing" => prefs.paragraph_spacing = ParagraphSpacing::parse(value)?,
                "margin_top" => prefs.margin_top = PageMargin::parse(value)?,
                "margin_bottom" => prefs.margin_bottom = PageMargin::parse(value)?,
                "margin_left" => prefs.margin_left = PageMargin::parse(value)?,
                "margin_right" => prefs.margin_right = PageMargin::parse(value)?,
                "first_line_indent" => {
                    prefs.first_line_indent = parse_bool("first_line_indent", value)?
                }
                "justified" => {
                    prefs.justified = parse_bool("justified", value)?;
                    saw_justified = true;
                }
                "paragraph_alignment" => {
                    prefs.paragraph_alignment = ParagraphAlignment::parse(value)?
                }
                "chinese" => {
                    prefs.chinese_script = crate::reader_hanzi::ChineseScript::parse(value)?
                }
                "immersive" => prefs.immersive = parse_bool("immersive", value)?,
                "dark_mode" => prefs.dark_mode = parse_bool("dark_mode", value)?,
                "full_refresh" => prefs.full_refresh = FullRefreshEvery::parse(value)?,
                "show_progress" => {
                    prefs.show_progress = parse_bool("show_progress", value)?;
                }
                "status_page" => {
                    prefs.status_page = parse_bool("status_page", value)?;
                    saw_status_page = true;
                }
                "status_chapter" => {
                    prefs.status_chapter = parse_bool("status_chapter", value)?;
                    saw_status_chapter = true;
                }
                "status_time" => prefs.status_time = parse_bool("status_time", value)?,
                "status_battery" => prefs.status_battery = parse_bool("status_battery", value)?,
                "swap_page_keys" => prefs.swap_page_keys = parse_bool("swap_page_keys", value)?,
                "long_press_chapter" => {
                    prefs.long_press_chapter = parse_bool("long_press_chapter", value)?
                }
                "auto_page_turn" => prefs.auto_page_turn = AutoPageTurn::parse(value)?,
                other => {
                    log::info!("rustmix-wave=reader-prefs status=ignored-key key={other}");
                }
            }
        }
        match version.as_deref() {
            Some(READER_PREFS_VERSION) | Some(READER_PREFS_VERSION_V1) => {}
            _ => return Err("unsupported Reader preference version".into()),
        }
        if !saw_status_page {
            prefs.status_page = prefs.show_progress;
        }
        if !saw_status_chapter {
            prefs.status_chapter = prefs.show_progress;
        }
        prefs.show_progress = prefs.status_page;
        if !saw_justified {
            prefs.justified = prefs.paragraph_alignment == ParagraphAlignment::Justified;
        } else if prefs.justified {
            prefs.paragraph_alignment = ParagraphAlignment::Justified;
        } else if prefs.paragraph_alignment == ParagraphAlignment::Justified {
            prefs.paragraph_alignment = ParagraphAlignment::Left;
        }
        Ok(prefs)
    }
}

/// Swap UP and DOWN while a book page is on screen.
#[must_use]
pub fn map_page_turn_event(event: ButtonEvent, swap: bool) -> ButtonEvent {
    if !swap {
        return event;
    }
    match event {
        ButtonEvent::Up => ButtonEvent::Down,
        ButtonEvent::Down => ButtonEvent::Up,
        other => other,
    }
}

/// Long-press SELECT on an immersive page reveals progress without opening a menu.
#[must_use]
pub fn show_immersive_status(event: ButtonEvent, held_ms: u32, immersive: bool) -> bool {
    immersive && event == ButtonEvent::Select && held_ms >= crate::buttons::KEY_LONG_PRESS_MS
}

/// The status overlay is still on screen for this many milliseconds after it appeared.
#[must_use]
pub const fn immersive_status_visible(elapsed_ms: u64) -> bool {
    elapsed_ms < IMMERSIVE_STATUS_MS
}

/// Long-press direction for a chapter jump. `Some(true)` moves forward.
#[must_use]
pub fn chapter_jump_forward(
    event: ButtonEvent,
    held_ms: u32,
    enabled: bool,
    swap: bool,
) -> Option<bool> {
    if !enabled || held_ms < crate::buttons::KEY_LONG_PRESS_MS {
        return None;
    }
    match event {
        ButtonEvent::Down => Some(!swap),
        ButtonEvent::Up => Some(swap),
        ButtonEvent::Select => None,
    }
}

/// True when the auto-turn interval has elapsed and a key has not paused it.
#[must_use]
pub fn poll_auto_page_turn(
    now_ms: u64,
    since_ms: u64,
    interval: AutoPageTurn,
    paused: bool,
) -> bool {
    if paused {
        return false;
    }
    let Some(period) = interval.millis() else {
        return false;
    };
    now_ms.saturating_sub(since_ms) >= period
}

/// Armed auto-turn keeps the panel on and blocks deep sleep between turns.
#[must_use]
pub const fn auto_page_turn_keeps_awake(interval: AutoPageTurn, paused: bool) -> bool {
    !paused && !matches!(interval, AutoPageTurn::Off)
}

/// Host-testable auto-turn clock. A key pauses it; setting the interval arms it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutoTurnClock {
    pub since_ms: u64,
    pub paused: bool,
}

impl AutoTurnClock {
    #[must_use]
    pub const fn new(now_ms: u64) -> Self {
        Self {
            since_ms: now_ms,
            paused: false,
        }
    }

    #[must_use]
    pub const fn on_key(mut self) -> Self {
        self.paused = true;
        self
    }

    #[must_use]
    pub const fn on_interval_set(mut self, now_ms: u64) -> Self {
        self.paused = false;
        self.since_ms = now_ms;
        self
    }

    #[must_use]
    pub const fn after_turn(mut self, now_ms: u64) -> Self {
        self.since_ms = now_ms;
        self
    }

    #[must_use]
    pub fn due(self, now_ms: u64, interval: AutoPageTurn) -> bool {
        poll_auto_page_turn(now_ms, self.since_ms, interval, self.paused)
    }
}

fn bool_marker(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "On"
    } else {
        "Off"
    }
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    match value.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{key} must be true or false")),
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
    /// Seek by byte offset instead of a page index from the previous layout.
    pub anchor_by_offset: bool,
}

/// One wrapped Reader line. `paragraph_end` prevents Justified rendering from
/// stretching the final line of a paragraph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderPageLine {
    pub text: String,
    pub paragraph_end: bool,
    /// First visual line of a paragraph. Drawing indents it by two CJK cells.
    pub first_line_indent: bool,
}

impl ReaderPageLine {
    #[must_use]
    pub fn new(text: impl Into<String>, paragraph_end: bool) -> Self {
        Self {
            text: text.into(),
            paragraph_end,
            first_line_indent: false,
        }
    }
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

/// Page anchors for the one spine item currently being laid out.
#[derive(Clone, Debug, Eq, PartialEq)]
struct EpubChapterLayout {
    index: usize,
    chapter_number: usize,
    chapter_count: usize,
    text_offset: u64,
    text_end_offset: u64,
    pages: Vec<u64>,
    next_offset: u64,
    complete: bool,
    truncated: bool,
    on_sd: bool,
    dirty: bool,
    fingerprint: u64,
    cache_path: PathBuf,
    anchor_limit: usize,
    byte_limit: usize,
    warning: Option<String>,
}

impl EpubChapterLayout {
    fn admits_another_page(&self) -> bool {
        if self.pages.len() >= self.anchor_limit.max(1) {
            return false;
        }
        let pages = (self.pages.len() as u64).saturating_add(1);
        match pages
            .checked_mul(8)
            .and_then(|anchors| anchors.checked_add(crate::epub_page_index::HEADER_LEN as u64))
        {
            Some(bytes) => bytes <= self.byte_limit.max(crate::epub_page_index::HEADER_LEN) as u64,
            None => false,
        }
    }

    fn at_start(&self, offset: u64) -> bool {
        match self.pages.first() {
            Some(&first) => offset <= first,
            None => offset <= self.text_offset,
        }
    }

    fn page_index_of(&self, offset: u64) -> Option<usize> {
        if self.pages.is_empty() {
            return None;
        }
        let index = self
            .pages
            .partition_point(|start| *start <= offset)
            .checked_sub(1)?;
        if self.pages[index] > offset {
            return None;
        }
        if let Some(&next) = self.pages.get(index + 1) {
            if offset < next {
                Some(index)
            } else {
                None
            }
        } else if self.complete || self.truncated || self.next_offset > offset {
            Some(index)
        } else {
            None
        }
    }

    fn next_anchor(&self, offset: u64) -> Option<u64> {
        let current = self.page_index_of(offset)?;
        self.pages.get(current + 1).copied()
    }

    fn previous_anchor(&self, offset: u64) -> Option<u64> {
        if !(self.complete || self.truncated || self.next_offset > offset) {
            return None;
        }
        let current = self
            .pages
            .partition_point(|start| *start <= offset)
            .checked_sub(1)?;
        let previous = current.checked_sub(1)?;
        Some(self.pages[previous])
    }

    fn estimate_pages(&self, offset: u64) -> (usize, usize) {
        let length = self.text_end_offset.saturating_sub(self.text_offset).max(1);
        let into_chapter = offset.saturating_sub(self.text_offset).min(length);
        if self.pages.is_empty() || self.next_offset <= self.text_offset {
            return (1, 0);
        }
        let through = self.next_offset.saturating_sub(self.text_offset).max(1);
        let indexed = self.pages.len() as u64;
        let pages = (length.saturating_mul(indexed) / through)
            .max(indexed)
            .max(1);
        let number = (into_chapter.saturating_mul(indexed) / through)
            .saturating_add(1)
            .min(pages);
        (
            usize::try_from(number).unwrap_or(usize::MAX),
            usize::try_from(pages).unwrap_or(usize::MAX),
        )
    }

    fn label_for(&self, offset: u64) -> ReaderChapterPageLabel {
        if let Some(index) = self.page_index_of(offset) {
            let page_number = index.saturating_add(1);
            if self.complete && !self.truncated {
                return ReaderChapterPageLabel {
                    chapter_number: self.chapter_number,
                    chapter_count: self.chapter_count,
                    page_number,
                    page_count: self.pages.len().max(1),
                    approximate: false,
                };
            }
            if self.truncated {
                let (_, estimated) = self.estimate_pages(offset);
                return ReaderChapterPageLabel {
                    chapter_number: self.chapter_number,
                    chapter_count: self.chapter_count,
                    page_number,
                    page_count: estimated.max(page_number),
                    approximate: true,
                };
            }
            return ReaderChapterPageLabel {
                chapter_number: self.chapter_number,
                chapter_count: self.chapter_count,
                page_number,
                page_count: 0,
                approximate: true,
            };
        }
        let (page_number, page_count) = self.estimate_pages(offset);
        ReaderChapterPageLabel {
            chapter_number: self.chapter_number,
            chapter_count: self.chapter_count,
            page_number,
            page_count: if self.truncated { page_count } else { 0 },
            approximate: true,
        }
    }

    fn store_if_dirty(&mut self) -> Result<(), String> {
        if !self.dirty || (self.pages.is_empty() && !self.complete && !self.truncated) {
            return Ok(());
        }
        let cache = crate::epub_page_index::ChapterAnchorCache {
            fingerprint: self.fingerprint,
            chapter_index: u32::try_from(self.index).unwrap_or(u32::MAX),
            text_offset: self.text_offset,
            text_end_offset: self.text_end_offset,
            anchors: self.pages.clone(),
            next_offset: self.next_offset,
            complete: self.complete,
            truncated: self.truncated,
        };
        match cache.store(&self.cache_path) {
            Ok(()) => {
                self.dirty = false;
                self.on_sd = true;
                Ok(())
            }
            Err(error) => {
                self.warning = Some(error.clone());
                Err(error)
            }
        }
    }
}

fn layout_one_epub_page(
    layout: &mut EpubChapterLayout,
    document: &EpubDocument,
    prefs: ReaderLayout,
) -> Result<bool, String> {
    if layout.complete || layout.truncated {
        return Ok(false);
    }
    if layout.next_offset >= layout.text_end_offset {
        layout.complete = true;
        layout.dirty = true;
        return Ok(false);
    }
    if !layout.admits_another_page() {
        layout.truncated = true;
        layout.dirty = true;
        return Ok(false);
    }
    let page = read_epub_page_until(
        document,
        prefs,
        layout.next_offset,
        layout.pages.len(),
        layout.text_end_offset,
    )?;
    if page.next_byte_offset <= layout.next_offset {
        layout.truncated = true;
        layout.dirty = true;
        return Ok(false);
    }
    let start = page.byte_offset;
    if layout.pages.last().is_some_and(|last| *last >= start)
        || start < layout.text_offset
        || start > layout.text_end_offset
    {
        layout.truncated = true;
        layout.dirty = true;
        return Ok(false);
    }
    layout.pages.push(start);
    layout.next_offset = page.next_byte_offset.min(layout.text_end_offset);
    layout.dirty = true;
    if layout.next_offset >= layout.text_end_offset {
        layout.complete = true;
    }
    Ok(true)
}

fn chapter_cache_fingerprint(
    book: &ReaderBook,
    layout: ReaderLayout,
    chapter_index: usize,
    text_offset: u64,
    text_end_offset: u64,
) -> u64 {
    let mut hash = book_fingerprint(book, layout);
    for value in [
        chapter_index as u64,
        text_offset,
        text_end_offset,
        0x4550_4348,
    ] {
        hash ^= value;
        hash = hash.wrapping_mul(CACHE_FNV_PRIME);
    }
    hash
}

fn clamp_to_chapter(offset: u64, start: u64, end: u64) -> u64 {
    if offset < start {
        start
    } else if offset > end {
        end
    } else {
        offset
    }
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
    epub_chapter: Option<EpubChapterLayout>,
    epub_preparing: Option<EpubChapterLayout>,
    pending_show_last: bool,
    pending_rewind_offset: Option<u64>,
    epub_cache_dir: PathBuf,
    epub_anchor_limit: usize,
    epub_byte_limit: usize,
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
        self.epub_chapter
            .as_ref()
            .map_or(self.page_offsets.len(), |chapter| chapter.pages.len())
    }

    #[must_use]
    pub fn epub_index_on_sd(&self) -> bool {
        self.epub_chapter
            .as_ref()
            .is_some_and(|chapter| chapter.on_sd)
    }

    #[must_use]
    pub fn progress_percent(&self) -> u8 {
        let source_size = self.source_size_bytes();
        if source_size == 0 {
            return 100;
        }
        if self.epub_chapter.is_some() {
            let offset = self.view_offset();
            return ((offset.saturating_mul(100) / source_size).min(100)) as u8;
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
        self.page_number_base
            .saturating_add(self.page_offsets.len())
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

    /// Product-facing page label. TXT keeps the book-relative label. EPUB shows
    /// the chapter as `i/N` and the page within that chapter.
    #[must_use]
    pub fn display_page_label(&self) -> String {
        self.current_epub_chapter_page_label().map_or_else(
            || format!("PAGE {}", self.page_label()),
            |chapter| {
                format!(
                    "CH {}  PAGE {}",
                    chapter.chapter_text(),
                    chapter.page_text()
                )
            },
        )
    }

    #[must_use]
    fn view_offset(&self) -> u64 {
        self.page_offsets
            .get(self.current_page)
            .copied()
            .or_else(|| self.current_cached_page().map(|page| page.byte_offset))
            .unwrap_or(0)
    }

    #[must_use]
    pub fn current_epub_chapter_page_label(&self) -> Option<ReaderChapterPageLabel> {
        if self.epub_chapter.is_none() {
            return None;
        }
        let offset = self.view_offset();
        self.epub_chapter_page_label_for_offset(offset).or_else(|| {
            self.epub_chapter
                .as_ref()
                .map(|layout| layout.label_for(offset))
        })
    }

    #[must_use]
    pub fn epub_chapter_page_label_for_offset(
        &self,
        offset: u64,
    ) -> Option<ReaderChapterPageLabel> {
        if self.book.format != BookFormat::Epub {
            return None;
        }
        let layout = self.epub_chapter.as_ref()?;
        let source_size = self.source_size_bytes();
        let in_chapter = offset >= layout.text_offset
            && (offset < layout.text_end_offset
                || (offset == layout.text_end_offset && layout.text_end_offset == source_size));
        if !in_chapter {
            return None;
        }
        Some(layout.label_for(offset))
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
        if self.epub_chapter.is_some() {
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
        if self.epub_chapter.is_some() {
            return self.previous_epub_page();
        }
        if self.current_page > 0 {
            self.current_page -= 1;
            self.ensure_page_cached(self.current_page)?;
        }
        Ok(())
    }

    fn next_epub_page(&mut self) -> Result<(), String> {
        self.cancel_chapter_prepare();
        self.pending_rewind_offset = None;
        let offset = self.view_offset();
        if let Some(next) = self
            .epub_chapter
            .as_ref()
            .and_then(|layout| layout.next_anchor(offset))
        {
            return self.render_epub_offset(next);
        }
        let chapter_end = self
            .epub_chapter
            .as_ref()
            .map(|layout| layout.text_end_offset)
            .unwrap_or(0);
        let next_byte = self.current_page_end_offset()?;
        if next_byte > offset && next_byte < chapter_end {
            return self.render_epub_offset(next_byte);
        }
        self.enter_next_chapter()
    }

    fn previous_epub_page(&mut self) -> Result<(), String> {
        let offset = self.view_offset();
        let at_start = self
            .epub_chapter
            .as_ref()
            .is_some_and(|layout| layout.at_start(offset));
        if at_start {
            self.pending_rewind_offset = None;
            return self.prepare_previous_chapter();
        }
        self.cancel_chapter_prepare();
        if let Some(previous) = self
            .epub_chapter
            .as_ref()
            .and_then(|layout| layout.previous_anchor(offset))
        {
            self.pending_rewind_offset = None;
            return self.render_epub_offset(previous);
        }
        self.pending_rewind_offset = Some(offset);
        self.layout_epub_batch(READER_EPUB_INDEX_YIELD_EVERY_PAGES)?;
        self.finish_pending_rewind().map(|_| ())
    }

    fn enter_next_chapter(&mut self) -> Result<(), String> {
        let Some(current) = self.epub_chapter.as_ref() else {
            return Ok(());
        };
        let next_index = current.index.saturating_add(1);
        let count = self
            .epub_document
            .as_ref()
            .map(|document| document.chapters.len())
            .unwrap_or(0);
        if next_index >= count {
            return Ok(());
        }
        let start = self
            .epub_document
            .as_ref()
            .and_then(|document| document.chapters.get(next_index))
            .map(|chapter| chapter.text_offset)
            .unwrap_or(0);
        self.jump_to_chapter(next_index, start)
    }

    fn prepare_previous_chapter(&mut self) -> Result<(), String> {
        let Some(current) = self.epub_chapter.as_ref() else {
            return Ok(());
        };
        if current.index == 0 {
            return Ok(());
        }
        let prepared = self.load_chapter_layout(current.index - 1)?;
        if prepared.complete || prepared.truncated {
            let offset = prepared
                .pages
                .last()
                .copied()
                .unwrap_or(prepared.text_offset);
            self.store_epub_caches()?;
            self.epub_preparing = None;
            self.pending_show_last = false;
            self.epub_chapter = Some(prepared);
            return self.render_epub_offset(offset);
        }
        self.epub_preparing = Some(prepared);
        self.pending_show_last = true;
        self.layout_epub_batch(READER_EPUB_BACKGROUND_INDEX_PAGES)?;
        self.finish_pending_last_page().map(|_| ())
    }

    fn cancel_chapter_prepare(&mut self) {
        if let Some(layout) = self.epub_preparing.as_mut() {
            let _ = layout.store_if_dirty();
        }
        self.epub_preparing = None;
        self.pending_show_last = false;
    }

    fn jump_to_chapter(&mut self, index: usize, offset: u64) -> Result<(), String> {
        self.store_epub_caches()?;
        self.epub_preparing = None;
        self.pending_show_last = false;
        self.pending_rewind_offset = None;
        let layout = self.load_chapter_layout(index)?;
        let offset = clamp_to_chapter(offset, layout.text_offset, layout.text_end_offset);
        self.epub_chapter = Some(layout);
        self.render_epub_offset(offset)
    }

    fn load_chapter_layout(&self, index: usize) -> Result<EpubChapterLayout, String> {
        let document = self
            .epub_document
            .as_ref()
            .ok_or_else(|| "EPUB document is unavailable".to_string())?;
        let chapter = document
            .chapters
            .get(index)
            .ok_or_else(|| "EPUB chapter is unavailable".to_string())?;
        let fingerprint = chapter_cache_fingerprint(
            &self.book,
            self.layout,
            index,
            chapter.text_offset,
            chapter.text_end_offset,
        );
        let cache_path = crate::epub_page_index::cache_path(&self.epub_cache_dir, fingerprint);
        let loaded = crate::epub_page_index::ChapterAnchorCache::load(
            &cache_path,
            &crate::epub_page_index::ChapterCacheExpect {
                fingerprint,
                chapter_index: u32::try_from(index).unwrap_or(u32::MAX),
                text_offset: chapter.text_offset,
                text_end_offset: chapter.text_end_offset,
            },
        );
        Ok(EpubChapterLayout {
            index,
            chapter_number: chapter.number,
            chapter_count: document.chapters.len().max(chapter.number),
            text_offset: chapter.text_offset,
            text_end_offset: chapter.text_end_offset,
            pages: loaded
                .as_ref()
                .map(|cache| cache.anchors.clone())
                .unwrap_or_default(),
            next_offset: loaded
                .as_ref()
                .map(|cache| cache.next_offset)
                .unwrap_or(chapter.text_offset),
            complete: loaded.as_ref().is_some_and(|cache| cache.complete),
            truncated: loaded.as_ref().is_some_and(|cache| cache.truncated),
            on_sd: loaded.is_some(),
            dirty: false,
            fingerprint,
            cache_path,
            anchor_limit: self.epub_anchor_limit,
            byte_limit: self.epub_byte_limit,
            warning: None,
        })
    }

    fn render_epub_offset(&mut self, offset: u64) -> Result<(), String> {
        let prefs = self.layout;
        let mut page = {
            let document = self
                .epub_document
                .as_ref()
                .ok_or_else(|| "EPUB document is unavailable".to_string())?;
            read_epub_page(document, prefs, offset, 0)?
        };
        let local = self
            .epub_chapter
            .as_ref()
            .and_then(|layout| layout.page_index_of(page.byte_offset));
        self.page_number_base = local.unwrap_or(0);
        self.current_page = 0;
        self.page_offsets = vec![page.byte_offset];
        self.epub_page_numbers_exact = local.is_some()
            && self
                .epub_chapter
                .as_ref()
                .is_some_and(|layout| layout.complete && !layout.truncated);
        page.page_index = self.current_absolute_page();
        self.cache.clear();
        self.push_cached_page(page);
        self.sync_epub_flags();
        Ok(())
    }

    fn layout_epub_batch(&mut self, budget: usize) -> Result<usize, String> {
        let preparing = self.epub_preparing.is_some();
        let Some(mut layout) = (if preparing {
            self.epub_preparing.take()
        } else {
            self.epub_chapter.take()
        }) else {
            return Ok(0);
        };
        let prefs = self.layout;
        let mut added = 0_usize;
        let result = (|| {
            while added < budget {
                if added > 0 && added % READER_EPUB_INDEX_YIELD_EVERY_PAGES == 0 {
                    std::thread::sleep(Duration::from_millis(READER_EPUB_INDEX_YIELD_MILLIS));
                    if layout_button_pending() {
                        break;
                    }
                }
                let document = self
                    .epub_document
                    .as_ref()
                    .ok_or_else(|| "EPUB document is unavailable".to_string())?;
                if !layout_one_epub_page(&mut layout, document, prefs)? {
                    break;
                }
                added += 1;
            }
            Ok(added)
        })();
        if preparing {
            self.epub_preparing = Some(layout);
        } else {
            self.epub_chapter = Some(layout);
        }
        self.sync_epub_flags();
        result
    }

    fn sync_epub_flags(&mut self) {
        let Some(layout) = self.epub_chapter.as_ref() else {
            return;
        };
        self.indexed_through = layout.next_offset;
        self.index_complete = layout.complete;
        self.index_truncated = layout.truncated;
        self.epub_chapter_pages = vec![ReaderEpubChapterPages {
            chapter_number: layout.chapter_number,
            text_offset: layout.text_offset,
            text_end_offset: layout.text_end_offset,
            first_page: if layout.pages.is_empty() {
                None
            } else {
                Some(0)
            },
            indexed_pages: layout.pages.len(),
            complete: layout.complete,
        }];
    }

    fn reconcile_view_page_number(&mut self) {
        let offset = self.view_offset();
        if let Some(index) = self
            .epub_chapter
            .as_ref()
            .and_then(|layout| layout.page_index_of(offset))
        {
            self.page_number_base = index;
            self.epub_page_numbers_exact = self
                .epub_chapter
                .as_ref()
                .is_some_and(|layout| layout.complete && !layout.truncated);
            if let Some(page) = self
                .cache
                .iter_mut()
                .find(|page| page.byte_offset == offset)
            {
                page.page_index = index;
            }
        }
        self.sync_epub_flags();
    }

    fn finish_pending_last_page(&mut self) -> Result<bool, String> {
        let ready = self
            .epub_preparing
            .as_ref()
            .is_some_and(|layout| layout.complete || layout.truncated);
        if !self.pending_show_last || !ready {
            return Ok(false);
        }
        if let Some(layout) = self.epub_preparing.as_mut() {
            let _ = layout.store_if_dirty();
        }
        let prepared = self
            .epub_preparing
            .take()
            .ok_or_else(|| "EPUB chapter prepare disappeared".to_string())?;
        let offset = prepared
            .pages
            .last()
            .copied()
            .unwrap_or(prepared.text_offset);
        if let Some(current) = self.epub_chapter.as_mut() {
            let _ = current.store_if_dirty();
        }
        self.epub_chapter = Some(prepared);
        self.pending_show_last = false;
        self.render_epub_offset(offset)?;
        Ok(true)
    }

    fn finish_pending_rewind(&mut self) -> Result<bool, String> {
        let Some(offset) = self.pending_rewind_offset else {
            return Ok(false);
        };
        if let Some(previous) = self
            .epub_chapter
            .as_ref()
            .and_then(|layout| layout.previous_anchor(offset))
        {
            self.pending_rewind_offset = None;
            self.render_epub_offset(previous)?;
            return Ok(true);
        }
        let stalled = self
            .epub_chapter
            .as_ref()
            .is_some_and(|layout| layout.complete || layout.truncated);
        if stalled {
            self.pending_rewind_offset = None;
        }
        Ok(false)
    }

    /// Returns true when the visible page changed because a deferred chapter
    /// edge finished.
    fn resolve_epub_edge(&mut self) -> Result<bool, String> {
        if self.finish_pending_last_page()? {
            return Ok(true);
        }
        self.finish_pending_rewind()
    }

    fn store_epub_caches(&mut self) -> Result<(), String> {
        let mut error = None;
        if let Some(layout) = self.epub_chapter.as_mut() {
            if let Err(err) = layout.store_if_dirty() {
                error = Some(err);
            }
        }
        if let Some(layout) = self.epub_preparing.as_mut() {
            if let Err(err) = layout.store_if_dirty() {
                error = Some(err);
            }
        }
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
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

/// Which Reading Preferences list is on screen. BOOT backs up one level.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PreferenceMenu {
    #[default]
    Root,
    Typography,
    Page,
    Display,
    Status,
    Controls,
}

/// Reading Preferences rows. UP/DOWN moves, SELECT changes a value or opens
/// a submenu. Layout changes stay on this menu until Done or BOOT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadingPreference {
    Presets,
    TypographyMenu,
    PageMenu,
    ChineseScript,
    DisplayMenu,
    StatusMenu,
    ControlsMenu,
    BookFontSize,
    BookFont,
    LetterSpacing,
    LineSpacing,
    ParagraphSpacing,
    FirstLineIndent,
    Justified,
    MarginTop,
    MarginBottom,
    MarginLeft,
    MarginRight,
    Orientation,
    ParagraphAlignment,
    ReadingTheme,
    Immersive,
    DarkMode,
    FullRefresh,
    StatusPage,
    StatusChapter,
    StatusTime,
    StatusBattery,
    SwapPageKeys,
    LongPressChapter,
    AutoPageTurn,
    Done,
}

impl ReadingPreference {
    const ROOT: [Self; 8] = [
        Self::Presets,
        Self::TypographyMenu,
        Self::PageMenu,
        Self::ChineseScript,
        Self::DisplayMenu,
        Self::StatusMenu,
        Self::ControlsMenu,
        Self::Done,
    ];
    const TYPOGRAPHY: [Self; 7] = [
        Self::BookFontSize,
        Self::BookFont,
        Self::LetterSpacing,
        Self::LineSpacing,
        Self::ParagraphSpacing,
        Self::FirstLineIndent,
        Self::Justified,
    ];
    const PAGE: [Self; 6] = [
        Self::MarginTop,
        Self::MarginBottom,
        Self::MarginLeft,
        Self::MarginRight,
        Self::Orientation,
        Self::ParagraphAlignment,
    ];
    const DISPLAY: [Self; 4] = [
        Self::Immersive,
        Self::ReadingTheme,
        Self::DarkMode,
        Self::FullRefresh,
    ];
    const STATUS: [Self; 4] = [
        Self::StatusPage,
        Self::StatusChapter,
        Self::StatusTime,
        Self::StatusBattery,
    ];
    const CONTROLS: [Self; 3] = [
        Self::SwapPageKeys,
        Self::LongPressChapter,
        Self::AutoPageTurn,
    ];

    #[must_use]
    pub const fn rows(menu: PreferenceMenu) -> &'static [Self] {
        match menu {
            PreferenceMenu::Root => &Self::ROOT,
            PreferenceMenu::Typography => &Self::TYPOGRAPHY,
            PreferenceMenu::Page => &Self::PAGE,
            PreferenceMenu::Display => &Self::DISPLAY,
            PreferenceMenu::Status => &Self::STATUS,
            PreferenceMenu::Controls => &Self::CONTROLS,
        }
    }

    #[must_use]
    pub const fn submenu(self) -> Option<PreferenceMenu> {
        match self {
            Self::TypographyMenu => Some(PreferenceMenu::Typography),
            Self::PageMenu => Some(PreferenceMenu::Page),
            Self::DisplayMenu => Some(PreferenceMenu::Display),
            Self::StatusMenu => Some(PreferenceMenu::Status),
            Self::ControlsMenu => Some(PreferenceMenu::Controls),
            _ => None,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Presets => "Presets",
            Self::TypographyMenu => "Typography",
            Self::PageMenu => "Page",
            Self::ChineseScript => "Chinese",
            Self::DisplayMenu => "Display",
            Self::StatusMenu => "Status Bar",
            Self::ControlsMenu => "Controls",
            Self::ReadingTheme => "Reading Theme",
            Self::Immersive => "Immersive",
            Self::Orientation => "Orientation",
            Self::BookFontSize => "Book Font Size",
            Self::BookFont => "Book Font",
            Self::LetterSpacing => "Letter Spacing",
            Self::LineSpacing => "Line Spacing",
            Self::ParagraphSpacing => "Paragraph Spacing",
            Self::FirstLineIndent => "First-line Indent",
            Self::Justified => "Justified",
            Self::MarginTop => "Top Margin",
            Self::MarginBottom => "Bottom Margin",
            Self::MarginLeft => "Left Margin",
            Self::MarginRight => "Right Margin",
            Self::ParagraphAlignment => "Paragraph Alignment",
            Self::DarkMode => "Dark Mode",
            Self::FullRefresh => "Full Refresh",
            Self::StatusPage => "Page Number",
            Self::StatusChapter => "Chapter Progress",
            Self::StatusTime => "Time",
            Self::StatusBattery => "Battery",
            Self::SwapPageKeys => "Swap Page Keys",
            Self::LongPressChapter => "Chapter Jump",
            Self::AutoPageTurn => "Auto Page Turn",
            Self::Done => "Done",
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
    /// A deferred chapter boundary finished and the visible page changed.
    ReadingPositionChanged,
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
    pub preference_menu: PreferenceMenu,
    preferences_layout_dirty: bool,
    pub last_message: Option<String>,
    persistence_event: Option<String>,
    last_persistence_event: Option<String>,
    clear_ghost_requested: bool,
    turns_since_refresh: u8,
    auto_turn_rearm: bool,
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
            preference_menu: PreferenceMenu::Root,
            preferences_layout_dirty: false,
            last_message: None,
            persistence_event: None,
            last_persistence_event: None,
            clear_ghost_requested: false,
            turns_since_refresh: 0,
            auto_turn_rearm: false,
            sd_cjk_faces: Vec::new(),
            epub_page_anchor_limit: READER_EPUB_PAGE_ANCHOR_LIMIT,
            epub_index_bytes_limit: READER_EPUB_ANCHOR_INDEX_BYTES_LIMIT,
        }
    }
}

/// Page window that contains `offset`. A layout cache for the new metrics is
/// searched by byte offset; a missing cache opens one page at that offset.
fn page_window_for_offset(
    cached: Option<&ReaderAnchorCache>,
    offset: u64,
) -> Option<(usize, Vec<u64>, usize, u64, bool)> {
    let cache = cached?;
    if !cache.complete && offset >= cache.indexed_through {
        return None;
    }
    let index = cache.offsets.iter().rposition(|start| *start <= offset)?;
    Some((
        cache.base_page,
        cache.offsets.clone(),
        index,
        cache.indexed_through,
        cache.complete,
    ))
}

/// Page number for a relayout that starts mid-book without a matching cache.
///
/// Uses the first rebuilt page's byte length so the status bar does not show
/// page 1 while the new index is still empty.
fn estimated_page_for_offset(offset: u64, first_page: &ReaderCachedPage) -> usize {
    let page_bytes = first_page
        .next_byte_offset
        .saturating_sub(first_page.byte_offset)
        .max(1);
    usize::try_from(offset / page_bytes).unwrap_or(usize::MAX)
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
        self.epub_index_bytes_limit = bytes.max(crate::epub_page_index::HEADER_LEN + 8);
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
            anchor_by_offset: false,
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
            anchor_by_offset: true,
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

    /// True while a book is opening, the current EPUB chapter is still being
    /// laid out, or a TXT nearby-page cache is incomplete.
    #[must_use]
    pub fn needs_background_tick(&self) -> bool {
        if self.loading.is_some() {
            return true;
        }
        self.session.as_ref().is_some_and(|session| {
            session.epub_preparing.is_some()
                || session.pending_show_last
                || session.pending_rewind_offset.is_some()
                || session
                    .epub_chapter
                    .as_ref()
                    .is_some_and(|chapter| !chapter.complete && !chapter.truncated)
                || (session.epub_chapter.is_none()
                    && session.cache.len() < READER_NEARBY_PAGE_CACHE
                    && !session.index_complete)
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
                        BookFormat::Text => self.open_txt_session(
                            &loading.book,
                            encoding,
                            loading.resume.as_ref(),
                            loading.anchor_by_offset,
                        ),
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
                            self.last_message = Some(
                                "Saved position ready; this chapter lays out on demand".into(),
                            );
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
            if session.epub_chapter.is_some() {
                match session.layout_epub_batch(READER_EPUB_BACKGROUND_INDEX_PAGES) {
                    Ok(added) => {
                        let moved = match session.resolve_epub_edge() {
                            Ok(moved) => moved,
                            Err(error) => {
                                if let Some(layout) = session.epub_chapter.as_mut() {
                                    layout.truncated = true;
                                    layout.warning = Some(error.clone());
                                }
                                session.sync_epub_flags();
                                self.last_message = Some(error);
                                false
                            }
                        };
                        session.reconcile_view_page_number();
                        let checkpoint = session
                            .epub_chapter
                            .as_ref()
                            .is_some_and(|layout| layout.dirty)
                            || session
                                .epub_preparing
                                .as_ref()
                                .is_some_and(|layout| layout.dirty);
                        let outcome = if moved {
                            ReaderTickOutcome::ReadingPositionChanged
                        } else if added > 0 {
                            ReaderTickOutcome::BackgroundCacheAdvanced
                        } else {
                            ReaderTickOutcome::None
                        };
                        (outcome, checkpoint)
                    }
                    Err(error) => {
                        if let Some(layout) = session.epub_chapter.as_mut() {
                            layout.truncated = true;
                            layout.warning = Some(error.clone());
                        }
                        session.sync_epub_flags();
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
            self.note_page_turn();
        }
    }

    pub fn next_page(&mut self) {
        if let Some(session) = self.session.as_mut() {
            if let Err(error) = session.next_page() {
                self.last_message = Some(error);
                return;
            }
            self.persist_current_session_best_effort();
            self.note_page_turn();
        }
    }

    /// Count a successful page turn and request a full refresh every N turns.
    pub fn note_page_turn(&mut self) {
        let Some(every) = self.preferences.effective_full_refresh_turns() else {
            self.turns_since_refresh = 0;
            return;
        };
        self.turns_since_refresh = self.turns_since_refresh.saturating_add(1);
        if self.turns_since_refresh >= every {
            self.turns_since_refresh = 0;
            self.request_clear_ghosting();
        }
    }

    #[must_use]
    pub fn take_auto_turn_rearm(&mut self) -> bool {
        core::mem::take(&mut self.auto_turn_rearm)
    }

    /// Jump one EPUB chapter, or about ten TXT pages when the book has none.
    pub fn jump_chapter(&mut self, forward: bool) {
        let Some(format) = self.session.as_ref().map(|session| session.book.format) else {
            return;
        };
        if format == BookFormat::Epub && self.jump_epub_chapter(forward) {
            self.note_page_turn();
            return;
        }
        for _ in 0..10 {
            if forward {
                self.next_page();
            } else {
                self.previous_page();
            }
        }
    }

    fn jump_epub_chapter(&mut self, forward: bool) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        let Some(document) = session.epub_document.as_ref() else {
            return false;
        };
        if document.chapters.is_empty() {
            return false;
        }
        let offset = session.view_offset();
        let index = session
            .epub_chapter
            .as_ref()
            .map(|layout| layout.index)
            .or_else(|| {
                document.chapters.iter().position(|chapter| {
                    offset >= chapter.text_offset && offset < chapter.text_end_offset
                })
            })
            .unwrap_or(0);
        let target_index = if forward {
            index.saturating_add(1)
        } else if document
            .chapters
            .get(index)
            .is_some_and(|chapter| offset > chapter.text_offset)
        {
            index
        } else {
            match index.checked_sub(1) {
                Some(previous) => previous,
                None => return false,
            }
        };
        let Some(target) = document.chapters.get(target_index) else {
            return false;
        };
        let label = target.label.clone();
        let text_offset = target.text_offset;
        match session.jump_to_chapter(target_index, text_offset) {
            Ok(()) => {
                self.last_message = Some(format!("Chapter: {label}"));
                self.persist_current_session_best_effort();
                true
            }
            Err(error) => {
                self.last_message = Some(error);
                false
            }
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
                if let Some(label) =
                    session.epub_chapter_page_label_for_offset(bookmark.byte_offset)
                {
                    return Some(label.page_number);
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
        let Some(document) = session.epub_document.as_ref() else {
            return false;
        };
        let index = document
            .chapters
            .iter()
            .position(|chapter| {
                entry.text_offset >= chapter.text_offset
                    && entry.text_offset < chapter.text_end_offset
            })
            .or_else(|| {
                document
                    .chapters
                    .iter()
                    .position(|chapter| chapter.spine_index == entry.spine_index)
            })
            .unwrap_or(0);
        match session.jump_to_chapter(index, entry.text_offset) {
            Ok(()) => {
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
        self.preference_menu = PreferenceMenu::Root;
    }

    /// Leave a submenu for the root list. Returns false when already at root.
    pub fn close_preference_submenu(&mut self) -> bool {
        if self.preference_menu == PreferenceMenu::Root {
            return false;
        }
        self.preference_menu = PreferenceMenu::Root;
        self.preferences_selected = 0;
        true
    }

    pub fn cycle_preference_previous(&mut self) {
        let len = self.preference_rows().len();
        self.preferences_selected = self.preferences_selected.checked_sub(1).unwrap_or(len - 1);
    }

    pub fn cycle_preference_next(&mut self) {
        let len = self.preference_rows().len();
        self.preferences_selected = (self.preferences_selected + 1) % len;
    }

    #[must_use]
    pub fn preference_rows(&self) -> &'static [ReadingPreference] {
        ReadingPreference::rows(self.preference_menu)
    }

    #[must_use]
    pub fn selected_preference(&self) -> ReadingPreference {
        let rows = self.preference_rows();
        rows[self.preferences_selected % rows.len()]
    }

    /// Apply one Settings-style SELECT action without leaving the menu.
    ///
    /// Redraw-only settings persist immediately. Layout settings are stored and
    /// marked dirty; the open page is rebuilt when the menu closes.
    #[must_use]
    pub fn activate_selected_preference(&mut self) -> bool {
        self.apply_selected_preference();
        false
    }

    /// Apply the highlighted preference without tearing down an open TXT/EPUB.
    ///
    /// WeRead shares this editor. Pagination waits until the menu closes.
    pub fn activate_shared_preference(&mut self) {
        self.refresh_font_catalog();
        self.apply_selected_preference();
    }

    #[must_use]
    pub const fn layout_changes_pending(&self) -> bool {
        self.preferences_layout_dirty
    }

    fn apply_selected_preference(&mut self) -> bool {
        let row = self.selected_preference();
        if let Some(menu) = row.submenu() {
            self.preference_menu = menu;
            self.preferences_selected = 0;
            return false;
        }
        let layout_sensitive = match row {
            ReadingPreference::Presets => {
                let preset = self.preferences.cycle_preset();
                self.last_message = Some(format!("Preset: {}", preset.label()));
                true
            }
            ReadingPreference::ChineseScript => {
                self.preferences.chinese_script = self.preferences.chinese_script.next();
                self.last_message = Some(format!(
                    "Chinese: {}",
                    self.preferences.chinese_script.label()
                ));
                true
            }
            ReadingPreference::Immersive => {
                self.preferences.immersive = !self.preferences.immersive;
                self.last_message =
                    Some(format!("Immersive: {}", on_off(self.preferences.immersive)));
                true
            }
            ReadingPreference::ReadingTheme => {
                self.preferences.theme = self.preferences.theme.next();
                self.last_message =
                    Some(format!("Reading theme: {}", self.preferences.theme.label()));
                self.persist_preferences_best_effort();
                self.request_clear_ghosting();
                false
            }
            ReadingPreference::DarkMode => {
                self.preferences.dark_mode = !self.preferences.dark_mode;
                self.last_message =
                    Some(format!("Dark mode: {}", on_off(self.preferences.dark_mode)));
                self.persist_preferences_best_effort();
                self.request_clear_ghosting();
                false
            }
            ReadingPreference::FullRefresh => {
                self.preferences.full_refresh = self.preferences.full_refresh.next();
                self.turns_since_refresh = 0;
                self.last_message = Some(format!(
                    "Full refresh: {}",
                    self.preferences.full_refresh.label()
                ));
                self.persist_preferences_best_effort();
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
            ReadingPreference::LetterSpacing => {
                self.preferences.letter_spacing = self.preferences.letter_spacing.next();
                self.last_message = Some(format!(
                    "Letter spacing: {}",
                    self.preferences.letter_spacing.label()
                ));
                true
            }
            ReadingPreference::LineSpacing => {
                self.preferences.line_spacing = self.preferences.line_spacing.next();
                self.last_message = Some(format!(
                    "Line spacing: {}",
                    self.preferences.line_spacing.label()
                ));
                true
            }
            ReadingPreference::ParagraphSpacing => {
                self.preferences.paragraph_spacing = self.preferences.paragraph_spacing.next();
                self.last_message = Some(format!(
                    "Paragraph spacing: {}",
                    self.preferences.paragraph_spacing.label()
                ));
                true
            }
            ReadingPreference::FirstLineIndent => {
                self.preferences.first_line_indent = !self.preferences.first_line_indent;
                self.last_message = Some(format!(
                    "First-line indent: {}",
                    on_off(self.preferences.first_line_indent)
                ));
                true
            }
            ReadingPreference::Justified => {
                self.preferences.justified = !self.preferences.justified;
                self.preferences.paragraph_alignment = if self.preferences.justified {
                    ParagraphAlignment::Justified
                } else if self.preferences.paragraph_alignment == ParagraphAlignment::Justified {
                    ParagraphAlignment::Left
                } else {
                    self.preferences.paragraph_alignment
                };
                self.last_message =
                    Some(format!("Justified: {}", on_off(self.preferences.justified)));
                true
            }
            ReadingPreference::MarginTop => {
                self.preferences.margin_top = self.preferences.margin_top.next();
                self.last_message = Some(format!(
                    "Top margin: {}",
                    self.preferences.margin_top.label()
                ));
                true
            }
            ReadingPreference::MarginBottom => {
                self.preferences.margin_bottom = self.preferences.margin_bottom.next();
                self.last_message = Some(format!(
                    "Bottom margin: {}",
                    self.preferences.margin_bottom.label()
                ));
                true
            }
            ReadingPreference::MarginLeft => {
                self.preferences.margin_left = self.preferences.margin_left.next();
                self.last_message = Some(format!(
                    "Left margin: {}",
                    self.preferences.margin_left.label()
                ));
                true
            }
            ReadingPreference::MarginRight => {
                self.preferences.margin_right = self.preferences.margin_right.next();
                self.last_message = Some(format!(
                    "Right margin: {}",
                    self.preferences.margin_right.label()
                ));
                true
            }
            ReadingPreference::ParagraphAlignment => {
                self.preferences.paragraph_alignment = self.preferences.paragraph_alignment.next();
                self.preferences.justified =
                    self.preferences.paragraph_alignment == ParagraphAlignment::Justified;
                self.last_message = Some(format!(
                    "Paragraph alignment: {}",
                    self.preferences.paragraph_alignment.label()
                ));
                true
            }
            ReadingPreference::StatusPage => {
                self.preferences.status_page = !self.preferences.status_page;
                self.preferences.show_progress = self.preferences.status_page;
                self.last_message = Some(format!(
                    "Page number: {}",
                    on_off(self.preferences.status_page)
                ));
                self.persist_preferences_best_effort();
                false
            }
            ReadingPreference::StatusChapter => {
                self.preferences.status_chapter = !self.preferences.status_chapter;
                self.last_message = Some(format!(
                    "Chapter progress: {}",
                    on_off(self.preferences.status_chapter)
                ));
                self.persist_preferences_best_effort();
                false
            }
            ReadingPreference::StatusTime => {
                self.preferences.status_time = !self.preferences.status_time;
                self.last_message = Some(format!("Time: {}", on_off(self.preferences.status_time)));
                self.persist_preferences_best_effort();
                false
            }
            ReadingPreference::StatusBattery => {
                self.preferences.status_battery = !self.preferences.status_battery;
                self.last_message = Some(format!(
                    "Battery: {}",
                    on_off(self.preferences.status_battery)
                ));
                self.persist_preferences_best_effort();
                false
            }
            ReadingPreference::SwapPageKeys => {
                self.preferences.swap_page_keys = !self.preferences.swap_page_keys;
                self.last_message = Some(format!(
                    "Swap page keys: {}",
                    on_off(self.preferences.swap_page_keys)
                ));
                self.persist_preferences_best_effort();
                false
            }
            ReadingPreference::LongPressChapter => {
                self.preferences.long_press_chapter = !self.preferences.long_press_chapter;
                self.last_message = Some(format!(
                    "Chapter jump: {}",
                    on_off(self.preferences.long_press_chapter)
                ));
                self.persist_preferences_best_effort();
                false
            }
            ReadingPreference::AutoPageTurn => {
                self.preferences.auto_page_turn = self.preferences.auto_page_turn.next();
                self.auto_turn_rearm = true;
                self.last_message = Some(format!(
                    "Auto page turn: {}",
                    self.preferences.auto_page_turn.label()
                ));
                self.persist_preferences_best_effort();
                false
            }
            ReadingPreference::TypographyMenu
            | ReadingPreference::PageMenu
            | ReadingPreference::DisplayMenu
            | ReadingPreference::StatusMenu
            | ReadingPreference::ControlsMenu
            | ReadingPreference::Done => false,
        };
        if !layout_sensitive {
            return false;
        }
        self.preferences_layout_dirty = true;
        false
    }

    /// Close the editor and rebuild an open TXT/EPUB at the current text offset.
    ///
    /// Returns true when a staged rebuild is now in progress.
    pub fn finish_preferences_edit(&mut self) -> bool {
        self.commit_deferred_layout(true)
    }

    /// Close the editor for WeRead. The local session keeps its pages; the
    /// caller repaginates the open chapter from the saved character offset.
    pub fn finish_shared_preferences_edit(&mut self) {
        let _ = self.commit_deferred_layout(false);
    }

    fn commit_deferred_layout(&mut self, rebuild_open_book: bool) -> bool {
        let dirty = self.preferences_layout_dirty;
        self.preferences_layout_dirty = false;
        if !dirty {
            return false;
        }
        if rebuild_open_book {
            self.request_layout_rebuild()
        } else {
            self.persist_shared_typography();
            false
        }
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
        self.note_deferred_layout()
    }

    pub fn cycle_book_font_size(&mut self) -> bool {
        self.preferences.font_size = self.preferences.font_size.next();
        self.last_message = Some(format!(
            "Book font size: {}",
            self.preferences.font_size.label()
        ));
        self.note_deferred_layout()
    }

    pub fn cycle_book_font(&mut self) -> bool {
        self.cycle_book_font_choice();
        self.last_message = Some(format!("Book font: {}", self.book_font_display_label()));
        self.note_deferred_layout()
    }

    /// PREFS.TXT and NVS are written once when the editor closes.
    fn note_deferred_layout(&mut self) -> bool {
        self.preferences_layout_dirty = true;
        false
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
        anchor_by_offset: bool,
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
        let mut uncached_anchor = None;
        let (mut page_number_base, page_offsets, current_page, indexed_through, index_complete) =
            if anchor_by_offset {
                let offset = requested
                    .filter(|location| location.matches_book(book))
                    .map(|location| location.byte_offset.min(book.size_bytes))
                    .unwrap_or(0);
                page_window_for_offset(cached.as_ref(), offset).unwrap_or_else(|| {
                    uncached_anchor = Some(offset);
                    (0, vec![offset], 0, offset, false)
                })
            } else if let Some(cache) = cached {
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
        let mut page = read_txt_page(book, encoding, layout, offset, absolute_page)?;
        if let Some(anchor) = uncached_anchor {
            page_number_base = estimated_page_for_offset(anchor, &page);
            page.page_index = page_number_base.saturating_add(current_page);
        }
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
            epub_chapter: None,
            epub_preparing: None,
            pending_show_last: false,
            pending_rewind_offset: None,
            epub_cache_dir: PathBuf::new(),
            epub_anchor_limit: self.epub_page_anchor_limit,
            epub_byte_limit: self.epub_index_bytes_limit,
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
        // Byte offset is the resume anchor. A saved page index from the old
        // whole-book index is not a chapter-local page, so it is not used to seek.
        let requested = requested.filter(|location| location.matches_book(book));
        let requested_offset = requested
            .map(|location| location.byte_offset.min(source_size))
            .unwrap_or(0);
        let chapter_index = document
            .chapters
            .iter()
            .position(|chapter| {
                requested_offset >= chapter.text_offset
                    && (requested_offset < chapter.text_end_offset
                        || (requested_offset == chapter.text_end_offset
                            && chapter.text_end_offset == source_size))
            })
            .unwrap_or(0);
        let mut session_book = book.clone();
        if !document.title.trim().is_empty() {
            session_book.title = document.title.clone();
        }
        let mut session = ReaderSession {
            book: session_book,
            encoding: TextEncoding::Utf8,
            epub_document: Some(document),
            layout,
            current_page: 0,
            page_number_base: 0,
            page_offsets: vec![0],
            indexed_through: 0,
            index_complete: false,
            index_truncated: false,
            epub_page_numbers_exact: false,
            cache: Vec::new(),
            epub_chapter_pages: Vec::new(),
            epub_chapter: None,
            epub_preparing: None,
            pending_show_last: false,
            pending_rewind_offset: None,
            epub_cache_dir: self.cache_directory(),
            epub_anchor_limit: self.epub_page_anchor_limit,
            epub_byte_limit: self.epub_index_bytes_limit,
        };
        session.jump_to_chapter(chapter_index, requested_offset)?;
        if let Some(warning) = session
            .epub_chapter
            .as_ref()
            .and_then(|chapter| chapter.warning.clone())
        {
            self.persistence_warning = Some(format!("EPUB chapter cache: {warning}"));
        }
        log::info!(
            "rustmix-wave=epub-open status=first-page-ready chapter={}/{} indexed-pages={} complete={} offset={}",
            session
                .epub_chapter
                .as_ref()
                .map(|chapter| chapter.chapter_number)
                .unwrap_or(0),
            session
                .epub_chapter
                .as_ref()
                .map(|chapter| chapter.chapter_count)
                .unwrap_or(0),
            session.indexed_page_count(),
            session.index_complete,
            session.view_offset()
        );
        Ok(session)
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
            session.store_epub_caches()?;
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
    let mut line_indent = layout.first_line_indent;
    let indent_px = layout.indent_px();
    let mut consumed = decoded
        .first()
        .map_or(0, |(_, offset)| offset.saturating_sub(1));

    let push_line = |lines: &mut Vec<ReaderPageLine>,
                     text: String,
                     paragraph_end: bool,
                     first_line_indent: bool|
     -> bool {
        lines.push(ReaderPageLine {
            text,
            paragraph_end,
            first_line_indent,
        });
        lines.len() >= layout.lines_per_page
    };
    let push_paragraph_gap = |lines: &mut Vec<ReaderPageLine>| -> bool {
        for _ in 0..layout.paragraph_gap_lines {
            if lines.len() >= layout.lines_per_page {
                return true;
            }
            lines.push(ReaderPageLine {
                text: String::new(),
                paragraph_end: false,
                first_line_indent: false,
            });
        }
        false
    };

    for (character, next_offset) in decoded.iter().copied() {
        let character = match character {
            '\r' => continue,
            '\n' => {
                let indent = line_indent && !line.is_empty();
                let full = push_line(&mut lines, core::mem::take(&mut line), true, indent);
                line_width = 0;
                line_indent = layout.first_line_indent;
                consumed = next_offset;
                if full || push_paragraph_gap(&mut lines) {
                    break;
                }
                continue;
            }
            value if value.is_control() => ' ',
            value => value,
        };
        let advance = layout.advance_px(character);
        let limit = (layout.max_line_width_px - if line_indent { indent_px } else { 0 }).max(1);
        if !line.is_empty() && line_width + advance > limit {
            let indent = line_indent;
            let full = push_line(&mut lines, core::mem::take(&mut line), false, indent);
            line_width = 0;
            line_indent = false;
            if full {
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
        let indent = line_indent && !line.is_empty();
        lines.push(ReaderPageLine {
            text: line,
            paragraph_end: true,
            first_line_indent: indent,
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
        pages.push(vec![ReaderPageLine::new(String::new(), true)]);
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

fn feed_cache_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(CACHE_FNV_PRIME);
    }
}

/// Hash preferences that change line breaks, the displayed script, or immersive chrome.
///
/// EPUB chapter caches call [`book_fingerprint`] and then mix in the chapter
/// range, so this helper is the only place those preferences are recorded.
fn feed_layout_preferences(hash: &mut u64, layout: &ReaderLayout) {
    feed_cache_bytes(hash, &layout.letter_spacing_px.to_le_bytes());
    feed_cache_bytes(hash, &layout.line_spacing_px.to_le_bytes());
    feed_cache_bytes(hash, &layout.paragraph_gap_lines.to_le_bytes());
    feed_cache_bytes(hash, &layout.margin_top_px.to_le_bytes());
    feed_cache_bytes(hash, &layout.margin_bottom_px.to_le_bytes());
    feed_cache_bytes(hash, &layout.margin_left_px.to_le_bytes());
    feed_cache_bytes(hash, &layout.margin_right_px.to_le_bytes());
    feed_cache_bytes(hash, &[u8::from(layout.first_line_indent)]);
    feed_cache_bytes(hash, &[u8::from(layout.justified)]);
    feed_cache_bytes(hash, layout.letter_spacing.marker().as_bytes());
    feed_cache_bytes(hash, layout.line_spacing.marker().as_bytes());
    feed_cache_bytes(hash, layout.paragraph_spacing.marker().as_bytes());
    feed_cache_bytes(hash, layout.chinese_script.marker().as_bytes());
    feed_cache_bytes(hash, &[u8::from(layout.immersive)]);
    feed_cache_bytes(
        hash,
        if layout.immersive {
            b"immersive"
        } else {
            b"chrome"
        },
    );
}

fn book_fingerprint(book: &ReaderBook, layout: ReaderLayout) -> u64 {
    let mut hash = CACHE_FNV_OFFSET;
    feed_cache_bytes(&mut hash, book.path.as_bytes());
    feed_cache_bytes(&mut hash, &book.size_bytes.to_le_bytes());
    feed_cache_bytes(&mut hash, &book.modified_seconds.to_le_bytes());
    feed_cache_bytes(&mut hash, book.format.marker().as_bytes());
    feed_cache_bytes(&mut hash, &layout.lines_per_page.to_le_bytes());
    feed_cache_bytes(&mut hash, &layout.chars_per_line.to_le_bytes());
    feed_cache_bytes(&mut hash, &layout.max_line_width_px.to_le_bytes());
    feed_cache_bytes(&mut hash, &layout.font_size_px.to_le_bytes());
    feed_cache_bytes(&mut hash, layout.orientation.marker().as_bytes());
    feed_cache_bytes(&mut hash, layout.font_size.marker().as_bytes());
    feed_cache_bytes(&mut hash, layout.book_font.marker().as_bytes());
    feed_cache_bytes(&mut hash, &layout.sd_cjk_file);
    feed_cache_bytes(&mut hash, layout.paragraph_alignment.marker().as_bytes());
    feed_layout_preferences(&mut hash, &layout);
    feed_cache_bytes(&mut hash, READER_CACHE_VERSION.as_bytes());
    hash
}

fn serialize_location(location: &ReaderLocation) -> String {
    format!(
        "version={}\npath={}\ntitle={}\nformat={}\nsize={}\nmodified={}\npage={}\noffset={}\nchapter={}\nchapter_page={}\nchapter_pages={}\nchapter_count={}\n",
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
        optional_usize(location.epub_chapter.as_ref().map(|chapter| chapter.page_count)),
        optional_usize(location.epub_chapter.as_ref().map(|chapter| chapter.chapter_count))
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
    let mut chapter_count = None;
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
            "chapter_count" => chapter_count = parse_optional_usize(value),
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
        epub_chapter: chapter_page_label(chapter, chapter_page, chapter_pages, chapter_count),
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
        optional_usize(
            location
                .epub_chapter
                .as_ref()
                .map(|chapter| chapter.chapter_count),
        ),
    ]
    .join("\t")
}

fn parse_location_fields(value: &str) -> Result<ReaderLocation, String> {
    let fields = split_escaped_tabs(value)?;
    // 7 fields: original TXT/EPUB offset record.
    // 10 fields: chapter page metadata without a chapter total.
    // 11 fields: chapter i/N plus the page within the chapter.
    if fields.len() != 7 && fields.len() != 10 && fields.len() != 11 {
        return Err("invalid location field count".into());
    }
    let epub_chapter = if fields.len() >= 10 {
        chapter_page_label(
            parse_optional_usize(&fields[7]),
            parse_optional_usize(&fields[8]),
            parse_optional_usize(&fields[9]),
            fields.get(10).and_then(|value| parse_optional_usize(value)),
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
    chapter_count: Option<usize>,
) -> Option<ReaderChapterPageLabel> {
    Some(ReaderChapterPageLabel {
        chapter_number: chapter_number?,
        chapter_count: chapter_count.unwrap_or(0),
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
                match parse_location_fields(value) {
                    Ok(location) => output.push(location),
                    Err(error) => {
                        log::info!(
                            "rustmix-wave=reader-progress status=ignored-entry error={error}"
                        );
                    }
                }
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
        atomic_replace_text, auto_page_turn_keeps_awake, book_format_from_path,
        chapter_cache_fingerprint, chapter_jump_forward, detect_txt_encoding,
        immersive_status_visible, is_fat83_safe_file_name, load_location_record,
        map_page_turn_event, normalize_decoded, paginate_decoded, parse_anchor_cache,
        parse_location_fields, parse_location_record, poll_auto_page_turn, scan_txt_library,
        serialize_location, serialize_location_fields, show_immersive_status,
        with_layout_button_poll, AutoPageTurn, AutoTurnClock, BookFont, BookFontSize, BookFormat,
        FullRefreshEvery, LetterSpacing, LineSpacing, PageMargin, ParagraphAlignment,
        ParagraphSpacing, ReaderBook, ReaderChapterPageLabel, ReaderLoadingStage, ReaderLocation,
        ReaderOrientation, ReaderPreferences, ReaderSession, ReaderTickOutcome, ReaderUiState,
        ReadingPreference, ReadingPreset, ReadingTheme, TextEncoding, LEGACY_READER_POSITIONS_FILE,
        READER_BOOKMARKS_FILE, READER_CACHE_OFFSET_LIMIT, READER_CACHE_VERSION,
        READER_EPUB_ANCHOR_INDEX_BYTES_LIMIT, READER_EPUB_INDEX_YIELD_EVERY_PAGES,
        READER_EPUB_INDEX_YIELD_MILLIS, READER_EPUB_PAGE_ANCHOR_LIMIT, READER_POSITIONS_FILE,
        READER_PREFS_FILE, READER_RECENT_FILE, READER_STATE_FILE,
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
        assert_eq!(parsed.letter_spacing, LetterSpacing::Px0);
        assert!(!parsed.show_progress);
        assert!(parsed.serialized().contains("font_size=48"));
        assert!(parsed.serialized().contains("book_font=serif"));
        assert!(parsed.serialized().contains("letter_spacing=0"));
        assert!(parsed.serialized().contains("paragraph_alignment=right"));
    }

    #[test]
    fn letter_spacing_persists_changes_wrap_and_cache_keys() {
        let parsed = ReaderPreferences::parse(
            "version=1\ntheme=classic\norientation=portrait\nfont_size=24\nbook_font=cjk-unifont\nletter_spacing=extra\nparagraph_alignment=left\nshow_progress=true\n",
        )
        .unwrap();
        assert_eq!(parsed.letter_spacing, LetterSpacing::Px4);
        assert_eq!(
            ReaderPreferences::parse(&parsed.serialized())
                .unwrap()
                .letter_spacing,
            LetterSpacing::Px4
        );
        assert_eq!(LetterSpacing::Px0.next(), LetterSpacing::Px1);
        assert_eq!(LetterSpacing::Px4.next(), LetterSpacing::Px0);
        assert_eq!(LetterSpacing::parse("tight").unwrap(), LetterSpacing::Px0);
        assert_eq!(LetterSpacing::parse("loose").unwrap(), LetterSpacing::Px2);

        let tight = ReaderPreferences {
            book_font: BookFont::CjkUnifont,
            letter_spacing: LetterSpacing::Px0,
            ..ReaderPreferences::default()
        };
        let loose = ReaderPreferences {
            letter_spacing: LetterSpacing::Px4,
            ..tight
        };
        assert_ne!(tight.layout(), loose.layout());
        assert_eq!(loose.layout().letter_spacing_px, 4);
        assert_eq!(
            loose.layout().advance_px('A') - tight.layout().advance_px('A'),
            4
        );
        assert_eq!(
            loose.layout().advance_px('中') - tight.layout().advance_px('中'),
            4
        );
        let book = ReaderBook {
            path: "BOOK.TXT".into(),
            title: "Book".into(),
            format: BookFormat::Text,
            size_bytes: 100,
            modified_seconds: 1,
        };
        assert_ne!(
            ReaderUiState::cache_file_name_for(&book, tight.layout()),
            ReaderUiState::cache_file_name_for(&book, loose.layout())
        );

        let latin: Vec<(char, u64)> = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd"
            .chars()
            .enumerate()
            .map(|(index, value)| (value, index as u64 + 1))
            .collect();
        let (tight_latin, _) = paginate_decoded(&latin, tight.layout());
        let (loose_latin, _) = paginate_decoded(&latin, loose.layout());
        assert!(
            loose_latin[0].text.chars().count() < tight_latin[0].text.chars().count(),
            "latin extra advance should wrap sooner"
        );

        let cjk: Vec<(char, u64)> = "中"
            .repeat(24)
            .chars()
            .enumerate()
            .map(|(index, value)| (value, index as u64 + 1))
            .collect();
        let (tight_cjk, _) = paginate_decoded(&cjk, tight.layout());
        let (loose_cjk, _) = paginate_decoded(&cjk, loose.layout());
        assert!(tight_cjk[0].text.contains('中'));
        assert!(
            loose_cjk[0].text.chars().count() < tight_cjk[0].text.chars().count(),
            "cjk extra advance should wrap sooner"
        );
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
        assert!(!reader.cycle_book_font_size());
        assert!(reader.loading_stage().is_none());
        assert!(reader.layout_changes_pending());
        assert!(reader.finish_preferences_edit());
        assert_eq!(
            reader.loading_stage(),
            Some(ReaderLoadingStage::UpdatingLayout)
        );
        assert!(state.join(READER_PREFS_FILE).exists());
    }

    #[test]
    fn layout_rebuild_keeps_the_text_offset_until_the_menu_closes() {
        let root = temp_dir("offset-books");
        let state = temp_dir("offset-state");
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
        reader.next_page();
        let offset = reader
            .session
            .as_ref()
            .unwrap()
            .current_location()
            .byte_offset;
        assert!(offset > 0);
        reader.begin_preferences_edit();
        assert!(!reader.cycle_book_font_size());
        assert!(reader.session.is_some());
        assert_eq!(
            reader
                .session
                .as_ref()
                .unwrap()
                .current_location()
                .byte_offset,
            offset
        );
        assert!(reader.finish_preferences_edit());
        assert_eq!(reader.tick(), ReaderTickOutcome::LoadingStageChanged);
        assert_eq!(reader.tick(), ReaderTickOutcome::FirstPageReady);
        let session = reader.session.as_ref().unwrap();
        assert_eq!(session.current_location().byte_offset, offset);
        assert!(session.current_absolute_page() >= 1);
        assert!(session.current_cached_page().is_some());
    }

    #[test]
    fn letter_spacing_change_rebuilds_the_open_page() {
        let root = temp_dir("spacing-books");
        let state = temp_dir("spacing-state");
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
        reader.begin_preferences_edit();
        reader.cycle_preference_next();
        assert_eq!(
            reader.selected_preference(),
            ReadingPreference::TypographyMenu
        );
        assert!(!reader.activate_selected_preference());
        reader.cycle_preference_next();
        reader.cycle_preference_next();
        assert_eq!(
            reader.selected_preference(),
            ReadingPreference::LetterSpacing
        );
        assert!(!reader.activate_selected_preference());
        assert_eq!(reader.preferences.letter_spacing, LetterSpacing::Px1);
        assert!(reader.loading_stage().is_none());
        let before = fs::read_to_string(state.join(READER_PREFS_FILE)).unwrap_or_default();
        assert!(!before.contains("letter_spacing=1"));
        assert!(reader.finish_preferences_edit());
        assert_eq!(
            reader.loading_stage(),
            Some(ReaderLoadingStage::UpdatingLayout)
        );
        let prefs = fs::read_to_string(state.join(READER_PREFS_FILE)).unwrap();
        assert!(prefs.contains("letter_spacing=1"));
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
        assert_eq!(reader.selected_preference(), ReadingPreference::Presets);
        reader.cycle_preference_next();
        assert_eq!(
            reader.selected_preference(),
            ReadingPreference::TypographyMenu
        );
        assert!(!reader.activate_selected_preference());
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
        assert!(reader.close_preference_submenu());
        assert_eq!(reader.selected_preference(), ReadingPreference::Presets);
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
            epub_chapter: None,
            epub_preparing: None,
            pending_show_last: false,
            pending_rewind_offset: None,
            epub_cache_dir: PathBuf::new(),
            epub_anchor_limit: READER_EPUB_PAGE_ANCHOR_LIMIT,
            epub_byte_limit: READER_EPUB_ANCHOR_INDEX_BYTES_LIMIT,
        });
        assert_eq!(reader.bookmark_display_page(&bookmark), 3);
    }

    #[test]
    fn epub_chapter_labels_use_chapter_relative_page_totals_and_persist() {
        let label = ReaderChapterPageLabel {
            chapter_number: 3,
            chapter_count: 12,
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
        assert_eq!(label.chapter_text(), "3/12");
        assert_eq!(label.page_text(), "2/9");
    }

    #[test]
    fn legacy_location_fields_without_chapter_metadata_remain_readable() {
        let location = parse_location_fields("book.txt\tBook\ttxt\t10\t0\t2\t5").unwrap();
        assert_eq!(location.format, BookFormat::Text);
        assert_eq!(location.epub_chapter, None);
        let chapter = parse_location_fields("book.epub\tBook\tepub\t10\t0\t2\t5\t4\t3\t12")
            .unwrap()
            .epub_chapter
            .unwrap();
        assert_eq!(chapter.chapter_number, 4);
        assert_eq!(chapter.chapter_count, 0);
        assert_eq!(chapter.chapter_text(), "4");
        assert_eq!(chapter.page_text(), "3/12");
    }

    #[test]
    fn corrupt_progress_entries_are_skipped() {
        let list = super::parse_location_list(
            "version=1\nentry=bad\nentry=book.txt\tBook\ttxt\t10\t0\t2\t5\n",
            10,
        )
        .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].byte_offset, 5);
        assert!(super::parse_location_list(
            "version=99\nentry=book.txt\tBook\ttxt\t1\t0\t0\t0\n",
            10
        )
        .is_err());
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
    fn long_cjk_epub_opens_the_current_chapter_without_indexing_the_rest() {
        let root = temp_dir("long-cjk-books");
        let state = temp_dir("long-cjk-state");
        let mut probe = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        probe.preferences.font_size = BookFontSize::Px16;
        probe.preferences.book_font = BookFont::CjkUnifont;
        let lines = probe.preferences.layout().lines_per_page.max(2);
        write_cjk_epub(&root, 2, lines * 2_200);

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
        {
            let session = reader.session.as_ref().unwrap();
            let chapter = session.epub_chapter.as_ref().unwrap();
            assert_eq!(chapter.chapter_number, 1);
            assert_eq!(chapter.index, 0);
            assert!(session.display_page_label().contains("CH 1/2"));
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
        }
        for _ in 0..8 {
            assert_ne!(reader.tick(), ReaderTickOutcome::Failed);
            assert_eq!(
                reader
                    .session
                    .as_ref()
                    .unwrap()
                    .epub_chapter
                    .as_ref()
                    .unwrap()
                    .index,
                0
            );
        }
        for _ in 0..12 {
            if reader
                .session
                .as_ref()
                .unwrap()
                .epub_chapter
                .as_ref()
                .unwrap()
                .index
                == 1
            {
                break;
            }
            if reader.session.as_ref().unwrap().index_complete {
                reader.next_page();
            } else {
                reader.tick();
            }
        }
        let session = reader.session.as_ref().unwrap();
        assert_eq!(session.epub_chapter.as_ref().unwrap().chapter_number, 2);
        assert!(session.indexed_page_count() < 64);
        assert!(!session.index_complete);
        assert!(session.display_page_label().contains("CH 2/2"));
        assert!(READER_EPUB_PAGE_ANCHOR_LIMIT >= 200_000);
    }

    #[test]
    fn epub_chapter_cap_keeps_the_book_open_with_approximate_progress() {
        let root = temp_dir("cap-cjk-books");
        let state = temp_dir("cap-cjk-state");
        write_cjk_epub(&root, 80, 1);
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
        for _ in 0..8 {
            if reader.session.as_ref().unwrap().index_truncated {
                break;
            }
            assert_ne!(reader.tick(), ReaderTickOutcome::Failed);
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
    fn laid_out_epub_chapter_reopens_from_its_cache_and_byte_offset() {
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
        for _ in 0..40 {
            let session = reader.session.as_ref().unwrap();
            if session.epub_chapter.as_ref().unwrap().index == 1 && session.index_complete {
                break;
            }
            if session.index_complete {
                reader.next_page();
            } else {
                reader.tick();
            }
        }
        let session = reader.session.as_ref().unwrap();
        assert_eq!(session.epub_chapter.as_ref().unwrap().chapter_number, 2);
        assert!(session.index_complete);
        assert!(session.epub_index_on_sd());
        let mut saved = session.current_location();
        let offset = saved.byte_offset;
        saved.page_index = 50_000;

        let mut restored = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        restored.preferences.font_size = BookFontSize::Px32;
        restored.preferences.book_font = BookFont::CjkUnifont;
        restored.request_open_book(book, Some(saved));
        open_until_ready(&mut restored);
        let session = restored.session.as_ref().unwrap();
        assert_eq!(session.view_offset(), offset);
        assert_eq!(session.epub_chapter.as_ref().unwrap().chapter_number, 2);
        assert!(session.index_complete);
        assert!(session.epub_chapter.as_ref().unwrap().index == 1);
        let label = session.display_page_label();
        assert!(label.contains("CH 2/2"));
        assert!(label.contains('/'));
        assert!(!label.contains('+'));
    }

    #[test]
    fn epub_layout_polls_buttons_between_batches() {
        let root = temp_dir("poll-cjk-books");
        let state = temp_dir("poll-cjk-state");
        write_cjk_epub(&root, 800, 1);
        let mut reader = ReaderUiState::with_roots(
            root.to_string_lossy().into_owned(),
            state.to_string_lossy().into_owned(),
        );
        reader.preferences.font_size = BookFontSize::Px16;
        reader.preferences.book_font = BookFont::CjkUnifont;
        reader.refresh_library();
        let book = reader.books[0].clone();
        reader.request_open_book(book, None);
        open_until_ready(&mut reader);

        let mut polls = 0_usize;
        let added = with_layout_button_poll(
            &mut || {
                polls += 1;
                false
            },
            || {
                reader
                    .session
                    .as_mut()
                    .unwrap()
                    .layout_epub_batch(16)
                    .unwrap()
            },
        );
        assert_eq!(added, 16);
        assert!(polls >= 3, "polls={polls}");

        polls = 0;
        let added = with_layout_button_poll(
            &mut || {
                polls += 1;
                true
            },
            || {
                reader
                    .session
                    .as_mut()
                    .unwrap()
                    .layout_epub_batch(64)
                    .unwrap()
            },
        );
        assert_eq!(polls, 1);
        assert!(added <= READER_EPUB_INDEX_YIELD_EVERY_PAGES);
        assert!(added > 0);
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
    fn layout_spacing_margins_indent_and_justification_change_pages_and_cache_keys() {
        let base = ReaderPreferences {
            book_font: BookFont::CjkUnifont,
            font_size: BookFontSize::Px16,
            ..ReaderPreferences::default()
        };
        let spaced = ReaderPreferences {
            line_spacing: LineSpacing::Px12,
            ..base
        };
        assert!(spaced.layout().lines_per_page < base.layout().lines_per_page);
        assert_eq!(spaced.layout().line_spacing_px, 12);

        let inset = ReaderPreferences {
            margin_left: PageMargin::Px24,
            margin_right: PageMargin::Px24,
            margin_top: PageMargin::Px16,
            margin_bottom: PageMargin::Px16,
            ..base
        };
        assert!(inset.layout().max_line_width_px < base.layout().max_line_width_px);
        assert!(inset.layout().lines_per_page <= base.layout().lines_per_page);

        let gapped = ReaderPreferences {
            paragraph_spacing: ParagraphSpacing::Lines1,
            ..base
        };
        let decoded: Vec<(char, u64)> = "Hello\nWorld"
            .chars()
            .enumerate()
            .map(|(index, value)| (value, index as u64 + 1))
            .collect();
        let (lines, _) = paginate_decoded(&decoded, gapped.layout());
        assert_eq!(lines[0].text, "Hello");
        assert!(lines[0].paragraph_end);
        assert!(lines[1].text.is_empty());
        assert_eq!(lines[2].text, "World");

        let indented = ReaderPreferences {
            first_line_indent: true,
            ..base
        };
        let cjk: Vec<(char, u64)> = "中"
            .repeat(40)
            .chars()
            .enumerate()
            .map(|(index, value)| (value, index as u64 + 1))
            .collect();
        let (plain, _) = paginate_decoded(&cjk, base.layout());
        let (first, _) = paginate_decoded(&cjk, indented.layout());
        assert!(first[0].first_line_indent);
        assert!(!first[1].first_line_indent);
        assert!(first[0].text.chars().count() < plain[0].text.chars().count());

        let mut ragged = base;
        ragged.justified = false;
        ragged.paragraph_alignment = ParagraphAlignment::Left;
        assert_ne!(base.layout(), ragged.layout());
        assert_eq!(ragged.effective_alignment(), ParagraphAlignment::Left);

        let tracked = ReaderPreferences {
            letter_spacing: LetterSpacing::Px2,
            ..base
        };
        let traditional = ReaderPreferences {
            chinese_script: crate::reader_hanzi::ChineseScript::Traditional,
            ..base
        };
        assert_eq!(
            traditional.layout().lines_per_page,
            base.layout().lines_per_page
        );
        assert_eq!(
            traditional.layout().chinese_script,
            crate::reader_hanzi::ChineseScript::Traditional
        );
        let mut display_only = base;
        display_only.dark_mode = true;
        display_only.auto_page_turn = AutoPageTurn::Secs30;
        assert_eq!(base.layout(), display_only.layout());

        let book = ReaderBook {
            path: "BOOK.TXT".into(),
            title: "Book".into(),
            format: BookFormat::Text,
            size_bytes: 40,
            modified_seconds: 1,
        };
        let names = [
            ReaderUiState::cache_file_name_for(&book, base.layout()),
            ReaderUiState::cache_file_name_for(&book, spaced.layout()),
            ReaderUiState::cache_file_name_for(&book, inset.layout()),
            ReaderUiState::cache_file_name_for(&book, gapped.layout()),
            ReaderUiState::cache_file_name_for(&book, indented.layout()),
            ReaderUiState::cache_file_name_for(&book, ragged.layout()),
            ReaderUiState::cache_file_name_for(&book, tracked.layout()),
            ReaderUiState::cache_file_name_for(&book, traditional.layout()),
        ];
        for (index, name) in names.iter().enumerate() {
            for other in names.iter().skip(index + 1) {
                assert_ne!(name, other);
            }
        }
        assert_ne!(
            chapter_cache_fingerprint(&book, base.layout(), 1, 0, 40),
            chapter_cache_fingerprint(&book, traditional.layout(), 1, 0, 40)
        );
        assert_ne!(
            chapter_cache_fingerprint(&book, base.layout(), 1, 0, 40),
            chapter_cache_fingerprint(&book, tracked.layout(), 1, 0, 40)
        );
        assert_eq!(READER_CACHE_VERSION, "6");
    }

    #[test]
    fn anchor_cache_rejects_version_three_files() {
        let book = ReaderBook {
            path: "BOOK.TXT".into(),
            title: "Book".into(),
            format: BookFormat::Text,
            size_bytes: 40,
            modified_seconds: 1,
        };
        for version in ["3", "4", "5"] {
            let text = format!(
                "version={version}\nfingerprint=0000000000000001\nbase_page=0\nindexed_through=1\ncomplete=false\noffset=0\n"
            );
            let error = parse_anchor_cache(&text, &book, ReaderPreferences::default().layout())
                .unwrap_err();
            assert_eq!(error, "unsupported cache version");
        }
    }

    #[test]
    fn immersive_reading_fills_the_panel_and_changes_cache_keys() {
        let chrome = ReaderPreferences {
            margin_top: PageMargin::Px24,
            margin_bottom: PageMargin::Px24,
            margin_left: PageMargin::Px16,
            margin_right: PageMargin::Px16,
            ..ReaderPreferences::default()
        };
        let mut full = chrome;
        full.immersive = true;
        let layout = full.layout();
        assert!(layout.immersive);
        assert!(layout.lines_per_page > chrome.layout().lines_per_page);
        assert!(layout.max_line_width_px > chrome.layout().max_line_width_px);
        assert_eq!(layout.margin_top_px, 0);
        assert_eq!(layout.margin_bottom_px, 0);
        assert_eq!(layout.margin_left_px, 8);
        assert_eq!(layout.margin_right_px, 8);
        assert_eq!(chrome.margin_top, PageMargin::Px24);
        let book = ReaderBook {
            path: "BOOK.TXT".into(),
            title: "Book".into(),
            format: BookFormat::Text,
            size_bytes: 40,
            modified_seconds: 1,
        };
        assert_ne!(
            ReaderUiState::cache_file_name_for(&book, chrome.layout()),
            ReaderUiState::cache_file_name_for(&book, layout)
        );
        assert_ne!(
            chapter_cache_fingerprint(&book, chrome.layout(), 0, 0, 40),
            chapter_cache_fingerprint(&book, layout, 0, 0, 40)
        );
        let landscape = ReaderPreferences {
            immersive: true,
            orientation: ReaderOrientation::Landscape,
            ..ReaderPreferences::default()
        };
        assert!(landscape.layout().max_line_width_px > layout.max_line_width_px);
        let mut sd_one = ReaderPreferences::default();
        sd_one.book_font = BookFont::SdCjk;
        sd_one.set_sd_cjk_file_name(Some("ONE.BIN"));
        let mut sd_two = sd_one;
        sd_two.set_sd_cjk_file_name(Some("TWO.BIN"));
        assert_ne!(sd_one.layout().sd_cjk_file, sd_two.layout().sd_cjk_file);
        assert_ne!(
            ReaderUiState::cache_file_name_for(&book, sd_one.layout()),
            ReaderUiState::cache_file_name_for(&book, sd_two.layout())
        );
        assert_ne!(
            chapter_cache_fingerprint(&book, sd_one.layout(), 0, 0, 40),
            chapter_cache_fingerprint(&book, sd_two.layout(), 0, 0, 40)
        );
        assert!(show_immersive_status(ButtonEvent::Select, 600, true));
        assert!(!show_immersive_status(ButtonEvent::Select, 599, true));
        assert!(!show_immersive_status(ButtonEvent::Select, 900, false));
        assert!(!show_immersive_status(ButtonEvent::Down, 900, true));
        assert!(immersive_status_visible(2_499));
        assert!(!immersive_status_visible(2_500));
    }

    #[test]
    fn version_one_preferences_migrate_and_round_trip() {
        let legacy = ReaderPreferences::parse(
            "version=1\ntheme=high-contrast\norientation=landscape\nfont_size=xlarge\nbook_font=serif\nparagraph_alignment=right\nshow_progress=false\n",
        )
        .unwrap();
        assert_eq!(legacy.paragraph_alignment, ParagraphAlignment::Right);
        assert!(!legacy.justified);
        assert!(!legacy.show_progress);
        assert!(!legacy.status_page);
        assert!(!legacy.status_chapter);
        assert!(!legacy.status_time);
        assert!(!legacy.status_battery);
        assert_eq!(legacy.line_spacing, LineSpacing::Px0);
        assert_eq!(legacy.margin_left, PageMargin::Px0);
        assert!(!legacy.first_line_indent);
        assert!(!legacy.dark_mode);
        assert_eq!(legacy.full_refresh, FullRefreshEvery::Off);
        assert_eq!(legacy.auto_page_turn, AutoPageTurn::Off);
        assert_eq!(
            legacy.chinese_script,
            crate::reader_hanzi::ChineseScript::Original
        );
        assert!(!legacy.immersive);
        let stored = legacy.serialized();
        assert!(stored.starts_with("version=2\n"));
        assert_eq!(ReaderPreferences::parse(&stored).unwrap(), legacy);

        let modern = ReaderPreferences::parse(
            "version=2\ntheme=classic\norientation=portrait\nfont_size=24\nbook_font=serif\nletter_spacing=1\nline_spacing=8\nparagraph_spacing=2\nmargin_top=8\nmargin_bottom=16\nmargin_left=24\nmargin_right=0\nfirst_line_indent=true\njustified=false\nparagraph_alignment=center\nchinese=traditional\nimmersive=true\ndark_mode=true\nfull_refresh=10\nshow_progress=true\nstatus_page=true\nstatus_chapter=false\nstatus_time=true\nstatus_battery=true\nswap_page_keys=true\nlong_press_chapter=true\nauto_page_turn=60\n",
        )
        .unwrap();
        assert_eq!(modern.line_spacing, LineSpacing::Px8);
        assert_eq!(modern.paragraph_spacing, ParagraphSpacing::Lines2);
        assert_eq!(modern.margin_top, PageMargin::Px8);
        assert_eq!(modern.margin_right, PageMargin::Px0);
        assert!(modern.first_line_indent);
        assert!(!modern.justified);
        assert_eq!(modern.paragraph_alignment, ParagraphAlignment::Center);
        assert!(modern.immersive);
        assert!(modern.dark_mode);
        assert!(modern.serialized().contains("\nshow_progress=true\n"));
        assert!(modern.serialized().contains("\nstatus_page=true\n"));
        let mut split = ReaderPreferences::default();
        split.show_progress = false;
        split.status_page = true;
        let stored = split.serialized();
        assert!(stored.contains("\nshow_progress=false\n"));
        assert!(stored.contains("\nstatus_page=true\n"));
        let base = ReaderPreferences::default().serialized();
        let with_unknown = format!("{base}future_setting=1\n");
        assert_eq!(
            ReaderPreferences::parse(&with_unknown).unwrap(),
            ReaderPreferences::parse(&base).unwrap()
        );
        assert!(modern.status_time);
        assert!(!modern.status_chapter);
        assert!(modern.swap_page_keys);
        assert_eq!(modern.auto_page_turn, AutoPageTurn::Secs60);
        assert_eq!(modern.full_refresh, FullRefreshEvery::Turns10);
        assert_eq!(
            ReaderPreferences::parse(&modern.serialized()).unwrap(),
            modern
        );
    }

    #[test]
    fn presets_apply_layout_combos_without_dropping_other_preferences() {
        let mut prefs = ReaderPreferences::default();
        prefs.dark_mode = true;
        prefs.swap_page_keys = true;
        assert_eq!(prefs.matching_preset(), None);
        let applied = prefs.cycle_preset();
        assert_eq!(applied, ReadingPreset::Compact);
        assert!(prefs.immersive);
        assert_eq!(prefs.font_size, BookFontSize::Px20);
        assert!(!prefs.first_line_indent);
        assert_eq!(prefs.letter_spacing, LetterSpacing::Px0);
        assert!(prefs.dark_mode);
        assert!(prefs.swap_page_keys);
        assert_eq!(prefs.cycle_preset(), ReadingPreset::Comfortable);
        assert!(!prefs.immersive);
        assert_eq!(prefs.font_size, BookFontSize::Px24);
        assert_eq!(prefs.letter_spacing, LetterSpacing::Px1);
        assert_eq!(prefs.line_spacing, LineSpacing::Px4);
        assert_eq!(prefs.paragraph_spacing, ParagraphSpacing::Lines1);
        assert_eq!(prefs.margin_left, PageMargin::Px8);
        assert!(prefs.first_line_indent);
        assert!(prefs.justified);
        assert_eq!(prefs.cycle_preset(), ReadingPreset::LargePrint);
        assert!(!prefs.immersive);
        assert_eq!(prefs.font_size, BookFontSize::Px48);
        assert_eq!(prefs.letter_spacing, LetterSpacing::Px2);
        assert_eq!(prefs.line_spacing, LineSpacing::Px12);
        assert_eq!(prefs.paragraph_spacing, ParagraphSpacing::Lines2);
        assert_eq!(prefs.margin_top, PageMargin::Px16);
        assert!(!prefs.first_line_indent);
        assert!(!prefs.justified);
        assert_eq!(prefs.paragraph_alignment, ParagraphAlignment::Left);
        assert_eq!(prefs.matching_preset(), Some(ReadingPreset::LargePrint));
    }

    #[test]
    fn full_refresh_counter_and_auto_turn_pause_on_a_key() {
        let mut reader = ReaderUiState::default();
        reader.preferences.full_refresh = FullRefreshEvery::Turns5;
        for _ in 0..4 {
            reader.note_page_turn();
        }
        assert!(!reader.take_clear_ghost_request());
        reader.note_page_turn();
        assert!(reader.take_clear_ghost_request());
        reader.note_page_turn();
        assert!(!reader.take_clear_ghost_request());

        reader.preferences.full_refresh = FullRefreshEvery::Off;
        reader.preferences.dark_mode = false;
        reader.note_page_turn();
        assert!(!reader.take_clear_ghost_request());
        reader.preferences.dark_mode = true;
        for _ in 0..4 {
            reader.note_page_turn();
        }
        assert!(!reader.take_clear_ghost_request());
        reader.note_page_turn();
        assert!(reader.take_clear_ghost_request());
        reader.preferences.full_refresh = FullRefreshEvery::Turns10;
        for _ in 0..9 {
            reader.note_page_turn();
        }
        assert!(!reader.take_clear_ghost_request());
        reader.note_page_turn();
        assert!(reader.take_clear_ghost_request());

        let clock = AutoTurnClock::new(1_000);
        assert!(!clock.due(15_999, AutoPageTurn::Secs15));
        assert!(clock.due(16_000, AutoPageTurn::Secs15));
        let paused = clock.on_key();
        assert!(!paused.due(80_000, AutoPageTurn::Secs15));
        let armed = paused.on_interval_set(90_000);
        assert!(!armed.due(104_999, AutoPageTurn::Secs15));
        assert!(armed.due(105_000, AutoPageTurn::Secs15));
        assert!(!poll_auto_page_turn(200_000, 0, AutoPageTurn::Off, false));
        assert!(auto_page_turn_keeps_awake(AutoPageTurn::Secs120, false));
        assert!(!auto_page_turn_keeps_awake(AutoPageTurn::Secs60, true));
        assert!(!auto_page_turn_keeps_awake(AutoPageTurn::Off, false));
        assert_eq!(
            map_page_turn_event(ButtonEvent::Down, true),
            ButtonEvent::Up
        );
        assert_eq!(
            map_page_turn_event(ButtonEvent::Select, true),
            ButtonEvent::Select
        );
        assert_eq!(
            chapter_jump_forward(ButtonEvent::Down, 600, true, false),
            Some(true)
        );
        assert_eq!(
            chapter_jump_forward(ButtonEvent::Up, 600, true, true),
            Some(true)
        );
        assert_eq!(
            chapter_jump_forward(ButtonEvent::Down, 599, true, false),
            None
        );
        assert_eq!(
            chapter_jump_forward(ButtonEvent::Select, 2_000, true, false),
            None
        );
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
