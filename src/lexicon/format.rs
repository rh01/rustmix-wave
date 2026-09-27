//! RMXLEX1 reader. Every out-of-range length returns an error instead of panicking.

use anyhow::{bail, Result};

pub const PAGE_SIZE: usize = 4096;
pub const HEADER_LEN: usize = 64;
pub const MAX_KEY_LEN: usize = 64;
pub const MAX_ENTRY_BYTES: usize = 8 * 1024;
pub const MAX_FIELDS: usize = 32;
pub const LEXICON_PAGE_INDEX_MAX_BYTES: usize = 256 * 1024;
pub const MAGIC: &[u8; 8] = b"RMXLEX1\0";

pub const FIELD_HEADWORD: u8 = 1;
pub const FIELD_READING: u8 = 2;
pub const FIELD_PHONETIC: u8 = 3;
pub const FIELD_POS: u8 = 4;
pub const FIELD_DEF_ZH: u8 = 5;
pub const FIELD_DEF_EN: u8 = 6;
pub const FIELD_EXAMPLE: u8 = 7;
pub const FIELD_FORMS: u8 = 8;
pub const FIELD_KANJI_INFO: u8 = 9;
pub const FIELD_NOTE: u8 = 10;

/// IEEE CRC-32, the same polynomial as `zlib.crc32`.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    pub flags: u16,
    pub entry_count: u32,
    pub key_count: u32,
    pub page_size: u32,
    pub page_count: u32,
    pub pidx_off: u32,
    pub pidx_len: u32,
    pub keys_off: u32,
    pub etab_off: u32,
    pub ent_off: u32,
    pub ent_len: u32,
    pub src_lang: [u8; 4],
    pub dst_lang: [u8; 4],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyRecord {
    pub key: Vec<u8>,
    pub entry_id: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Field {
    pub id: u8,
    pub bytes: Vec<u8>,
}

impl Field {
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryRecord {
    pub tags: u32,
    pub freq_rank: u32,
    pub flags: u8,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageIndexEntry {
    pub first_key: Vec<u8>,
    pub offset: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedLexicon {
    pub header: Header,
    pub pages: Vec<PageIndexEntry>,
    pub keys: Vec<KeyRecord>,
    pub entries: Vec<EntryRecord>,
}

pub fn parse_header(data: &[u8]) -> Result<Header> {
    if data.len() < HEADER_LEN {
        bail!("truncated header");
    }
    if &data[..8] != MAGIC {
        bail!("bad magic");
    }
    let version = u16::from_le_bytes(data[8..10].try_into().unwrap());
    if version != 1 {
        bail!("unsupported version");
    }
    let expected = crc32(&data[..60]);
    let actual = u32::from_le_bytes(data[60..64].try_into().unwrap());
    if expected != actual {
        bail!("bad header crc");
    }
    let flags = u16::from_le_bytes(data[10..12].try_into().unwrap());
    let entry_count = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let key_count = u32::from_le_bytes(data[16..20].try_into().unwrap());
    let page_size = u32::from_le_bytes(data[20..24].try_into().unwrap());
    let page_count = u32::from_le_bytes(data[24..28].try_into().unwrap());
    let pidx_off = u32::from_le_bytes(data[28..32].try_into().unwrap());
    let pidx_len = u32::from_le_bytes(data[32..36].try_into().unwrap());
    let keys_off = u32::from_le_bytes(data[36..40].try_into().unwrap());
    let etab_off = u32::from_le_bytes(data[40..44].try_into().unwrap());
    let ent_off = u32::from_le_bytes(data[44..48].try_into().unwrap());
    let ent_len = u32::from_le_bytes(data[48..52].try_into().unwrap());
    if page_size != PAGE_SIZE as u32 {
        bail!("unexpected page size");
    }
    if pidx_len as usize > LEXICON_PAGE_INDEX_MAX_BYTES {
        bail!("page index exceeds 256 KiB");
    }
    let mut src_lang = [0; 4];
    let mut dst_lang = [0; 4];
    src_lang.copy_from_slice(&data[52..56]);
    dst_lang.copy_from_slice(&data[56..60]);
    Ok(Header {
        flags,
        entry_count,
        key_count,
        page_size,
        page_count,
        pidx_off,
        pidx_len,
        keys_off,
        etab_off,
        ent_off,
        ent_len,
        src_lang,
        dst_lang,
    })
}

pub fn parse_lexicon(data: &[u8]) -> Result<ParsedLexicon> {
    let header = parse_header(data)?;
    need(data, header.pidx_off, header.pidx_len)?;
    need(
        data,
        header.keys_off,
        header
            .page_count
            .checked_mul(PAGE_SIZE as u32)
            .ok_or_else(|| anyhow::anyhow!("page span overflow"))?,
    )?;
    need(
        data,
        header.etab_off,
        header
            .entry_count
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("entry table overflow"))?,
    )?;
    need(data, header.ent_off, header.ent_len)?;
    let index = &data[header.pidx_off as usize..(header.pidx_off + header.pidx_len) as usize];
    let pages = parse_page_index(index, header.page_count, header.keys_off)?;
    let mut keys = Vec::new();
    for page in &pages {
        let start = page.offset as usize;
        let bytes = data
            .get(start..start + PAGE_SIZE)
            .ok_or_else(|| anyhow::anyhow!("key page out of range"))?;
        let records = parse_key_page(bytes)?;
        if records.first().map(|item| item.key.as_slice()) != Some(page.first_key.as_slice()) {
            bail!("page index first key mismatch");
        }
        keys.extend(records);
    }
    if keys.len() != header.key_count as usize {
        bail!("key count mismatch");
    }
    for pair in keys.windows(2) {
        if pair[1].key < pair[0].key {
            bail!("keys are not ordered");
        }
    }
    let mut entries = Vec::with_capacity(header.entry_count as usize);
    for index in 0..header.entry_count {
        let rel = read_u32(data, header.etab_off as usize + index as usize * 4)?;
        let end = if index + 1 < header.entry_count {
            read_u32(data, header.etab_off as usize + (index as usize + 1) * 4)?
        } else {
            header.ent_len
        };
        if rel > end || end > header.ent_len {
            bail!("entry offset out of range");
        }
        let span = (end - rel) as usize;
        if span > MAX_ENTRY_BYTES {
            bail!("entry exceeds 8 KiB");
        }
        let start = header.ent_off as usize + rel as usize;
        let blob = data
            .get(start..start + span)
            .ok_or_else(|| anyhow::anyhow!("entry out of range"))?;
        entries.push(parse_entry(blob)?);
    }
    Ok(ParsedLexicon {
        header,
        pages,
        keys,
        entries,
    })
}

pub fn parse_page_index(
    blob: &[u8],
    page_count: u32,
    keys_off: u32,
) -> Result<Vec<PageIndexEntry>> {
    let mut pages = Vec::with_capacity(page_count as usize);
    let mut offset = 0;
    for index in 0..page_count {
        if offset >= blob.len() {
            bail!("truncated page index");
        }
        let key_len = blob[offset] as usize;
        offset += 1;
        if key_len == 0 || key_len > MAX_KEY_LEN || offset + key_len + 4 > blob.len() {
            bail!("bad page index key");
        }
        let first_key = blob[offset..offset + key_len].to_vec();
        offset += key_len;
        let page_offset = u32::from_le_bytes(blob[offset..offset + 4].try_into().unwrap());
        offset += 4;
        let expected = keys_off
            .checked_add(
                index
                    .checked_mul(PAGE_SIZE as u32)
                    .ok_or_else(|| anyhow::anyhow!("page offset overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("page offset overflow"))?;
        if page_offset != expected {
            bail!("page offset mismatch");
        }
        pages.push(PageIndexEntry {
            first_key,
            offset: page_offset,
        });
    }
    if offset != blob.len() {
        bail!("page index trailing bytes");
    }
    Ok(pages)
}

pub fn parse_key_page(page: &[u8]) -> Result<Vec<KeyRecord>> {
    if page.len() != PAGE_SIZE {
        bail!("short key page");
    }
    let count = u16::from_le_bytes(page[0..2].try_into().unwrap()) as usize;
    let mut offset = 2;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        if offset >= PAGE_SIZE {
            bail!("record crosses page");
        }
        let key_len = page[offset] as usize;
        offset += 1;
        if key_len == 0 || key_len > MAX_KEY_LEN {
            bail!("key_len out of range");
        }
        let rec_end = offset + key_len + 4;
        if rec_end > PAGE_SIZE {
            bail!("record crosses page");
        }
        let key = page[offset..offset + key_len].to_vec();
        offset += key_len;
        let entry_id = u32::from_le_bytes(page[offset..offset + 4].try_into().unwrap());
        offset += 4;
        records.push(KeyRecord { key, entry_id });
    }
    Ok(records)
}

pub fn parse_entry(blob: &[u8]) -> Result<EntryRecord> {
    if blob.len() < 10 || blob.len() > MAX_ENTRY_BYTES {
        bail!("entry size out of range");
    }
    let tags = u32::from_le_bytes(blob[0..4].try_into().unwrap());
    let freq_rank = u32::from_le_bytes(blob[4..8].try_into().unwrap());
    let flags = blob[8];
    let field_count = blob[9] as usize;
    if field_count > MAX_FIELDS {
        bail!("too many fields");
    }
    let mut offset = 10;
    let mut fields = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        if offset + 3 > blob.len() {
            bail!("truncated field");
        }
        let id = blob[offset];
        let len = u16::from_le_bytes(blob[offset + 1..offset + 3].try_into().unwrap()) as usize;
        offset += 3;
        if offset + len > blob.len() {
            bail!("field exceeds entry");
        }
        fields.push(Field {
            id,
            bytes: blob[offset..offset + len].to_vec(),
        });
        offset += len;
    }
    if offset != blob.len() {
        bail!("entry trailing bytes");
    }
    Ok(EntryRecord {
        tags,
        freq_rank,
        flags,
        fields,
    })
}

fn need(data: &[u8], offset: u32, length: u32) -> Result<()> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| anyhow::anyhow!("offset overflow"))? as usize;
    if offset as usize > data.len() || end > data.len() {
        bail!("offset out of range");
    }
    Ok(())
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow::anyhow!("offset out of range"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::{crc32, parse_entry, parse_key_page, parse_lexicon, PAGE_SIZE};

    fn mini() -> Vec<u8> {
        include_bytes!("../../tests/fixtures/lexicon/MINI.LEX").to_vec()
    }

    #[test]
    fn crc32_matches_zlib_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn mini_lexicon_exact_prefix_and_duplicate_keys() {
        let parsed = parse_lexicon(&mini()).unwrap();
        assert_eq!(parsed.header.entry_count, 6);
        let ids = |key: &str| -> Vec<u32> {
            parsed
                .keys
                .iter()
                .filter(|item| item.key == key.as_bytes())
                .map(|item| item.entry_id)
                .collect()
        };
        assert_eq!(ids("apple"), vec![0]);
        assert_eq!(ids("ねこ"), vec![4]);
        assert_eq!(ids("猫"), vec![4]);
        assert_eq!(ids("nihao"), vec![5]);
        assert_eq!(ids("bank"), vec![2, 3]);
        let prefix: Vec<_> = parsed
            .keys
            .iter()
            .filter(|item| item.key.starts_with(b"app"))
            .map(|item| item.entry_id)
            .collect();
        assert_eq!(prefix, vec![0, 1]);
        assert!(parsed
            .keys
            .windows(2)
            .all(|pair| pair[0].key <= pair[1].key));
    }

    #[test]
    fn bad_magic_is_an_error() {
        let mut data = mini();
        data[0] = b'X';
        assert!(parse_lexicon(&data).is_err());
    }

    #[test]
    fn bad_crc_is_an_error() {
        let mut data = mini();
        data[60] ^= 0xFF;
        assert!(parse_lexicon(&data).is_err());
    }

    #[test]
    fn offset_out_of_range_is_an_error() {
        let mut data = mini();
        data[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
        let sum = crc32(&data[..60]);
        data[60..64].copy_from_slice(&sum.to_le_bytes());
        assert!(parse_lexicon(&data).is_err());
    }

    #[test]
    fn declared_page_index_over_256kib_is_an_error() {
        let mut data = mini();
        data[32..36].copy_from_slice(&(256 * 1024 + 1u32).to_le_bytes());
        let sum = crc32(&data[..60]);
        data[60..64].copy_from_slice(&sum.to_le_bytes());
        assert!(parse_lexicon(&data).is_err());
    }

    #[test]
    fn key_len_over_64_is_an_error() {
        let mut page = vec![0u8; PAGE_SIZE];
        page[0..2].copy_from_slice(&1u16.to_le_bytes());
        page[2] = 65;
        assert!(parse_key_page(&page).is_err());
    }

    #[test]
    fn record_crossing_page_is_an_error() {
        let mut page = vec![0u8; PAGE_SIZE];
        page[0..2].copy_from_slice(&1u16.to_le_bytes());
        page[PAGE_SIZE - 3] = 64;
        assert!(parse_key_page(&page).is_err());
    }

    #[test]
    fn entry_over_8kib_is_an_error() {
        let mut blob = vec![0u8; 8193];
        blob[9] = 1;
        assert!(parse_entry(&blob).is_err());
    }

    #[test]
    fn truncated_field_is_an_error() {
        let mut blob = vec![0u8; 10];
        blob[9] = 1;
        assert!(parse_entry(&blob).is_err());
    }

    #[test]
    fn seeded_mutations_do_not_panic() {
        let original = mini();
        let mut state = 0x1234_5678u32;
        for _ in 0..32 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let mut data = original.clone();
            let index = (state as usize) % data.len();
            data[index] ^= (state >> 16) as u8;
            let _ = parse_lexicon(&data);
        }
    }
}
