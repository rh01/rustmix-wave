//! Bounded glyph bitmap cache. Large allocations land in PSRAM on ESP32-S3
//! because `CONFIG_SPIRAM_USE_MALLOC` is enabled.

use super::{FaceId, RasterGlyph};

const DEFAULT_MAX_BYTES: usize = 512 * 1024;
const DEFAULT_MAX_ENTRIES: usize = 768;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct CacheKey {
    pub face: FaceId,
    pub px: u8,
    pub character: char,
}

#[derive(Clone, Debug)]
struct Entry {
    key: CacheKey,
    glyph: RasterGlyph,
}

#[derive(Debug)]
pub struct GlyphCache {
    entries: Vec<Entry>,
    bytes: usize,
    max_bytes: usize,
    max_entries: usize,
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES)
    }
}

impl GlyphCache {
    #[must_use]
    pub fn new(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: Vec::new(),
            bytes: 0,
            max_bytes,
            max_entries,
        }
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn get(&mut self, key: CacheKey) -> Option<RasterGlyph> {
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        let entry = self.entries.remove(index);
        let glyph = entry.glyph.clone();
        self.entries.push(entry);
        Some(glyph)
    }

    pub fn insert(&mut self, key: CacheKey, glyph: RasterGlyph) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            let old = self.entries.remove(index);
            self.bytes = self.bytes.saturating_sub(old.glyph.bitmap.len());
        }
        while !self.entries.is_empty()
            && (self.entries.len() >= self.max_entries
                || self.bytes.saturating_add(glyph.bitmap.len()) > self.max_bytes)
        {
            let old = self.entries.remove(0);
            self.bytes = self.bytes.saturating_sub(old.glyph.bitmap.len());
        }
        self.bytes = self.bytes.saturating_add(glyph.bitmap.len());
        self.entries.push(Entry { key, glyph });
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheKey, GlyphCache};
    use crate::fonts::{FaceId, RasterGlyph};

    fn sample(width: u8) -> RasterGlyph {
        RasterGlyph {
            width,
            height: width,
            advance: width,
            left: 0,
            top: -i8::try_from(width.saturating_sub(2)).unwrap_or(1),
            bitmap: vec![0xFF; ((usize::from(width) + 7) / 8) * usize::from(width)],
        }
    }

    #[test]
    fn evicts_oldest_entries_when_byte_budget_is_exceeded() {
        let mut cache = GlyphCache::new(64, 8);
        cache.insert(
            CacheKey {
                face: FaceId::Unifont,
                px: 16,
                character: '中',
            },
            sample(16),
        );
        cache.insert(
            CacheKey {
                face: FaceId::Unifont,
                px: 16,
                character: '文',
            },
            sample(16),
        );
        assert!(cache.len() <= 2);
        assert!(cache.bytes() <= 64);
    }
}
