//! RMXSRS1 progress file with TMP → BAK → BIN replacement.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Result};

use super::scheduler::Algo;
use crate::lexicon::crc32;

pub const VOCAB_ROOT: &str = "/sdcard/RUSTMIX/VOCAB";
pub const PROGRESS_BIN: &str = "PROGRESS.BIN";
pub const PROGRESS_TMP: &str = "PROGRESS.TMP";
pub const PROGRESS_BAK: &str = "PROGRESS.BAK";
pub const REVIEW_LOG: &str = "REVIEW.LOG";
pub const SETTINGS_FILE: &str = "SETTINGS.TXT";
pub const DICTS_FILE: &str = "DICTS.TXT";
pub const MYWORDS_FILE: &str = "MYWORDS.TXT";
const MAGIC: &[u8; 8] = b"RMXSRS1\0";
const RECORD_LEN: usize = 32;
/// Wi-Fi and SD can replace `PROGRESS.BIN`. Refuse to load more than this.
pub const MAX_PROGRESS_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredCard {
    pub dict_slot: u8,
    pub state: u8,
    pub reps: u16,
    pub entry_id: u32,
    pub stability_bits: u32,
    pub difficulty_bits: u32,
    pub due_day: u32,
    pub last_day: u32,
    pub lapses: u16,
}

impl StoredCard {
    #[must_use]
    pub fn stability(&self) -> f32 {
        f32::from_bits(self.stability_bits)
    }

    #[must_use]
    pub fn difficulty(&self) -> f32 {
        f32::from_bits(self.difficulty_bits)
    }

    #[must_use]
    pub fn key(&self) -> (u8, u32) {
        (self.dict_slot, self.entry_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgressFile {
    pub algo: Algo,
    pub cards: Vec<StoredCard>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VocabSettings {
    pub algo: Algo,
    pub new_per_day: u32,
    pub max_reviews: u32,
    pub retention_thousandths: u32,
    pub list: String,
}

impl Default for VocabSettings {
    fn default() -> Self {
        Self {
            algo: Algo::Fsrs6,
            new_per_day: 20,
            max_reviews: 200,
            retention_thousandths: 900,
            list: "CET4".into(),
        }
    }
}

impl VocabSettings {
    #[must_use]
    pub fn retention(&self) -> f64 {
        f64::from(self.retention_thousandths) / 1000.0
    }
}

pub fn load_progress(dir: &Path) -> Result<ProgressFile> {
    let bin = dir.join(PROGRESS_BIN);
    if let Ok(file) = read_progress_file(&bin) {
        return Ok(file);
    }
    let bak = dir.join(PROGRESS_BAK);
    if bak.is_file() {
        return read_progress_file(&bak);
    }
    if bin.is_file() {
        bail!("progress checksum failed");
    }
    Ok(ProgressFile {
        algo: Algo::Fsrs6,
        cards: Vec::new(),
    })
}

pub fn save_progress(dir: &Path, progress: &ProgressFile) -> Result<()> {
    fs::create_dir_all(dir)?;
    let bytes = encode_progress(progress);
    atomic_replace(dir, PROGRESS_TMP, PROGRESS_BAK, PROGRESS_BIN, &bytes)
}

pub fn encode_progress(progress: &ProgressFile) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16 + progress.cards.len() * RECORD_LEN + 4);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&progress.algo.file_code().to_le_bytes());
    bytes.extend_from_slice(&(progress.cards.len() as u32).to_le_bytes());
    for card in &progress.cards {
        bytes.push(card.dict_slot);
        bytes.push(card.state);
        bytes.extend_from_slice(&card.reps.to_le_bytes());
        bytes.extend_from_slice(&card.entry_id.to_le_bytes());
        bytes.extend_from_slice(&card.stability_bits.to_le_bytes());
        bytes.extend_from_slice(&card.difficulty_bits.to_le_bytes());
        bytes.extend_from_slice(&card.due_day.to_le_bytes());
        bytes.extend_from_slice(&card.last_day.to_le_bytes());
        bytes.extend_from_slice(&card.lapses.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
    }
    let sum = crc32(&bytes);
    bytes.extend_from_slice(&sum.to_le_bytes());
    bytes
}

/// Total file length for `count` records, or `None` when the product does not
/// fit in an unsigned integer of `width_bits` (32 on the ESP32).
#[must_use]
pub fn progress_span_bytes(count: u32, width_bits: u32) -> Option<u64> {
    let records = u64::from(count).checked_mul(RECORD_LEN as u64)?;
    let body = records.checked_add(16)?;
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

pub fn decode_progress(bytes: &[u8]) -> Result<ProgressFile> {
    if bytes.len() < 16 || &bytes[..8] != MAGIC {
        bail!("bad progress magic");
    }
    let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
    if version != 1 {
        bail!("unsupported progress version");
    }
    let algo = Algo::from_file_code(u16::from_le_bytes(bytes[10..12].try_into().unwrap()));
    let count_u32 = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let Some(total) = progress_span_bytes(count_u32, usize::BITS) else {
        bail!("progress length overflow");
    };
    if total != bytes.len() as u64 {
        bail!("progress length mismatch");
    }
    let body =
        usize::try_from(total - 4).map_err(|_| anyhow::anyhow!("progress length overflow"))?;
    let expected = crc32(&bytes[..body]);
    let actual = u32::from_le_bytes(bytes[body..body + 4].try_into().unwrap());
    if expected != actual {
        bail!("bad progress crc");
    }
    let count = count_u32 as usize;
    let mut cards = Vec::with_capacity(count);
    for index in 0..count {
        let start = 16 + index * RECORD_LEN;
        let rec = &bytes[start..start + RECORD_LEN];
        cards.push(StoredCard {
            dict_slot: rec[0],
            state: rec[1],
            reps: u16::from_le_bytes(rec[2..4].try_into().unwrap()),
            entry_id: u32::from_le_bytes(rec[4..8].try_into().unwrap()),
            stability_bits: u32::from_le_bytes(rec[8..12].try_into().unwrap()),
            difficulty_bits: u32::from_le_bytes(rec[12..16].try_into().unwrap()),
            due_day: u32::from_le_bytes(rec[16..20].try_into().unwrap()),
            last_day: u32::from_le_bytes(rec[20..24].try_into().unwrap()),
            lapses: u16::from_le_bytes(rec[24..26].try_into().unwrap()),
        });
    }
    Ok(ProgressFile { algo, cards })
}

fn read_progress_file(path: &Path) -> Result<ProgressFile> {
    let len = fs::metadata(path)?.len();
    if len > MAX_PROGRESS_BYTES {
        bail!("progress file exceeds {MAX_PROGRESS_BYTES} bytes");
    }
    let bytes = fs::read(path)?;
    decode_progress(&bytes)
}

pub fn atomic_replace(
    dir: &Path,
    tmp_name: &str,
    bak_name: &str,
    bin_name: &str,
    bytes: &[u8],
) -> Result<()> {
    let tmp = dir.join(tmp_name);
    let bak = dir.join(bak_name);
    let bin = dir.join(bin_name);
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if bin.exists() {
        let _ = fs::remove_file(&bak);
        fs::rename(&bin, &bak)?;
    }
    fs::rename(&tmp, &bin)?;
    Ok(())
}

pub fn load_settings(dir: &Path) -> VocabSettings {
    let path = dir.join(SETTINGS_FILE);
    let Ok(text) = fs::read_to_string(path) else {
        return VocabSettings::default();
    };
    let mut settings = VocabSettings::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "algo" => settings.algo = Algo::parse(value.trim()),
            "new_per_day" => {
                settings.new_per_day = value.trim().parse().unwrap_or(settings.new_per_day)
            }
            "max_reviews" => {
                settings.max_reviews = value.trim().parse().unwrap_or(settings.max_reviews)
            }
            "retention" => {
                if let Ok(parsed) = value.trim().parse::<f64>() {
                    settings.retention_thousandths = (parsed * 1000.0).round() as u32;
                }
            }
            "list" => settings.list = value.trim().to_string(),
            _ => {}
        }
    }
    settings
}

pub fn ensure_dict_slot(dir: &Path, dict_id: &str) -> Result<u8> {
    fs::create_dir_all(dir)?;
    let path = dir.join(DICTS_FILE);
    let mut lines = if path.is_file() {
        fs::read_to_string(&path)?
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if let Some(index) = lines.iter().position(|line| line == dict_id) {
        return Ok(index as u8);
    }
    if lines.len() >= 16 {
        bail!("dict slot table is full");
    }
    lines.push(dict_id.to_string());
    fs::write(path, lines.join("\n") + "\n")?;
    Ok((lines.len() - 1) as u8)
}

pub fn dict_slots(dir: &Path) -> Result<Vec<String>> {
    let path = dir.join(DICTS_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    Ok(fs::read_to_string(path)?
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

pub fn add_myword(dir: &Path, dict_id: &str, entry_id: u32) -> Result<()> {
    fs::create_dir_all(dir)?;
    let bin = dir.join(MYWORDS_FILE);
    let mut lines = if bin.is_file() {
        fs::read_to_string(&bin)?
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let row = format!("{dict_id},{entry_id}");
    if lines.iter().any(|line| line == &row) {
        return Ok(());
    }
    lines.push(row);
    atomic_replace(
        dir,
        "MYWORDS.TMP",
        "MYWORDS.BAK",
        MYWORDS_FILE,
        (lines.join("\n") + "\n").as_bytes(),
    )
}

pub fn read_mywords(dir: &Path) -> Result<Vec<(String, u32)>> {
    let path = dir.join(MYWORDS_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    for line in fs::read_to_string(path)?.lines() {
        let Some((dict_id, id)) = line.split_once(',') else {
            continue;
        };
        if let Ok(entry_id) = id.trim().parse() {
            rows.push((dict_id.trim().to_string(), entry_id));
        }
    }
    Ok(rows)
}

pub fn append_review_log(
    dir: &Path,
    day: u32,
    dict_slot: u8,
    entry_id: u32,
    rating: &str,
    elapsed: u32,
) -> Result<()> {
    fs::create_dir_all(dir)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(REVIEW_LOG))?;
    writeln!(file, "{day},{dict_slot},{entry_id},{rating},{elapsed}")?;
    Ok(())
}

pub fn recent_review_days(dir: &Path) -> Vec<u32> {
    let path = dir.join(REVIEW_LOG);
    let Ok(bytes) = read_tail(&path, 256 * 1024) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .filter_map(|line| line.split(',').next()?.parse().ok())
        .collect()
}

fn read_tail(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len > limit {
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start((len - limit) as u64))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[must_use]
pub fn temp_vocab_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rmx-{name}-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::{
        crc32, decode_progress, encode_progress, load_progress, progress_span_bytes, save_progress,
        temp_vocab_dir, ProgressFile, StoredCard, MAGIC, PROGRESS_BAK, PROGRESS_BIN,
    };
    use crate::vocab::scheduler::Algo;
    use std::fs;

    fn sample() -> ProgressFile {
        ProgressFile {
            algo: Algo::Fsrs6,
            cards: vec![StoredCard {
                dict_slot: 1,
                state: 2,
                reps: 3,
                entry_id: 42,
                stability_bits: 1.5f32.to_bits(),
                difficulty_bits: 4.25f32.to_bits(),
                due_day: 20_000,
                last_day: 19_990,
                lapses: 1,
            }],
        }
    }

    #[test]
    fn progress_round_trip() {
        let bytes = encode_progress(&sample());
        assert_eq!(decode_progress(&bytes).unwrap(), sample());
    }

    #[test]
    fn corrupt_bin_falls_back_to_bak() {
        let dir = temp_vocab_dir("bak");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        save_progress(&dir, &sample()).unwrap();
        fs::copy(dir.join(PROGRESS_BIN), dir.join(PROGRESS_BAK)).unwrap();
        let mut broken = fs::read(dir.join(PROGRESS_BIN)).unwrap();
        broken[20] ^= 0xFF;
        fs::write(dir.join(PROGRESS_BIN), broken).unwrap();
        let loaded = load_progress(&dir).unwrap();
        assert_eq!(loaded, sample());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_tmp_rename_replaces_bin() {
        let dir = temp_vocab_dir("tmp");
        let _ = fs::remove_dir_all(&dir);
        save_progress(&dir, &sample()).unwrap();
        let mut second = sample();
        second.cards[0].reps = 9;
        save_progress(&dir, &second).unwrap();
        assert!(!dir.join("PROGRESS.TMP").exists());
        assert!(dir.join(PROGRESS_BAK).is_file());
        assert_eq!(load_progress(&dir).unwrap().cards[0].reps, 9);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrapped_progress_count_is_rejected_on_32_bit_width() {
        assert_eq!(progress_span_bytes(0x0800_0000, 32), None);
        assert!(progress_span_bytes(0x0800_0000, 64).is_some());
        let mut bytes = vec![0u8; 20];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&0x0800_0000u32.to_le_bytes());
        let sum = crc32(&bytes[..16]);
        bytes[16..20].copy_from_slice(&sum.to_le_bytes());
        let error = decode_progress(&bytes).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("overflow") || message.contains("mismatch"),
            "{message}"
        );
    }

    #[test]
    fn oversized_progress_file_is_rejected_before_read() {
        let dir = temp_vocab_dir("huge-progress");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PROGRESS_BIN);
        let file = fs::File::create(&path).unwrap();
        file.set_len(super::MAX_PROGRESS_BYTES + 1).unwrap();
        drop(file);
        let error = super::read_progress_file(&path).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{}", error);
        assert!(load_progress(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_crc_is_rejected() {
        let mut bytes = encode_progress(&sample());
        let last = bytes.len() - 1;
        bytes[last] ^= 0x5A;
        assert!(decode_progress(&bytes).is_err());
    }
}
