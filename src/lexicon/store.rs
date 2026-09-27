//! Bounded lexicon reads: the page index stays in memory, each lookup reads one page.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{bail, Result};

use super::format::{
    parse_entry, parse_header, parse_key_page, parse_page_index, EntryRecord, Header, KeyRecord,
    PageIndexEntry, PAGE_SIZE,
};
use crate::runtime_worker::{run_named_worker, NamedWorkerError};

/// Open dictionaries whose page index exceeds this on a 32 KiB worker stack.
pub const PAGE_INDEX_WORKER_THRESHOLD: usize = 64 * 1024;
const WORKER_STACK: usize = 32 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexiconIndex {
    pub header: Header,
    pub pages: Vec<PageIndexEntry>,
    pub path: String,
}

pub struct LexiconStore<R> {
    reader: R,
    pub index: LexiconIndex,
}

impl LexiconStore<File> {
    pub fn open_path(path: &Path) -> Result<Self> {
        let index = load_index(path)?;
        let reader = File::open(path)?;
        Ok(Self { reader, index })
    }
}

impl<R: Read + Seek> LexiconStore<R> {
    pub fn open(mut reader: R) -> Result<Self> {
        let index = read_index(&mut reader, String::new())?;
        Ok(Self { reader, index })
    }

    pub fn lookup_exact(&mut self, key: &[u8]) -> Result<Vec<u32>> {
        lookup_exact(&mut self.reader, &self.index, key)
    }

    pub fn lookup_prefix(&mut self, key: &[u8], limit: usize) -> Result<Vec<(Vec<u8>, u32)>> {
        lookup_prefix(&mut self.reader, &self.index, key, limit)
    }

    pub fn entry(&mut self, id: u32) -> Result<EntryRecord> {
        read_entry(&mut self.reader, &self.index.header, id)
    }
}

pub fn load_index(path: &Path) -> Result<LexiconIndex> {
    let mut file = File::open(path)?;
    let header = read_header_only(&mut file)?;
    if header.pidx_len as usize > PAGE_INDEX_WORKER_THRESHOLD {
        let path_buf = path.to_path_buf();
        return match run_named_worker("lexicon-open", WORKER_STACK, move || {
            let mut file = File::open(&path_buf).map_err(|err| err.to_string())?;
            read_index(&mut file, path_buf.display().to_string()).map_err(|err| err.to_string())
        }) {
            Ok(index) => Ok(index),
            Err(NamedWorkerError::Operation(error)) => bail!(error),
            Err(error) => bail!("{error}"),
        };
    }
    read_index(&mut file, path.display().to_string())
}

pub fn lookup_exact<R: Read + Seek>(
    reader: &mut R,
    index: &LexiconIndex,
    key: &[u8],
) -> Result<Vec<u32>> {
    let mut found = Vec::new();
    for_matching_pages(reader, index, key, &mut |record| {
        if record.key.as_slice() == key {
            found.push(record.entry_id);
            true
        } else {
            record.key.as_slice() < key
        }
    })?;
    Ok(found)
}

pub fn lookup_prefix<R: Read + Seek>(
    reader: &mut R,
    index: &LexiconIndex,
    prefix: &[u8],
    limit: usize,
) -> Result<Vec<(Vec<u8>, u32)>> {
    let mut found = Vec::new();
    if limit == 0 {
        return Ok(found);
    }
    for_matching_pages(reader, index, prefix, &mut |record| {
        if record.key.starts_with(prefix) {
            found.push((record.key.clone(), record.entry_id));
            found.len() < limit
        } else {
            record.key.as_slice() < prefix
        }
    })?;
    Ok(found)
}

fn for_matching_pages<R, F>(
    reader: &mut R,
    index: &LexiconIndex,
    key: &[u8],
    visit: &mut F,
) -> Result<()>
where
    R: Read + Seek,
    F: FnMut(&KeyRecord) -> bool,
{
    if index.pages.is_empty() {
        return Ok(());
    }
    let start = start_page(&index.pages, key);
    for page in index.pages.iter().skip(start) {
        let records = read_page(reader, page.offset)?;
        if records.first().map(|item| item.key.as_slice()) != Some(page.first_key.as_slice()) {
            bail!("page index first key mismatch");
        }
        for record in &records {
            if !visit(record) {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn start_page(pages: &[PageIndexEntry], key: &[u8]) -> usize {
    let mut lo = 0usize;
    let mut hi = pages.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pages[mid].first_key.as_slice() <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 {
        0
    } else {
        lo - 1
    }
}

fn read_page<R: Read + Seek>(reader: &mut R, offset: u32) -> Result<Vec<KeyRecord>> {
    let mut page = vec![0u8; PAGE_SIZE];
    reader.seek(SeekFrom::Start(u64::from(offset)))?;
    reader.read_exact(&mut page)?;
    parse_key_page(&page)
}

pub fn read_entry<R: Read + Seek>(reader: &mut R, header: &Header, id: u32) -> Result<EntryRecord> {
    if id >= header.entry_count {
        bail!("entry id out of range");
    }
    let rel = read_u32_at(reader, header.etab_off as u64 + u64::from(id) * 4)?;
    let end = if id + 1 < header.entry_count {
        read_u32_at(reader, header.etab_off as u64 + u64::from(id + 1) * 4)?
    } else {
        header.ent_len
    };
    if rel > end || end > header.ent_len {
        bail!("entry offset out of range");
    }
    let span = (end - rel) as usize;
    if span > super::format::MAX_ENTRY_BYTES {
        bail!("entry exceeds 8 KiB");
    }
    let mut blob = vec![0u8; span];
    reader.seek(SeekFrom::Start(header.ent_off as u64 + u64::from(rel)))?;
    reader.read_exact(&mut blob)?;
    parse_entry(&blob)
}

fn read_index<R: Read + Seek>(reader: &mut R, path: String) -> Result<LexiconIndex> {
    let header = read_header_only(reader)?;
    if header.pidx_len as usize > super::format::LEXICON_PAGE_INDEX_MAX_BYTES {
        bail!("page index exceeds 256 KiB");
    }
    let mut blob = vec![0u8; header.pidx_len as usize];
    reader.seek(SeekFrom::Start(u64::from(header.pidx_off)))?;
    reader.read_exact(&mut blob)?;
    let pages = parse_page_index(&blob, header.page_count, header.keys_off)?;
    let end = reader.seek(SeekFrom::End(0))?;
    let need = header.ent_off as u64 + header.ent_len as u64;
    if end < need {
        bail!("offset out of range");
    }
    Ok(LexiconIndex {
        header,
        pages,
        path,
    })
}

fn read_header_only<R: Read + Seek>(reader: &mut R) -> Result<Header> {
    let mut header = [0u8; super::format::HEADER_LEN];
    reader.seek(SeekFrom::Start(0))?;
    reader.read_exact(&mut header)?;
    parse_header(&header)
}

fn read_u32_at<R: Read + Seek>(reader: &mut R, offset: u64) -> Result<u32> {
    let mut buf = [0u8; 4];
    reader.seek(SeekFrom::Start(offset))?;
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{load_index, LexiconStore};
    use crate::lexicon::normalize_key;

    fn cursor() -> Cursor<Vec<u8>> {
        Cursor::new(include_bytes!("../../tests/fixtures/lexicon/MINI.LEX").to_vec())
    }

    #[test]
    fn exact_kana_kanji_pinyin_and_duplicates() {
        let mut store = LexiconStore::open(cursor()).unwrap();
        assert_eq!(store.lookup_exact(b"apple").unwrap(), vec![0]);
        assert_eq!(
            store
                .lookup_exact(normalize_key("ネコ").as_bytes())
                .unwrap(),
            vec![4]
        );
        assert_eq!(store.lookup_exact("猫".as_bytes()).unwrap(), vec![4]);
        assert_eq!(store.lookup_exact(b"nihao").unwrap(), vec![5]);
        assert_eq!(store.lookup_exact(b"bank").unwrap(), vec![2, 3]);
        let entry = store.entry(5).unwrap();
        assert_eq!(entry.fields[0].text(), "你好");
    }

    #[test]
    fn prefix_is_ordered_and_limited() {
        let mut store = LexiconStore::open(cursor()).unwrap();
        let hits = store.lookup_prefix(b"app", 8).unwrap();
        assert_eq!(
            hits.iter()
                .map(|item| item.0.as_slice())
                .collect::<Vec<_>>(),
            vec![b"apple".as_slice(), b"apply".as_slice()]
        );
        assert_eq!(store.lookup_prefix(b"app", 1).unwrap().len(), 1);
        assert!(store.lookup_exact(b"missing").unwrap().is_empty());
    }

    #[test]
    fn missing_entry_id_errors() {
        let mut store = LexiconStore::open(cursor()).unwrap();
        assert!(store.entry(99).is_err());
    }

    #[test]
    fn load_index_from_fixture_path() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/lexicon/MINI.LEX");
        let index = load_index(&path).unwrap();
        assert_eq!(index.header.entry_count, 6);
        assert!(index.header.pidx_len < super::PAGE_INDEX_WORKER_THRESHOLD as u32);
    }

    #[test]
    fn hostile_page_count_header_is_rejected() {
        let mut header = vec![0u8; 64];
        header[..8].copy_from_slice(b"RMXLEX1\0");
        header[8..10].copy_from_slice(&1u16.to_le_bytes());
        header[20..24].copy_from_slice(&4096u32.to_le_bytes());
        header[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        let sum = crate::lexicon::crc32(&header[..60]);
        header[60..64].copy_from_slice(&sum.to_le_bytes());
        let path = std::env::temp_dir().join(format!("rmx-hostile-{}.lex", std::process::id()));
        std::fs::write(&path, &header).unwrap();
        let error = load_index(&path).unwrap_err();
        assert!(error.to_string().contains("page count exceeds index"));
        let _ = std::fs::remove_file(&path);
    }
}
