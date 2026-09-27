//! RMXWLS1 word-list reader.

use anyhow::{bail, Result};

use super::format::crc32;

pub const MAGIC: &[u8; 8] = b"RMXWLS1\0";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WordListFile {
    pub dict_id: String,
    pub title: String,
    pub entry_ids: Vec<u32>,
}

pub fn parse_wordlist(data: &[u8]) -> Result<WordListFile> {
    if data.len() < 16 || &data[..8] != MAGIC {
        bail!("bad word list magic");
    }
    let version = u16::from_le_bytes(data[8..10].try_into().unwrap());
    if version != 1 {
        bail!("unsupported word list version");
    }
    let mut offset = 12usize;
    if offset >= data.len() {
        bail!("truncated word list");
    }
    let dict_len = data[offset] as usize;
    offset += 1;
    if offset + dict_len >= data.len() {
        bail!("bad dict id");
    }
    let dict_id = std::str::from_utf8(&data[offset..offset + dict_len])
        .map_err(|_| anyhow::anyhow!("dict id is not utf-8"))?
        .to_string();
    offset += dict_len;
    if offset >= data.len() {
        bail!("bad title");
    }
    let title_len = data[offset] as usize;
    offset += 1;
    if offset + title_len + 4 > data.len() {
        bail!("bad title");
    }
    let title = std::str::from_utf8(&data[offset..offset + title_len])
        .map_err(|_| anyhow::anyhow!("title is not utf-8"))?
        .to_string();
    offset += title_len;
    if offset + 4 > data.len() {
        bail!("truncated word list");
    }
    let count = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;
    let end = offset
        .checked_add(
            count
                .checked_mul(4)
                .ok_or_else(|| anyhow::anyhow!("count overflow"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("count overflow"))?;
    if end + 4 != data.len() {
        bail!("word list length mismatch");
    }
    let mut entry_ids = Vec::with_capacity(count);
    for index in 0..count {
        let start = offset + index * 4;
        entry_ids.push(u32::from_le_bytes(
            data[start..start + 4].try_into().unwrap(),
        ));
    }
    let expected = crc32(&data[..end]);
    let actual = u32::from_le_bytes(data[end..end + 4].try_into().unwrap());
    if expected != actual {
        bail!("bad word list crc");
    }
    Ok(WordListFile {
        dict_id,
        title,
        entry_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_wordlist;

    #[test]
    fn mini_wordlist_round_trip_ids() {
        let parsed =
            parse_wordlist(include_bytes!("../../tests/fixtures/lexicon/MINI.WLS")).unwrap();
        assert_eq!(parsed.dict_id, "MINI");
        assert_eq!(parsed.title, "Mini deck");
        assert_eq!(parsed.entry_ids, vec![0, 1, 4, 5]);
    }

    #[test]
    fn bad_magic_and_crc() {
        let mut data = include_bytes!("../../tests/fixtures/lexicon/MINI.WLS").to_vec();
        data[0] = b'X';
        assert!(parse_wordlist(&data).is_err());
        let mut data = include_bytes!("../../tests/fixtures/lexicon/MINI.WLS").to_vec();
        let last = data.len() - 1;
        data[last] ^= 0xFF;
        assert!(parse_wordlist(&data).is_err());
    }

    #[test]
    fn truncated_wordlist_errors() {
        assert!(parse_wordlist(b"RMXWLS1").is_err());
    }
}
