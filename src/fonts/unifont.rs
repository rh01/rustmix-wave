//! Embedded GNU Unifont GB2312 16x16 fallback.
//!
//! The packed bitmap is generated from the SIL OFL Unifont `.hex` source. ASCII
//! stays in the existing Latin strikes; this table covers GB2312 plus CJK
//! punctuation and fullwidth forms.

const PACK: &[u8] = include_bytes!("unifont_gb2312.bin");
const MAGIC: &[u8; 4] = b"UGB1";
const HEADER_LEN: usize = 8;
const BITMAP_BYTES: usize = 32;
const NATIVE_PX: u8 = 16;

#[must_use]
pub fn pack_glyph_count() -> usize {
    if PACK.len() < HEADER_LEN || PACK[..4] != MAGIC[..] {
        return 0;
    }
    u32::from_le_bytes(PACK[4..8].try_into().unwrap_or([0; 4])) as usize
}

#[must_use]
pub fn native_bitmap(character: char) -> Option<&'static [u8]> {
    let code = character as u32;
    if code > 0xFFFF {
        return None;
    }
    let count = pack_glyph_count();
    if count == 0 {
        return None;
    }
    let codes_len = count.checked_mul(2)?;
    let codes_end = HEADER_LEN.checked_add(codes_len)?;
    if PACK.len() < codes_end.checked_add(count.checked_mul(BITMAP_BYTES)?)? {
        return None;
    }
    let codes = &PACK[HEADER_LEN..codes_end];
    let index = binary_search_u16(codes, code as u16)?;
    let bitmap_off = codes_end + index * BITMAP_BYTES;
    PACK.get(bitmap_off..bitmap_off + BITMAP_BYTES)
}

#[must_use]
pub fn contains(character: char) -> bool {
    native_bitmap(character).is_some()
}

#[allow(dead_code)]
#[must_use]
pub const fn native_px() -> u8 {
    NATIVE_PX
}

#[must_use]
pub fn bit_at(bitmap: &[u8], x: u8, y: u8) -> bool {
    if x >= NATIVE_PX || y >= NATIVE_PX || bitmap.len() < BITMAP_BYTES {
        return false;
    }
    let byte_index = usize::from(y) * 2 + usize::from(x / 8);
    let mask = 0x80 >> (x % 8);
    bitmap[byte_index] & mask != 0
}

fn binary_search_u16(codes: &[u8], needle: u16) -> Option<usize> {
    let mut lo = 0usize;
    let mut hi = codes.len() / 2;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let value = u16::from_le_bytes(codes[mid * 2..mid * 2 + 2].try_into().ok()?);
        match value.cmp(&needle) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => return Some(mid),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{contains, native_bitmap, pack_glyph_count};

    #[test]
    fn pack_contains_gb2312_ideograph_and_fullwidth_punctuation() {
        assert!(pack_glyph_count() > 6000);
        assert!(contains('中'));
        assert!(contains('文'));
        assert!(contains('，'));
        assert!(native_bitmap('中').is_some());
        assert!(native_bitmap('A').is_none());
    }
}
