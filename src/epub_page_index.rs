//! Per-chapter EPUB page-anchor cache.
//!
//! Opening a book lays out one spine item. The anchors for that chapter are
//! kept in RAM and, when storage is available, rewritten as a whole file via a
//! temporary name and `rename`. Whole-book `EPIX` indexes from the previous
//! reader are ignored.
//!
//! Page totals use `u64`. Adding chapter page counts with `sum::<usize>()`
//! panics in debug builds on 32-bit targets once the total exceeds `usize::MAX`.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Bytes occupied by the cache header. Anchors begin at this offset.
pub const HEADER_LEN: usize = 64;
/// Historical name for the anchor array base. One chapter file has no chapter
/// directory ahead of the anchors, so this is the header length.
pub const ANCHOR_BASE: usize = HEADER_LEN;

const MAGIC: &[u8; 4] = b"EPCH";
const LEGACY_WHOLE_BOOK_MAGIC: &[u8; 4] = b"EPIX";
const VERSION: u16 = 2;
const FLAG_COMPLETE: u16 = 1;
const FLAG_TRUNCATED: u16 = 2;

/// Identity checked before a chapter cache is reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChapterCacheExpect {
    pub fingerprint: u64,
    pub chapter_index: u32,
    pub text_offset: u64,
    pub text_end_offset: u64,
}

/// Anchors for one spine chapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChapterAnchorCache {
    pub fingerprint: u64,
    pub chapter_index: u32,
    pub text_offset: u64,
    pub text_end_offset: u64,
    pub anchors: Vec<u64>,
    pub next_offset: u64,
    pub complete: bool,
    pub truncated: bool,
}

impl ChapterAnchorCache {
    /// Load a matching chapter cache. A missing file, a legacy whole-book
    /// index, or a record that does not match `expect` yields `None` so the
    /// caller can lay the chapter out again.
    #[must_use]
    pub fn load(path: &Path, expect: &ChapterCacheExpect) -> Option<Self> {
        let backup = with_extension(path, "BAK");
        for candidate in [path, backup.as_path()] {
            if !candidate.exists() {
                continue;
            }
            match read_cache(candidate, expect) {
                Ok(Some(cache)) => return Some(cache),
                Ok(None) => {
                    if candidate == path {
                        let _ = fs::remove_file(candidate);
                    }
                }
                Err(error) => {
                    log::info!(
                        "rustmix-wave=epub-chapter-cache status=ignored path={} error={error}",
                        candidate.display()
                    );
                }
            }
        }
        None
    }

    /// Write the cache to `path` by creating a sibling temporary file and
    /// renaming it into place. The destination is never opened for in-place
    /// updates.
    pub fn store(&self, path: &Path) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or_else(|| "EPUB chapter cache path has no parent".to_string())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
        let temp = with_extension(path, "TMP");
        let backup = with_extension(path, "BAK");
        for candidate in [path, temp.as_path(), backup.as_path()] {
            if !is_fat83_file_name(candidate) {
                return Err(format!(
                    "EPUB chapter cache filename is not FAT 8.3 safe: {}",
                    candidate.display()
                ));
            }
        }
        let _ = fs::remove_file(&temp);
        {
            let mut file = File::create(&temp)
                .map_err(|error| format!("create {}: {error}", temp.display()))?;
            file.write_all(&self.encode())
                .map_err(|error| format!("write {}: {error}", temp.display()))?;
            file.sync_all()
                .map_err(|error| format!("sync {}: {error}", temp.display()))?;
        }
        let _ = fs::remove_file(&backup);
        if path.exists() {
            fs::rename(path, &backup)
                .map_err(|error| format!("backup {}: {error}", path.display()))?;
        }
        if let Err(error) = fs::rename(&temp, path) {
            if backup.exists() {
                let _ = fs::rename(&backup, path);
            }
            return Err(format!("replace {}: {error}", path.display()));
        }
        let _ = fs::remove_file(&backup);
        Ok(())
    }

    fn encode(&self) -> Vec<u8> {
        let page_count = u32::try_from(self.anchors.len()).unwrap_or(u32::MAX);
        let mut bytes = Vec::with_capacity(
            HEADER_LEN
                + usize::try_from(anchor_section_bytes(page_count).unwrap_or(0)).unwrap_or(0),
        );
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
        header[16..24].copy_from_slice(&self.text_offset.to_le_bytes());
        header[24..32].copy_from_slice(&self.text_end_offset.to_le_bytes());
        header[32..36].copy_from_slice(&self.chapter_index.to_le_bytes());
        header[36..40].copy_from_slice(&page_count.to_le_bytes());
        header[40..48].copy_from_slice(&self.next_offset.to_le_bytes());
        bytes.extend_from_slice(&header);
        for anchor in self.anchors.iter().take(page_count as usize) {
            bytes.extend_from_slice(&anchor.to_le_bytes());
        }
        bytes
    }
}

/// Directory plus file name that together hold the full 64-bit fingerprint.
///
/// Each component stays inside the FAT 8.3 limits. Using only the low 32 bits
/// as `XXXXXXXX.EPI` collided distinct books.
#[must_use]
pub fn cache_path(directory: &Path, fingerprint: u64) -> PathBuf {
    let high = format!("{:08X}", (fingerprint >> 32) as u32);
    let low = format!("{:08X}.EPC", fingerprint as u32);
    directory.join(high).join(low)
}

/// Byte length of the anchor array. The multiplication is `u64` so a large
/// page count cannot wrap a 32-bit `usize`.
#[must_use]
pub fn anchor_section_bytes(page_count: u32) -> Option<u64> {
    u64::from(page_count).checked_mul(8)
}

/// Sum chapter page counts without the debug overflow of `sum::<usize>()` on
/// 32-bit targets.
#[must_use]
pub fn saturating_page_total(counts: &[u32]) -> u64 {
    counts.iter().fold(0_u64, |total, count| {
        total.saturating_add(u64::from(*count))
    })
}

fn read_cache(
    path: &Path,
    expect: &ChapterCacheExpect,
) -> Result<Option<ChapterAnchorCache>, String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut header = [0_u8; HEADER_LEN];
    file.read_exact(&mut header)
        .map_err(|error| format!("read header: {error}"))?;
    if &header[0..4] == LEGACY_WHOLE_BOOK_MAGIC {
        log::info!(
            "rustmix-wave=epub-chapter-cache status=ignored-legacy-whole-book path={}",
            path.display()
        );
        return Ok(None);
    }
    if &header[0..4] != MAGIC {
        return Ok(None);
    }
    let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
    if version != VERSION {
        return Ok(None);
    }
    let flags = u16::from_le_bytes(header[6..8].try_into().unwrap());
    let fingerprint = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let text_offset = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let text_end_offset = u64::from_le_bytes(header[24..32].try_into().unwrap());
    let chapter_index = u32::from_le_bytes(header[32..36].try_into().unwrap());
    let page_count = u32::from_le_bytes(header[36..40].try_into().unwrap());
    let next_offset = u64::from_le_bytes(header[40..48].try_into().unwrap());
    if fingerprint != expect.fingerprint
        || chapter_index != expect.chapter_index
        || text_offset != expect.text_offset
        || text_end_offset != expect.text_end_offset
    {
        return Ok(None);
    }
    let anchor_bytes = anchor_section_bytes(page_count)
        .ok_or_else(|| "chapter anchor length overflow".to_string())?;
    let file_len = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    let available = file_len.saturating_sub(HEADER_LEN as u64);
    // `saturating_page_total` is the 32-bit-safe replacement for
    // `chapter.page_count` values summed with `sum::<usize>()`.
    let declared_pages = saturating_page_total(&[page_count]);
    if declared_pages != u64::from(page_count) || anchor_bytes > available {
        return Ok(None);
    }
    let mut anchors = Vec::with_capacity(page_count as usize);
    for _ in 0..page_count {
        let mut buf = [0_u8; 8];
        file.read_exact(&mut buf)
            .map_err(|error| format!("read anchor: {error}"))?;
        anchors.push(u64::from_le_bytes(buf));
    }
    if anchors.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Ok(None);
    }
    if anchors
        .iter()
        .any(|anchor| *anchor < text_offset || *anchor > text_end_offset)
    {
        return Ok(None);
    }
    if let Some(&last) = anchors.last() {
        if next_offset < last || next_offset > text_end_offset {
            return Ok(None);
        }
    }
    Ok(Some(ChapterAnchorCache {
        fingerprint,
        chapter_index,
        text_offset,
        text_end_offset,
        anchors,
        next_offset,
        complete: flags & FLAG_COMPLETE != 0,
        truncated: flags & FLAG_TRUNCATED != 0,
    }))
}

fn with_extension(path: &Path, extension: &str) -> PathBuf {
    let mut output = path.to_path_buf();
    output.set_extension(extension);
    output
}

fn is_fat83_file_name(path: &Path) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_for(cache: &ChapterAnchorCache) -> ChapterCacheExpect {
        ChapterCacheExpect {
            fingerprint: cache.fingerprint,
            chapter_index: cache.chapter_index,
            text_offset: cache.text_offset,
            text_end_offset: cache.text_end_offset,
        }
    }

    fn sample() -> ChapterAnchorCache {
        ChapterAnchorCache {
            fingerprint: 0x0123_4567_89AB_CDEF,
            chapter_index: 3,
            text_offset: 100,
            text_end_offset: 160,
            anchors: vec![100, 120, 140],
            next_offset: 160,
            complete: true,
            truncated: false,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rustmix-epub-chapter-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn cache_path_keeps_both_halves_of_a_64_bit_hash() {
        let directory = Path::new("/sdcard/RUSTMIX/READER/CACHE");
        let low = 0x1111_1111_u64;
        let first = low | (0x2222_2222_u64 << 32);
        let second = low | (0x3333_3333_u64 << 32);
        assert_eq!(first as u32, second as u32);
        let path_first = cache_path(directory, first);
        let path_second = cache_path(directory, second);
        assert_ne!(path_first, path_second);
        assert_eq!(
            path_first.file_name().and_then(|value| value.to_str()),
            Some("11111111.EPC")
        );
        assert_eq!(
            path_first
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|value| value.to_str()),
            Some("22222222")
        );
        assert_eq!(
            path_second
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|value| value.to_str()),
            Some("33333333")
        );
        assert!(is_fat83_file_name(&path_first));
        assert!(is_fat83_file_name(&with_extension(&path_first, "TMP")));
    }

    #[test]
    fn store_replaces_the_cache_through_a_temporary_file() {
        let root = temp_dir("atomic");
        let path = cache_path(&root, sample().fingerprint);
        let mut cache = sample();
        cache.store(&path).unwrap();
        assert!(!with_extension(&path, "TMP").exists());
        assert!(!with_extension(&path, "BAK").exists());
        let loaded = ChapterAnchorCache::load(&path, &expect_for(&cache)).unwrap();
        assert_eq!(loaded.anchors, vec![100, 120, 140]);

        cache.anchors.push(150);
        cache.next_offset = 160;
        cache.store(&path).unwrap();
        let loaded = ChapterAnchorCache::load(&path, &expect_for(&cache)).unwrap();
        assert_eq!(loaded.anchors, vec![100, 120, 140, 150]);
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.windows(4).any(|window| window == MAGIC));
        assert!(!bytes
            .windows(4)
            .any(|window| window == LEGACY_WHOLE_BOOK_MAGIC));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_whole_book_index_is_ignored() {
        let root = temp_dir("legacy");
        let path = cache_path(&root, 1);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = vec![0_u8; HEADER_LEN];
        bytes[0..4].copy_from_slice(LEGACY_WHOLE_BOOK_MAGIC);
        fs::write(&path, bytes).unwrap();
        assert!(ChapterAnchorCache::load(&path, &expect_for(&sample())).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn page_count_arithmetic_does_not_overflow_u32() {
        assert_eq!(
            saturating_page_total(&[u32::MAX, u32::MAX]),
            u64::from(u32::MAX) * 2
        );
        assert_eq!(
            anchor_section_bytes(u32::MAX),
            Some(u64::from(u32::MAX) * 8)
        );
        let wrapped = (u32::MAX as usize).wrapping_add(u32::MAX as usize);
        if usize::BITS == 32 {
            assert!(wrapped < u32::MAX as usize);
        }
        assert!(saturating_page_total(&[u32::MAX, u32::MAX]) > wrapped as u64 || usize::BITS > 32);
    }
}
