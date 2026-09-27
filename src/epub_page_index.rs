//! Paged EPUB page-anchor index.
//!
//! Anchors live in a fixed-size SD file (or a bounded RAM fallback when the
//! card cannot be written). The Reader keeps only a small reading window in
//! memory, so a long CJK book is not capped by one in-RAM array and a hostile
//! EPUB cannot grow the index without limit.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

/// Bytes occupied by the index header.
pub const HEADER_LEN: usize = 64;
/// Chapter-directory entries reserved in every index file.
pub const MAX_CHAPTERS: usize = 128;
/// On-disk size of one chapter-directory record.
pub const CHAPTER_STRIDE: usize = 32;
/// Byte offset where the anchor array begins.
pub const ANCHOR_BASE: usize = HEADER_LEN + CHAPTER_STRIDE * MAX_CHAPTERS;

const MAGIC: &[u8; 4] = b"EPIX";
const VERSION: u16 = 1;
const FLAG_COMPLETE: u16 = 1;
const FLAG_TRUNCATED: u16 = 2;
const CHAPTER_COMPLETE: u16 = 1;
const NO_PAGE: u32 = u32::MAX;

/// Caps applied while an index is created or extended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexLimits {
    pub page_limit: usize,
    pub bytes_limit: usize,
    pub ram_fallback_limit: usize,
}

/// Result of trying to record one more page anchor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendStatus {
    Stored,
    Complete,
    Truncated,
}

/// One spine chapter tracked by the page index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedChapter {
    pub chapter_number: usize,
    pub text_offset: u64,
    pub text_end_offset: u64,
    pub first_page: Option<usize>,
    pub page_count: usize,
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AnchorStorage {
    Sd { path: PathBuf },
    Ram { anchors: Vec<u64> },
}

/// Bounded page-anchor store for one EPUB layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpubAnchorIndex {
    storage: AnchorStorage,
    fingerprint: u64,
    source_size: u64,
    limits: IndexLimits,
    page_count: usize,
    next_anchor: u64,
    complete: bool,
    truncated: bool,
    chapters: Vec<IndexedChapter>,
    pub warning: Option<String>,
}

impl EpubAnchorIndex {
    /// Load a matching index or create one. Storage failures fall back to a
    /// bounded RAM table so opening the book still succeeds.
    #[must_use]
    pub fn open_or_create(
        path: &Path,
        fingerprint: u64,
        source_size: u64,
        chapters: &[IndexedChapter],
        limits: IndexLimits,
    ) -> Self {
        match Self::load(path, fingerprint, source_size, chapters, limits) {
            Ok(Some(index)) => index,
            Ok(None) => Self::create(path, fingerprint, source_size, chapters, limits)
                .unwrap_or_else(|error| {
                    Self::ram_only(fingerprint, source_size, chapters, limits, error)
                }),
            Err(error) => Self::create(path, fingerprint, source_size, chapters, limits)
                .unwrap_or_else(|_| {
                    Self::ram_only(fingerprint, source_size, chapters, limits, error)
                }),
        }
    }

    pub fn load(
        path: &Path,
        fingerprint: u64,
        source_size: u64,
        chapters: &[IndexedChapter],
        limits: IndexLimits,
    ) -> Result<Option<Self>, String> {
        if !path.exists() {
            return Ok(None);
        }
        match Self::read_existing(path, fingerprint, source_size, chapters, limits) {
            Ok(index) => Ok(Some(index)),
            Err(error) => {
                let _ = fs::remove_file(path);
                log::info!(
                    "rustmix-wave=epub-page-index status=rebuilt reason=invalid-file error={error}"
                );
                Ok(None)
            }
        }
    }

    pub fn create(
        path: &Path,
        fingerprint: u64,
        source_size: u64,
        chapters: &[IndexedChapter],
        limits: IndexLimits,
    ) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or_else(|| "EPUB page index path has no parent".to_string())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
        let mut index = Self {
            storage: AnchorStorage::Sd {
                path: path.to_path_buf(),
            },
            fingerprint,
            source_size,
            limits,
            page_count: 0,
            next_anchor: 0,
            complete: source_size == 0,
            truncated: false,
            chapters: fresh_chapters(chapters),
            warning: None,
        };
        index.write_header_and_directory(true)?;
        Ok(index)
    }

    #[must_use]
    pub fn ram_only(
        fingerprint: u64,
        source_size: u64,
        chapters: &[IndexedChapter],
        limits: IndexLimits,
        warning: String,
    ) -> Self {
        log::info!(
            "rustmix-wave=epub-page-index status=ram-fallback limit={} error={warning}",
            limits.ram_fallback_limit
        );
        Self {
            storage: AnchorStorage::Ram {
                anchors: Vec::new(),
            },
            fingerprint,
            source_size,
            limits,
            page_count: 0,
            next_anchor: 0,
            complete: source_size == 0,
            truncated: false,
            chapters: fresh_chapters(chapters),
            warning: Some(warning),
        }
    }

    #[must_use]
    pub fn page_count(&self) -> usize {
        self.page_count
    }

    #[must_use]
    pub fn next_anchor(&self) -> u64 {
        self.next_anchor
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.complete || self.truncated
    }

    #[must_use]
    pub fn on_sd(&self) -> bool {
        matches!(self.storage, AnchorStorage::Sd { .. })
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match &self.storage {
            AnchorStorage::Sd { path } => Some(path),
            AnchorStorage::Ram { .. } => None,
        }
    }

    #[must_use]
    pub fn chapters(&self) -> &[IndexedChapter] {
        &self.chapters
    }

    pub fn mark_complete(&mut self) {
        self.complete = true;
        if self.next_anchor < self.source_size {
            self.next_anchor = self.source_size;
        }
        for chapter in &mut self.chapters {
            chapter.complete = true;
        }
    }

    pub fn mark_truncated(&mut self) {
        if !self.truncated {
            log::info!(
                "rustmix-wave=epub-page-index status=truncated pages={} limit={} bytes-limit={}",
                self.page_count,
                self.limits.page_limit,
                self.limits.bytes_limit
            );
        }
        self.truncated = true;
    }

    /// Record the page that starts at `page_start`. `next_start` is the first
    /// byte of the following page, or the source length when the book ends.
    pub fn append(&mut self, page_start: u64, next_start: u64) -> Result<AppendStatus, String> {
        if self.complete {
            return Ok(AppendStatus::Complete);
        }
        if self.truncated || !self.can_store_another() {
            self.mark_truncated();
            return Ok(AppendStatus::Truncated);
        }
        if self.page_count > 0 {
            if let Some(last) = self.anchor(self.page_count - 1) {
                if page_start <= last {
                    self.mark_complete();
                    return Ok(AppendStatus::Complete);
                }
            }
        }
        if let Err(error) = self.write_anchor(page_start) {
            self.warning = Some(error);
            self.mark_truncated();
            return Ok(AppendStatus::Truncated);
        }
        self.note_page(page_start);
        self.page_count += 1;
        self.next_anchor = next_start;
        if self.next_anchor >= self.source_size {
            self.mark_complete();
        } else {
            self.close_finished_chapters();
        }
        if self.complete {
            Ok(AppendStatus::Complete)
        } else {
            Ok(AppendStatus::Stored)
        }
    }

    #[must_use]
    pub fn anchor(&self, page: usize) -> Option<u64> {
        if page >= self.page_count {
            return None;
        }
        match &self.storage {
            AnchorStorage::Ram { anchors } => anchors.get(page).copied(),
            AnchorStorage::Sd { path } => {
                let mut file = File::open(path).ok()?;
                read_anchor(&mut file, page)
            }
        }
    }

    /// Page whose anchor contains `offset`, once indexing has reached it.
    #[must_use]
    pub fn page_containing(&self, offset: u64) -> Option<usize> {
        if self.page_count == 0 {
            return None;
        }
        let page = match &self.storage {
            AnchorStorage::Ram { anchors } => {
                search_page(self.page_count, |page| anchors.get(page).copied(), offset)
            }
            AnchorStorage::Sd { path } => {
                let mut file = File::open(path).ok()?;
                search_page(self.page_count, |page| read_anchor(&mut file, page), offset)
            }
        }?;
        let anchor = self.anchor(page)?;
        if anchor > offset {
            return None;
        }
        let covers = self.complete || self.next_anchor > offset;
        if anchor == offset || (anchor < offset && covers) {
            Some(page)
        } else {
            None
        }
    }

    pub fn flush(&mut self, sync: bool) -> Result<(), String> {
        if !self.on_sd() {
            return Ok(());
        }
        self.write_header_and_directory(sync)
    }

    fn can_store_another(&self) -> bool {
        if self.page_count >= self.limits.page_limit.min(u32::MAX as usize) {
            return false;
        }
        match &self.storage {
            AnchorStorage::Ram { anchors } => anchors.len() < self.limits.ram_fallback_limit.max(1),
            AnchorStorage::Sd { .. } => {
                bytes_for_pages(self.page_count + 1) <= self.limits.bytes_limit.max(ANCHOR_BASE)
            }
        }
    }

    fn write_anchor(&mut self, offset: u64) -> Result<(), String> {
        match &mut self.storage {
            AnchorStorage::Ram { anchors } => {
                anchors.push(offset);
                Ok(())
            }
            AnchorStorage::Sd { path } => {
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&*path)
                    .map_err(|error| format!("open {}: {error}", path.display()))?;
                file.seek(SeekFrom::Start((ANCHOR_BASE + self.page_count * 8) as u64))
                    .map_err(|error| format!("seek {}: {error}", path.display()))?;
                file.write_all(&offset.to_le_bytes())
                    .map_err(|error| format!("write {}: {error}", path.display()))?;
                Ok(())
            }
        }
    }

    fn note_page(&mut self, page_start: u64) {
        let page_count = self.page_count;
        let source_size = self.source_size;
        if let Some(chapter) = self.chapters.iter_mut().find(|chapter| {
            page_start >= chapter.text_offset
                && (page_start < chapter.text_end_offset
                    || (page_start == chapter.text_end_offset
                        && chapter.text_end_offset == source_size))
        }) {
            if chapter.first_page.is_none() {
                chapter.first_page = Some(page_count);
            }
            chapter.page_count += 1;
        }
    }

    fn close_finished_chapters(&mut self) {
        for chapter in &mut self.chapters {
            if self.next_anchor >= chapter.text_end_offset {
                chapter.complete = true;
            }
        }
    }

    fn write_header_and_directory(&mut self, sync: bool) -> Result<(), String> {
        let AnchorStorage::Sd { path } = &self.storage else {
            return Ok(());
        };
        let path = path.clone();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        let header = self.header_bytes();
        file.seek(SeekFrom::Start(0))
            .map_err(|error| format!("seek {}: {error}", path.display()))?;
        file.write_all(&header)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        let mut directory = vec![0_u8; CHAPTER_STRIDE * MAX_CHAPTERS];
        for (index, chapter) in self.chapters.iter().take(MAX_CHAPTERS).enumerate() {
            write_chapter(&mut directory[index * CHAPTER_STRIDE..], chapter);
        }
        file.write_all(&directory)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        if sync {
            file.sync_all()
                .map_err(|error| format!("sync {}: {error}", path.display()))?;
        }
        Ok(())
    }

    fn header_bytes(&self) -> [u8; HEADER_LEN] {
        let mut header = [0_u8; HEADER_LEN];
        header[0..4].copy_from_slice(MAGIC);
        header[4..6].copy_from_slice(&VERSION.to_le_bytes());
        let mut flags = 0_u16;
        if self.complete {
            flags |= FLAG_COMPLETE;
        }
        if self.truncated {
            flags |= FLAG_TRUNCATED;
        }
        header[6..8].copy_from_slice(&flags.to_le_bytes());
        header[8..16].copy_from_slice(&self.fingerprint.to_le_bytes());
        header[16..20].copy_from_slice(&(self.page_count as u32).to_le_bytes());
        header[20..28].copy_from_slice(&self.next_anchor.to_le_bytes());
        header[28..30]
            .copy_from_slice(&(self.chapters.len().min(MAX_CHAPTERS) as u16).to_le_bytes());
        header[32..40].copy_from_slice(&self.source_size.to_le_bytes());
        header
    }

    fn read_existing(
        path: &Path,
        fingerprint: u64,
        source_size: u64,
        chapters: &[IndexedChapter],
        limits: IndexLimits,
    ) -> Result<Self, String> {
        let mut file =
            File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
        let mut header = [0_u8; HEADER_LEN];
        file.read_exact(&mut header)
            .map_err(|error| format!("read header: {error}"))?;
        if &header[0..4] != MAGIC {
            return Err("page index magic mismatch".into());
        }
        let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
        if version != VERSION {
            return Err("unsupported page index version".into());
        }
        let flags = u16::from_le_bytes(header[6..8].try_into().unwrap());
        let stored_fingerprint = u64::from_le_bytes(header[8..16].try_into().unwrap());
        if stored_fingerprint != fingerprint {
            return Err("page index fingerprint mismatch".into());
        }
        let mut page_count = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
        let next_anchor = u64::from_le_bytes(header[20..28].try_into().unwrap());
        let chapter_count = u16::from_le_bytes(header[28..30].try_into().unwrap()) as usize;
        let stored_source = u64::from_le_bytes(header[32..40].try_into().unwrap());
        if stored_source != source_size || chapter_count != chapters.len().min(MAX_CHAPTERS) {
            return Err("page index does not match this book".into());
        }
        let file_len = file
            .metadata()
            .map_err(|error| format!("stat {}: {error}", path.display()))?
            .len() as usize;
        let fit = file_len.saturating_sub(ANCHOR_BASE) / 8;
        if page_count > fit {
            page_count = fit;
        }
        let mut directory = vec![0_u8; CHAPTER_STRIDE * MAX_CHAPTERS];
        file.read_exact(&mut directory)
            .map_err(|error| format!("read chapter directory: {error}"))?;
        let mut loaded = Vec::with_capacity(chapter_count);
        for index in 0..chapter_count {
            let record = &directory[index * CHAPTER_STRIDE..(index + 1) * CHAPTER_STRIDE];
            let expected = &chapters[index];
            let parsed = read_chapter(record)?;
            if parsed.chapter_number != expected.chapter_number
                || parsed.text_offset != expected.text_offset
                || parsed.text_end_offset != expected.text_end_offset
            {
                return Err("page index chapter range mismatch".into());
            }
            loaded.push(parsed);
        }
        if loaded
            .iter()
            .map(|chapter| chapter.page_count)
            .sum::<usize>()
            > page_count
        {
            return Err("page index chapter counts exceed stored pages".into());
        }
        let index = Self {
            storage: AnchorStorage::Sd {
                path: path.to_path_buf(),
            },
            fingerprint,
            source_size,
            limits,
            page_count,
            next_anchor,
            complete: flags & FLAG_COMPLETE != 0,
            truncated: flags & FLAG_TRUNCATED != 0,
            chapters: loaded,
            warning: None,
        };
        if page_count > 0 {
            let first = index
                .anchor(0)
                .ok_or_else(|| "missing first page anchor".to_string())?;
            if first > source_size {
                return Err("first page anchor exceeds book".into());
            }
        }
        if page_count > 1 {
            let last = index
                .anchor(page_count - 1)
                .ok_or_else(|| "missing last page anchor".to_string())?;
            let first = index.anchor(0).unwrap_or(0);
            if last <= first || last > source_size {
                return Err("page anchors are not ordered".into());
            }
        }
        Ok(index)
    }
}

fn fresh_chapters(chapters: &[IndexedChapter]) -> Vec<IndexedChapter> {
    chapters
        .iter()
        .map(|chapter| IndexedChapter {
            chapter_number: chapter.chapter_number,
            text_offset: chapter.text_offset,
            text_end_offset: chapter.text_end_offset,
            first_page: None,
            page_count: 0,
            complete: false,
        })
        .collect()
}

fn bytes_for_pages(pages: usize) -> usize {
    ANCHOR_BASE.saturating_add(pages.saturating_mul(8))
}

fn search_page(
    page_count: usize,
    mut anchor_at: impl FnMut(usize) -> Option<u64>,
    offset: u64,
) -> Option<usize> {
    let mut lo = 0_usize;
    let mut hi = page_count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let anchor = anchor_at(mid)?;
        if anchor <= offset {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.checked_sub(1)
}

fn read_anchor(file: &mut File, page: usize) -> Option<u64> {
    file.seek(SeekFrom::Start((ANCHOR_BASE + page * 8) as u64))
        .ok()?;
    let mut buf = [0_u8; 8];
    file.read_exact(&mut buf).ok()?;
    Some(u64::from_le_bytes(buf))
}

fn write_chapter(record: &mut [u8], chapter: &IndexedChapter) {
    record[0..2].copy_from_slice(&(chapter.chapter_number as u16).to_le_bytes());
    let flags = if chapter.complete {
        CHAPTER_COMPLETE
    } else {
        0
    };
    record[2..4].copy_from_slice(&flags.to_le_bytes());
    record[4..12].copy_from_slice(&chapter.text_offset.to_le_bytes());
    record[12..20].copy_from_slice(&chapter.text_end_offset.to_le_bytes());
    let first = chapter
        .first_page
        .map(|page| page as u32)
        .unwrap_or(NO_PAGE);
    record[20..24].copy_from_slice(&first.to_le_bytes());
    record[24..28].copy_from_slice(&(chapter.page_count as u32).to_le_bytes());
}

fn read_chapter(record: &[u8]) -> Result<IndexedChapter, String> {
    if record.len() < CHAPTER_STRIDE {
        return Err("short chapter record".into());
    }
    let chapter_number = u16::from_le_bytes(record[0..2].try_into().unwrap()) as usize;
    let flags = u16::from_le_bytes(record[2..4].try_into().unwrap());
    let text_offset = u64::from_le_bytes(record[4..12].try_into().unwrap());
    let text_end_offset = u64::from_le_bytes(record[12..20].try_into().unwrap());
    let first_raw = u32::from_le_bytes(record[20..24].try_into().unwrap());
    let page_count = u32::from_le_bytes(record[24..28].try_into().unwrap()) as usize;
    Ok(IndexedChapter {
        chapter_number,
        text_offset,
        text_end_offset,
        first_page: if first_raw == NO_PAGE {
            None
        } else {
            Some(first_raw as usize)
        },
        page_count,
        complete: flags & CHAPTER_COMPLETE != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(pages: usize, bytes: usize, ram: usize) -> IndexLimits {
        IndexLimits {
            page_limit: pages,
            bytes_limit: bytes,
            ram_fallback_limit: ram,
        }
    }

    fn one_chapter(end: u64) -> Vec<IndexedChapter> {
        vec![IndexedChapter {
            chapter_number: 1,
            text_offset: 0,
            text_end_offset: end,
            first_page: None,
            page_count: 0,
            complete: false,
        }]
    }

    fn temp_index(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("rustmix-epub-index-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root.join("ABCDEF01.EPI")
    }

    #[test]
    fn sd_index_round_trips_anchors_without_keeping_them_in_ram() {
        let path = temp_index("roundtrip");
        let source = 10_000_u64;
        let mut index = EpubAnchorIndex::create(
            &path,
            0xabc,
            source,
            &one_chapter(source),
            limits(10_000, bytes_for_pages(10_000), 32),
        )
        .unwrap();
        for page in 0..300 {
            let start = page * 10;
            let status = index.append(start, start + 10).unwrap();
            assert_eq!(status, AppendStatus::Stored);
        }
        index.flush(true).unwrap();
        assert!(index.on_sd());
        assert_eq!(index.page_count(), 300);
        assert_eq!(index.anchor(0), Some(0));
        assert_eq!(index.anchor(299), Some(2990));
        assert_eq!(index.page_containing(2990), Some(299));
        assert_eq!(index.page_containing(2995), Some(299));
        assert_eq!(index.page_containing(5_000), None);

        let loaded = EpubAnchorIndex::load(
            &path,
            0xabc,
            source,
            &one_chapter(source),
            limits(10_000, bytes_for_pages(10_000), 32),
        )
        .unwrap()
        .unwrap();
        assert_eq!(loaded.page_count(), 300);
        assert_eq!(loaded.anchor(150), Some(1500));
        assert_eq!(loaded.page_containing(1500), Some(150));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn page_and_byte_caps_truncate_without_growing_the_file() {
        let path = temp_index("caps");
        let source = 10_000_u64;
        let byte_cap = bytes_for_pages(3);
        let mut index = EpubAnchorIndex::create(
            &path,
            7,
            source,
            &one_chapter(source),
            limits(100, byte_cap, 100),
        )
        .unwrap();
        let mut truncated = false;
        for page in 0..10 {
            let start = page * 10;
            if index.append(start, start + 10).unwrap() == AppendStatus::Truncated {
                truncated = true;
                break;
            }
        }
        index.flush(true).unwrap();
        assert!(truncated);
        assert!(index.is_truncated());
        assert_eq!(index.page_count(), 3);
        let len = fs::metadata(&path).unwrap().len();
        assert!(len <= byte_cap as u64);

        let mut ram = EpubAnchorIndex::ram_only(
            1,
            source,
            &one_chapter(source),
            limits(100, byte_cap, 4),
            "sd unavailable".into(),
        );
        let mut saw_cap = false;
        for page in 0..10 {
            let start = page * 10;
            if ram.append(start, start + 10).unwrap() == AppendStatus::Truncated {
                saw_cap = true;
                break;
            }
        }
        assert!(saw_cap);
        assert_eq!(ram.page_count(), 4);
        assert!(!ram.on_sd());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reaching_the_source_end_marks_the_chapter_complete() {
        let path = temp_index("complete");
        let mut index = EpubAnchorIndex::create(
            &path,
            1,
            30,
            &one_chapter(30),
            limits(100, bytes_for_pages(100), 8),
        )
        .unwrap();
        assert_eq!(index.append(0, 10).unwrap(), AppendStatus::Stored);
        assert!(!index.chapters()[0].complete);
        assert_eq!(index.append(10, 20).unwrap(), AppendStatus::Stored);
        assert_eq!(index.append(20, 30).unwrap(), AppendStatus::Complete);
        assert!(index.is_complete());
        assert!(index.chapters()[0].complete);
        assert_eq!(index.chapters()[0].page_count, 3);
        assert_eq!(index.page_containing(20), Some(2));
        assert_eq!(index.page_containing(25), Some(2));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
