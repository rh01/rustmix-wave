//! SD pronunciation lookup for Lexicon and Vocabulary.
//!
//! A dictionary directory holds `AUDIO.IDX` (sorted entry ids) and `AUDIO.PAK`
//! (concatenated `RMXADP1` clips). Lookup binary-searches the index and never
//! reads the whole pack. A missing card, index, or entry is a quiet miss.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};

use super::adpcm::{
    clip_span_bytes, decode_clip, AdpcmDecoder, ClipHeader, CLIP_HEADER_LEN, CLIP_MAX_BYTES,
    CLIP_SAMPLE_RATE_HZ,
};
use crate::lexicon::crc32;

pub const INDEX_MAGIC: &[u8; 8] = b"RMXAUD1\0";
pub const INDEX_HEADER_LEN: usize = 20;
pub const INDEX_RECORD_LEN: usize = 12;
/// Word-list packs stay well under this. A corrupt count cannot allocate
/// a multi-megabyte index on the UI path.
pub const INDEX_MAX_BYTES: usize = 256 * 1024;
pub const INDEX_NAME: &str = "AUDIO.IDX";
pub const PACK_NAME: &str = "AUDIO.PAK";

/// Total `AUDIO.IDX` length for `count` records, or `None` when the product
/// does not fit in an unsigned integer of `width_bits` (32 on the ESP32).
#[must_use]
pub fn index_span_bytes(count: u32, width_bits: u32) -> Option<u64> {
    let records = u64::from(count).checked_mul(INDEX_RECORD_LEN as u64)?;
    let body = records.checked_add(INDEX_HEADER_LEN as u64)?;
    let total = body.checked_add(4)?;
    let limit = if width_bits >= 64 {
        u64::MAX
    } else {
        u64::from(u32::MAX)
    };
    if records > limit || body > limit || total > limit {
        None
    } else {
        Some(total)
    }
}

/// `true` when a file length must not be read into memory.
///
/// Compare in `u64`. Narrowing to `usize` first accepts `cap + 2^32` on a
/// 32-bit target because that value truncates back to the cap.
#[must_use]
pub fn index_len_exceeds_cap(len: u64) -> bool {
    len > INDEX_MAX_BYTES as u64
}

/// End offset of one packed clip, or `None` when `length` is outside the clip
/// cap or `offset + length` does not fit in `width_bits`.
#[must_use]
pub fn packed_clip_end(offset: u32, length: u32, width_bits: u32) -> Option<u64> {
    if u64::from(length) < CLIP_HEADER_LEN as u64 || length > CLIP_MAX_BYTES {
        return None;
    }
    let end = u64::from(offset).checked_add(u64::from(length))?;
    let limit = if width_bits >= 64 {
        u64::MAX
    } else {
        u64::from(u32::MAX)
    };
    if end > limit {
        None
    } else {
        Some(end)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipLoc {
    pub entry_id: u32,
    pub offset: u32,
    pub length: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioIndex {
    pub sample_rate: u32,
    pub records: Vec<ClipLoc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PronounceTarget {
    pub dict_id: String,
    pub entry_id: u32,
}

pub struct PronounceSession {
    file: File,
    decoder: AdpcmDecoder,
    samples_left: u32,
}

impl PronounceSession {
    /// Open one clip. `Ok(None)` means the speaker files or this entry are
    /// absent. `Err` is a damaged index or clip, which callers treat as silence.
    pub fn open(lexicon_root: &Path, dict_id: &str, entry_id: u32) -> Result<Option<Self>> {
        let Some(loc) = lookup_clip(lexicon_root, dict_id, entry_id)? else {
            return Ok(None);
        };
        let Some(pak) = pack_path(lexicon_root, dict_id) else {
            return Ok(None);
        };
        let mut file = match File::open(&pak) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let header = read_clip_header(&mut file, loc)?;
        Ok(Some(Self {
            file,
            decoder: AdpcmDecoder::from_header(&header),
            samples_left: header.sample_count,
        }))
    }

    /// Read the next PCM16 mono chunk. `Ok(0)` means the clip is finished.
    pub fn read_pcm16_mono(&mut self, buffer: &mut [u8]) -> Result<usize> {
        if buffer.len() < 2 {
            return Err(anyhow!("pronounce buffer is too small"));
        }
        let mut produced = 0_usize;
        let capacity = buffer.len() / 2;
        while produced < capacity && self.samples_left > 0 {
            let mut byte = [0_u8; 1];
            self.file
                .read_exact(&mut byte)
                .map_err(|error| anyhow!("pronounce read failed: {error}"))?;
            let mut pair = [0_i16; 2];
            let count = self.decoder.pull_byte(byte[0], &mut pair);
            for sample in pair.into_iter().take(count) {
                if produced >= capacity {
                    break;
                }
                let start = produced * 2;
                buffer[start..start + 2].copy_from_slice(&sample.to_le_bytes());
                produced += 1;
                self.samples_left = self.samples_left.saturating_sub(1);
            }
        }
        Ok(produced * 2)
    }
}

/// `Ok(true)` only when a clip can be opened. Missing files and damaged
/// packs both return false so the UI can stay quiet.
#[must_use]
pub fn pronounce_available(lexicon_root: &Path, dict_id: &str, entry_id: u32) -> bool {
    matches!(lookup_clip(lexicon_root, dict_id, entry_id), Ok(Some(_)))
}

pub fn lookup_clip(lexicon_root: &Path, dict_id: &str, entry_id: u32) -> Result<Option<ClipLoc>> {
    let Some(path) = index_path(lexicon_root, dict_id) else {
        return Ok(None);
    };
    let Some(index) = read_index(&path)? else {
        return Ok(None);
    };
    let Some(loc) = find_entry(&index.records, entry_id) else {
        return Ok(None);
    };
    let Some(pak) = pack_path(lexicon_root, dict_id) else {
        return Ok(None);
    };
    match std::fs::metadata(&pak) {
        Ok(meta) => {
            let Some(end) = packed_clip_end(loc.offset, loc.length, 32) else {
                return Err(anyhow!("pronunciation clip is outside the pack"));
            };
            if meta.len() < end {
                return Err(anyhow!("pronunciation clip is outside the pack"));
            }
            Ok(Some(*loc))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn parse_index(bytes: &[u8]) -> Result<AudioIndex> {
    if bytes.len() < INDEX_HEADER_LEN + 4 || &bytes[..8] != INDEX_MAGIC {
        return Err(anyhow!("bad pronunciation index"));
    }
    let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
    let codec = u16::from_le_bytes(bytes[10..12].try_into().unwrap());
    if version != 1 || codec != 1 {
        return Err(anyhow!("unsupported pronunciation index"));
    }
    let sample_rate = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let count_u32 = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    if sample_rate != CLIP_SAMPLE_RATE_HZ {
        return Err(anyhow!("pronunciation index sample rate"));
    }
    let Some(total) = index_span_bytes(count_u32, usize::BITS) else {
        return Err(anyhow!("pronunciation index count"));
    };
    if index_len_exceeds_cap(total) || bytes.len() as u64 != total {
        return Err(anyhow!("pronunciation index length"));
    }
    let body = usize::try_from(total - 4).map_err(|_| anyhow!("pronunciation index count"))?;
    let actual = u32::from_le_bytes(bytes[body..body + 4].try_into().unwrap());
    if crc32(&bytes[..body]) != actual {
        return Err(anyhow!("pronunciation index crc"));
    }
    let count = count_u32 as usize;
    let mut records: Vec<ClipLoc> = Vec::with_capacity(count);
    for index in 0..count {
        let start = INDEX_HEADER_LEN + index * INDEX_RECORD_LEN;
        let entry_id = u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap());
        let offset = u32::from_le_bytes(bytes[start + 4..start + 8].try_into().unwrap());
        let length = u32::from_le_bytes(bytes[start + 8..start + 12].try_into().unwrap());
        if packed_clip_end(offset, length, 32).is_none() {
            return Err(anyhow!("pronunciation clip is outside the pack"));
        }
        if let Some(previous) = records.last() {
            if entry_id <= previous.entry_id {
                return Err(anyhow!("pronunciation index is not sorted"));
            }
        }
        records.push(ClipLoc {
            entry_id,
            offset,
            length,
        });
    }
    Ok(AudioIndex {
        sample_rate,
        records,
    })
}

pub fn encode_index(sample_rate: u32, records: &[ClipLoc]) -> Result<Vec<u8>> {
    let mut body = Vec::with_capacity(INDEX_HEADER_LEN + records.len() * INDEX_RECORD_LEN);
    body.extend_from_slice(INDEX_MAGIC);
    body.extend_from_slice(&1_u16.to_le_bytes());
    body.extend_from_slice(&1_u16.to_le_bytes());
    body.extend_from_slice(&sample_rate.to_le_bytes());
    body.extend_from_slice(&(records.len() as u32).to_le_bytes());
    let mut previous = None;
    for record in records {
        if previous.is_some_and(|prior: u32| record.entry_id <= prior) {
            return Err(anyhow!("pronunciation index is not sorted"));
        }
        previous = Some(record.entry_id);
        body.extend_from_slice(&record.entry_id.to_le_bytes());
        body.extend_from_slice(&record.offset.to_le_bytes());
        body.extend_from_slice(&record.length.to_le_bytes());
    }
    let crc = crc32(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    Ok(body)
}

fn read_index(path: &Path) -> Result<Option<AudioIndex>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let len = file.metadata()?.len();
    if index_len_exceeds_cap(len) {
        return Err(anyhow!("pronunciation index is too large"));
    }
    let mut bytes = Vec::new();
    Read::take(&mut file, INDEX_MAX_BYTES as u64).read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(parse_index(&bytes)?))
}

fn read_clip_header(file: &mut File, loc: ClipLoc) -> Result<ClipHeader> {
    file.seek(SeekFrom::Start(u64::from(loc.offset)))?;
    let mut header = [0_u8; CLIP_HEADER_LEN];
    file.read_exact(&mut header)?;
    let parsed = parse_clip_header_prefix(&header, loc.length)?;
    Ok(parsed)
}

fn parse_clip_header_prefix(header: &[u8; CLIP_HEADER_LEN], clip_len: u32) -> Result<ClipHeader> {
    if &header[..8] != super::adpcm::CLIP_MAGIC {
        return Err(anyhow!("bad pronunciation clip"));
    }
    let version = u16::from_le_bytes([header[8], header[9]]);
    let channels = u16::from_le_bytes([header[10], header[11]]);
    if version != 1 || channels != 1 {
        return Err(anyhow!("unsupported pronunciation clip"));
    }
    let sample_rate = u32::from_le_bytes(header[12..16].try_into().unwrap());
    let sample_count = u32::from_le_bytes(header[16..20].try_into().unwrap());
    let predictor = i16::from_le_bytes([header[20], header[21]]);
    let step_index = header[22];
    if sample_rate != CLIP_SAMPLE_RATE_HZ || step_index > 88 {
        return Err(anyhow!("pronunciation clip length"));
    }
    let Some(expected) = clip_span_bytes(sample_count, 32) else {
        return Err(anyhow!("pronunciation clip length"));
    };
    if u64::from(clip_len) != expected {
        return Err(anyhow!("pronunciation clip truncated"));
    }
    Ok(ClipHeader {
        sample_rate,
        sample_count,
        predictor,
        step_index,
    })
}

fn find_entry(records: &[ClipLoc], entry_id: u32) -> Option<&ClipLoc> {
    records
        .binary_search_by_key(&entry_id, |record| record.entry_id)
        .ok()
        .map(|index| &records[index])
}

pub fn index_path(lexicon_root: &Path, dict_id: &str) -> Option<PathBuf> {
    dict_dir(lexicon_root, dict_id).map(|dir| dir.join(INDEX_NAME))
}

pub fn pack_path(lexicon_root: &Path, dict_id: &str) -> Option<PathBuf> {
    dict_dir(lexicon_root, dict_id).map(|dir| dir.join(PACK_NAME))
}

fn dict_dir(lexicon_root: &Path, dict_id: &str) -> Option<PathBuf> {
    let id = fat_dict_id(dict_id)?;
    Some(lexicon_root.join(id))
}

fn fat_dict_id(dict_id: &str) -> Option<&str> {
    if dict_id.is_empty() || dict_id.len() > 8 {
        return None;
    }
    if dict_id
        .bytes()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        Some(dict_id)
    } else {
        None
    }
}

/// Decode a whole clip. Used by host tests; the device session streams.
pub fn decode_packed_clip(pak: &[u8], loc: ClipLoc) -> Result<Vec<i16>> {
    let Some(end) = packed_clip_end(loc.offset, loc.length, 32) else {
        return Err(anyhow!("pronunciation clip length"));
    };
    let start = usize::try_from(loc.offset).map_err(|_| anyhow!("pronunciation clip length"))?;
    let end = usize::try_from(end).map_err(|_| anyhow!("pronunciation clip length"))?;
    let bytes = pak
        .get(start..end)
        .ok_or_else(|| anyhow!("pronunciation clip"))?;
    decode_clip(bytes).map_err(|error| anyhow!(error))
}

#[cfg(test)]
mod tests {
    use super::{
        encode_index, index_len_exceeds_cap, index_span_bytes, lookup_clip, packed_clip_end,
        parse_index, pronounce_available, ClipLoc, PronounceSession, INDEX_HEADER_LEN, INDEX_MAGIC,
        INDEX_MAX_BYTES, INDEX_NAME, PACK_NAME,
    };
    use crate::audio::adpcm::{encode_clip, CLIP_HEADER_LEN, CLIP_SAMPLE_RATE_HZ};
    use std::fs;
    use std::path::Path;

    fn write_pack(dir: &Path, entries: &[(u32, &[i16])]) {
        fs::create_dir_all(dir).unwrap();
        let mut pak = Vec::new();
        let mut records = Vec::new();
        for (entry_id, samples) in entries {
            let clip = encode_clip(samples, CLIP_SAMPLE_RATE_HZ).unwrap();
            records.push(ClipLoc {
                entry_id: *entry_id,
                offset: pak.len() as u32,
                length: clip.len() as u32,
            });
            pak.extend_from_slice(&clip);
        }
        fs::write(dir.join(PACK_NAME), pak).unwrap();
        fs::write(
            dir.join(INDEX_NAME),
            encode_index(CLIP_SAMPLE_RATE_HZ, &records).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn index_lookup_finds_sorted_entry_and_rejects_crc() {
        let root = std::env::temp_dir().join(format!("rmx-aud-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write_pack(
            &root.join("ECDICT"),
            &[(1, &[0, 800, -800, 0]), (4, &[100, -100])],
        );
        let loc = lookup_clip(&root, "ECDICT", 4).unwrap().unwrap();
        assert_eq!(loc.entry_id, 4);
        assert!(lookup_clip(&root, "ECDICT", 2).unwrap().is_none());
        assert!(pronounce_available(&root, "ECDICT", 1));
        assert!(!pronounce_available(&root, "ECDICT", 9));

        let mut index = fs::read(root.join("ECDICT").join(INDEX_NAME)).unwrap();
        let parsed = parse_index(&index).unwrap();
        assert_eq!(parsed.records.len(), 2);
        let last = index.len() - 1;
        index[last] ^= 0xFF;
        assert!(parse_index(&index).is_err());
        fs::write(root.join("ECDICT").join(INDEX_NAME), index).unwrap();
        assert!(lookup_clip(&root, "ECDICT", 1).is_err());
        assert!(!pronounce_available(&root, "ECDICT", 1));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_pack_and_bad_dict_id_are_quiet() {
        let root = std::env::temp_dir().join(format!("rmx-aud-miss-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        assert!(lookup_clip(&root, "ECDICT", 1).unwrap().is_none());
        assert!(PronounceSession::open(&root, "ECDICT", 1)
            .unwrap()
            .is_none());
        write_pack(&root.join("JMDICT"), &[(3, &[0, 400])]);
        fs::remove_file(root.join("JMDICT").join(PACK_NAME)).unwrap();
        assert!(lookup_clip(&root, "JMDICT", 3).unwrap().is_none());
        assert!(lookup_clip(&root, "../ECDICT", 1).unwrap().is_none());
        assert!(lookup_clip(&root, "ecdict", 1).unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn session_decodes_in_chunks_and_stops() {
        let root = std::env::temp_dir().join(format!("rmx-aud-play-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let samples: Vec<i16> = (0..20).map(|index| index * 100).collect();
        write_pack(&root.join("CEDICT"), &[(7, &samples)]);
        let mut session = PronounceSession::open(&root, "CEDICT", 7).unwrap().unwrap();
        let mut pcm = Vec::new();
        let mut buffer = [0_u8; 8];
        loop {
            let bytes = session.read_pcm16_mono(&mut buffer).unwrap();
            if bytes == 0 {
                break;
            }
            pcm.extend_from_slice(&buffer[..bytes]);
        }
        assert_eq!(pcm.len(), samples.len() * 2);
        assert_eq!(session.read_pcm16_mono(&mut buffer).unwrap(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hostile_index_count_and_truncated_files_are_rejected() {
        assert_eq!(index_span_bytes(u32::MAX, 32), None);
        let wide = index_span_bytes(u32::MAX, 64).unwrap();
        assert!(wide > INDEX_MAX_BYTES as u64);
        let hostile_len = (INDEX_MAX_BYTES as u64).wrapping_add(1_u64 << 32);
        assert_eq!(hostile_len as u32, INDEX_MAX_BYTES as u32);
        assert!(index_len_exceeds_cap(hostile_len));
        assert_eq!(packed_clip_end(u32::MAX, CLIP_HEADER_LEN as u32, 32), None);
        assert!(packed_clip_end(u32::MAX, CLIP_HEADER_LEN as u32, 64).is_some());
        assert_eq!(packed_clip_end(0, u32::MAX, 32), None);

        let mut header = vec![0_u8; INDEX_HEADER_LEN + 4];
        header[..8].copy_from_slice(INDEX_MAGIC);
        header[8..10].copy_from_slice(&1_u16.to_le_bytes());
        header[10..12].copy_from_slice(&1_u16.to_le_bytes());
        header[12..16].copy_from_slice(&CLIP_SAMPLE_RATE_HZ.to_le_bytes());
        header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        let error = parse_index(&header).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("count") || message.contains("length"),
            "{message}"
        );

        let root = std::env::temp_dir().join(format!("rmx-aud-hostile-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write_pack(&root.join("ECDICT"), &[(1, &[0, 100])]);
        let index_path = root.join("ECDICT").join(INDEX_NAME);
        let mut truncated = fs::read(&index_path).unwrap();
        truncated.pop();
        assert!(parse_index(&truncated).is_err());
        fs::write(&index_path, &header).unwrap();
        assert!(lookup_clip(&root, "ECDICT", 1).is_err());
        assert!(!pronounce_available(&root, "ECDICT", 1));

        write_pack(&root.join("ECDICT"), &[(1, &[0, 100])]);
        let mut records = parse_index(&fs::read(&index_path).unwrap())
            .unwrap()
            .records;
        records[0].length = u32::MAX;
        fs::write(
            &index_path,
            encode_index(CLIP_SAMPLE_RATE_HZ, &records).unwrap(),
        )
        .unwrap();
        assert!(parse_index(&fs::read(&index_path).unwrap()).is_err());

        write_pack(&root.join("ECDICT"), &[(1, &[0, 100, 200, -200])]);
        let pak_path = root.join("ECDICT").join(PACK_NAME);
        let mut pak = fs::read(&pak_path).unwrap();
        pak.truncate(CLIP_HEADER_LEN);
        fs::write(&pak_path, pak).unwrap();
        assert!(PronounceSession::open(&root, "ECDICT", 1).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn damaged_clip_does_not_open() {
        let root = std::env::temp_dir().join(format!("rmx-aud-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write_pack(&root.join("KANJI"), &[(0, &[10, -10, 20, -20])]);
        let mut pak = fs::read(root.join("KANJI").join(PACK_NAME)).unwrap();
        pak[0] = b'X';
        fs::write(root.join("KANJI").join(PACK_NAME), pak).unwrap();
        assert!(PronounceSession::open(&root, "KANJI", 0).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
