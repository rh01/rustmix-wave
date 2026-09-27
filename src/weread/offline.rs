//! Whole-book cache on the SD card.
//!
//! Directory names are 8 hex characters so they stay FAT 8.3 safe. Chapter files
//! are 8 hex digits plus `.TXT`, which stays 8.3 for every `u32` index. Reads
//! refuse files larger than their cap before allocating.

use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use crate::weread::{
    decode,
    limits::{
        DOWNLOAD_CHUNK_BYTES, MAX_CHAPTERS, MAX_CHAPTER_TEXT, MAX_META_BYTES, MAX_SHARD_BYTES,
        MAX_TITLE_CHARS,
    },
    parse::{ChapterMeta, ReadingProgress},
    session::atomic_write,
    text,
};

/// One piece of a chapter response, handed from the PSRAM worker to the main task.
///
/// The main task is the only place that writes the SD card. FATFS code runs from
/// flash, and a task whose stack is in PSRAM must not disable the flash cache.
#[derive(Debug)]
pub enum DownloadEvent {
    BeginPart(&'static str),
    Chunk(Vec<u8>),
    EndPart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedChapter {
    pub uid: String,
    pub index: u32,
    pub title: String,
    pub text: String,
}

pub fn book_dir(root: &Path, book_id: &str) -> PathBuf {
    root.join("WEREAD").join(book_folder(book_id))
}

#[must_use]
pub fn book_folder(book_id: &str) -> String {
    let mut hash = 0x811c_9dc5u32;
    for byte in book_id.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08X}")
}

pub fn save_catalog(
    root: &Path,
    book_id: &str,
    title: &str,
    author: &str,
    format: &str,
    psvts: &str,
    chapters: &[ChapterMeta],
) -> Result<(), String> {
    let dir = book_dir(root, book_id);
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let meta = format!(
        "WRBK1\nid={book_id}\ntitle={}\nauthor={}\nformat={format}\npsvts={psvts}\nchapters={}\n",
        one_line(title),
        one_line(author),
        chapters.len().min(MAX_CHAPTERS)
    );
    if meta.len() > MAX_META_BYTES {
        return Err("book metadata exceeds the size limit".into());
    }
    atomic_write(
        &dir.join("META.TXT"),
        &dir.join("META.TMP"),
        &dir.join("META.BAK"),
        meta.as_bytes(),
    )?;
    let mut toc = String::from("WRTOC1\n");
    for chapter in chapters.iter().take(MAX_CHAPTERS) {
        toc.push_str(&chapter.uid);
        toc.push('\t');
        toc.push_str(&chapter.index.to_string());
        toc.push('\t');
        toc.push_str(&one_line(&chapter.title));
        toc.push('\n');
        if toc.len() > 128 * 1024 {
            break;
        }
    }
    atomic_write(
        &dir.join("TOC.TXT"),
        &dir.join("TOC.TMP"),
        &dir.join("TOC.BAK"),
        toc.as_bytes(),
    )
}

pub fn load_psvts(root: &Path, book_id: &str) -> String {
    let Ok(bytes) = read_capped(&book_dir(root, book_id).join("META.TXT"), MAX_META_BYTES) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .find_map(|line| line.strip_prefix("psvts="))
        .unwrap_or("")
        .trim()
        .to_string()
}

pub fn load_catalog(root: &Path, book_id: &str) -> Option<Vec<ChapterMeta>> {
    let path = book_dir(root, book_id).join("TOC.TXT");
    let bytes = read_capped(&path, 128 * 1024).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    if !text
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "WRTOC1")
    {
        return None;
    }
    let mut chapters = Vec::new();
    for line in text.lines().skip(1) {
        if chapters.len() >= MAX_CHAPTERS {
            break;
        }
        let mut parts = line.split('\t');
        let uid = parts.next()?.trim();
        let index = parts.next()?.trim().parse().unwrap_or(0);
        let title = parts.next().unwrap_or("").trim();
        if uid.is_empty() {
            continue;
        }
        chapters.push(ChapterMeta {
            uid: uid.chars().take(16).collect(),
            index,
            title: title.chars().take(MAX_TITLE_CHARS).collect(),
            word_count: 0,
            level: 1,
        });
    }
    Some(chapters)
}

/// Streams one chapter to `CHAP.TMP`, then renames it into place.
///
/// Dropping the writer without [`ChapterDownload::commit`] deletes the temp file.
pub struct ChapterDownload {
    file: Option<File>,
    temp_path: PathBuf,
    final_path: PathBuf,
    backup_path: PathBuf,
    part_len_pos: Option<u64>,
    part_len: usize,
    part_open: bool,
    committed: bool,
}

impl ChapterDownload {
    pub fn begin(root: &Path, book_id: &str, uid: &str, index: u32) -> Result<Self, String> {
        let dir = book_dir(root, book_id);
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let temp_path = dir.join("CHAP.TMP");
        let mut file = File::create(&temp_path).map_err(|error| error.to_string())?;
        let header = format!("WRRAW1\nuid={}\nidx={index}\ntitle=\n---\n", one_line(uid));
        file.write_all(header.as_bytes())
            .map_err(|error| error.to_string())?;
        Ok(Self {
            file: Some(file),
            temp_path,
            final_path: dir.join(chapter_name(index)),
            backup_path: dir.join("CHAP.BAK"),
            part_len_pos: None,
            part_len: 0,
            part_open: false,
            committed: false,
        })
    }

    pub fn apply(&mut self, event: DownloadEvent) -> Result<(), String> {
        match event {
            DownloadEvent::BeginPart(name) => self.begin_part(name),
            DownloadEvent::Chunk(bytes) => self.write_chunk(&bytes),
            DownloadEvent::EndPart => self.end_part(),
        }
    }

    pub fn commit(mut self) -> Result<(), String> {
        if self.part_open {
            return Err("chapter download incomplete".into());
        }
        if let Some(file) = self.file.as_mut() {
            file.sync_all().ok();
        }
        self.file.take();
        if self.final_path.exists() {
            let _ = fs::rename(&self.final_path, &self.backup_path);
        }
        fs::rename(&self.temp_path, &self.final_path).map_err(|error| error.to_string())?;
        self.committed = true;
        Ok(())
    }

    pub fn abort(self) {}

    fn begin_part(&mut self, name: &str) -> Result<(), String> {
        if self.part_open {
            return Err("chapter download part already open".into());
        }
        if name.is_empty()
            || name.len() > 8
            || !name.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err("chapter download part name is invalid".into());
        }
        let file = self
            .file
            .as_mut()
            .ok_or("chapter download file is closed")?;
        file.write_all(format!("PART {name}\n").as_bytes())
            .map_err(|error| error.to_string())?;
        self.part_len_pos = Some(file.stream_position().map_err(|error| error.to_string())?);
        file.write_all(b"00000000\n")
            .map_err(|error| error.to_string())?;
        self.part_len = 0;
        self.part_open = true;
        Ok(())
    }

    fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !self.part_open {
            return Err("chapter download chunk has no open part".into());
        }
        if bytes.len() > DOWNLOAD_CHUNK_BYTES {
            return Err("chapter download chunk exceeds the size limit".into());
        }
        if self.part_len.saturating_add(bytes.len()) > MAX_SHARD_BYTES {
            return Err("response exceeds size limit".into());
        }
        let file = self
            .file
            .as_mut()
            .ok_or("chapter download file is closed")?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        self.part_len += bytes.len();
        Ok(())
    }

    fn end_part(&mut self) -> Result<(), String> {
        if !self.part_open {
            return Err("chapter download part is not open".into());
        }
        let file = self
            .file
            .as_mut()
            .ok_or("chapter download file is closed")?;
        let end = file.stream_position().map_err(|error| error.to_string())?;
        let pos = self
            .part_len_pos
            .ok_or("chapter download length placeholder is missing")?;
        file.seek(SeekFrom::Start(pos))
            .map_err(|error| error.to_string())?;
        file.write_all(format!("{:08X}\n", self.part_len).as_bytes())
            .map_err(|error| error.to_string())?;
        file.seek(SeekFrom::Start(end))
            .map_err(|error| error.to_string())?;
        self.part_open = false;
        self.part_len_pos = None;
        Ok(())
    }
}

impl Drop for ChapterDownload {
    fn drop(&mut self) {
        self.file.take();
        if !self.committed {
            let _ = fs::remove_file(&self.temp_path);
        }
    }
}

/// Commit a finished download, or delete the temp file when the chapter failed.
pub fn complete_download(
    download: ChapterDownload,
    succeeded: bool,
    write_error: Option<String>,
) -> Result<(), String> {
    if let Some(error) = write_error {
        download.abort();
        return Err(error);
    }
    if !succeeded {
        download.abort();
        return Err("chapter download failed".into());
    }
    download.commit()
}

pub fn save_chapter(root: &Path, book_id: &str, chapter: &CachedChapter) -> Result<(), String> {
    let dir = book_dir(root, book_id);
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let name = chapter_name(chapter.index);
    let mut body = format!(
        "WRCH1\nuid={}\nidx={}\ntitle={}\n---\n",
        chapter.uid,
        chapter.index,
        one_line(&chapter.title)
    );
    body.push_str(
        &chapter
            .text
            .chars()
            .take(MAX_CHAPTER_TEXT)
            .collect::<String>(),
    );
    if body.len() > MAX_CHAPTER_TEXT + 1024 {
        return Err("chapter cache exceeds the size limit".into());
    }
    atomic_write(
        &dir.join(&name),
        &dir.join("CHAP.TMP"),
        &dir.join("CHAP.BAK"),
        body.as_bytes(),
    )
}

pub fn load_chapter(root: &Path, book_id: &str, index: u32) -> Option<CachedChapter> {
    let path = book_dir(root, book_id).join(chapter_name(index));
    match file_magic(&path)? {
        ChapterMagic::Plain => load_plain_chapter(&path, index),
        ChapterMagic::Raw => load_raw_chapter(root, book_id, index, &path),
    }
}

enum ChapterMagic {
    Plain,
    Raw,
}

fn file_magic(path: &Path) -> Option<ChapterMagic> {
    let mut file = File::open(path).ok()?;
    let mut buf = [0u8; 6];
    let read = file.read(&mut buf).ok()?;
    if read >= 6 && &buf[..6] == b"WRRAW1" {
        Some(ChapterMagic::Raw)
    } else if read >= 5 && &buf[..5] == b"WRCH1" {
        Some(ChapterMagic::Plain)
    } else {
        None
    }
}

fn load_plain_chapter(path: &Path, index: u32) -> Option<CachedChapter> {
    let bytes = read_capped(path, MAX_CHAPTER_TEXT + 1024).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let (header, body) = text.split_once("---\n")?;
    if !header
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "WRCH1")
    {
        return None;
    }
    let uid = field(header, "uid");
    let idx = field(header, "idx").parse().unwrap_or(index);
    let title = field(header, "title");
    Some(CachedChapter {
        uid,
        index: idx,
        title,
        text: body.chars().take(MAX_CHAPTER_TEXT).collect(),
    })
}

/// Decode a raw chapter by reading each part from the SD card.
///
/// The shard is assembled only here, when the chapter is opened. A successful
/// decode is rewritten as plain `WRCH1` text so the next open skips the shard.
fn load_raw_chapter(root: &Path, book_id: &str, index: u32, path: &Path) -> Option<CachedChapter> {
    let mut file = File::open(path).ok()?;
    let header = read_through_separator(&mut file, 1024)?;
    if !header
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "WRRAW1")
    {
        return None;
    }
    let uid = field(&header, "uid");
    let idx = field(&header, "idx").parse().unwrap_or(index);
    let mut parts = Vec::new();
    loop {
        let line = read_line(&mut file, 24).ok()?;
        if line.is_empty() {
            break;
        }
        let name = line.strip_prefix("PART ")?.trim();
        if name.is_empty() || parts.len() >= 4 {
            return None;
        }
        let len_line = read_line(&mut file, 16).ok()?;
        let len = usize::from_str_radix(len_line.trim(), 16).ok()?;
        if len > MAX_SHARD_BYTES {
            return None;
        }
        let mut buf = Vec::new();
        buf.try_reserve_exact(len).ok()?;
        let mut left = len;
        let mut chunk = [0u8; DOWNLOAD_CHUNK_BYTES];
        while left > 0 {
            let take = chunk.len().min(left);
            file.read_exact(&mut chunk[..take]).ok()?;
            buf.extend_from_slice(&chunk[..take]);
            left -= take;
        }
        parts.push(buf);
    }
    drop(file);
    let text = materialize_parts(&parts).ok()?;
    let cached = CachedChapter {
        uid: uid.chars().take(16).collect(),
        index: idx,
        title: String::new(),
        text: text.chars().take(MAX_CHAPTER_TEXT).collect(),
    };
    let _ = save_chapter(root, book_id, &cached);
    Some(cached)
}

fn materialize_parts(parts: &[Vec<u8>]) -> Result<String, &'static str> {
    if parts.is_empty() {
        return Err("chapter shard was empty or failed its checksum");
    }
    if parts.len() == 1 && parts[0].starts_with(b"PK\x03\x04") {
        return text::zip_html_text(&parts[0]);
    }
    let owned: Vec<String> = parts
        .iter()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .collect();
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let bytes = decode::decode_shards(&refs)?;
    if bytes.starts_with(b"PK\x03\x04") {
        return text::zip_html_text(&bytes);
    }
    Ok(text::plain_from_blocks(&text::blocks_from_markup(
        &String::from_utf8_lossy(&bytes),
    )))
}

fn read_through_separator(file: &mut File, max: usize) -> Option<String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    while out.len() < max {
        let read = file.read(&mut byte).ok()?;
        if read == 0 {
            return None;
        }
        out.push(byte[0]);
        if out.ends_with(b"\n---\n") {
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
    }
    None
}

fn read_line(file: &mut File, max: usize) -> Result<String, ()> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = file.read(&mut byte).map_err(|_| ())?;
        if read == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] == b'\r' {
            continue;
        }
        if out.len() >= max {
            return Err(());
        }
        out.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

pub fn chapter_cached(root: &Path, book_id: &str, index: u32) -> bool {
    book_dir(root, book_id).join(chapter_name(index)).is_file()
}

pub fn save_progress(
    root: &Path,
    book_id: &str,
    progress: &ReadingProgress,
    page: usize,
) -> Result<(), String> {
    let dir = book_dir(root, book_id);
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let body = format!(
        "WRPG1\nchapter_uid={}\nchapter_offset={}\nprogress={}\npage={page}\n",
        progress.chapter_uid, progress.chapter_offset, progress.progress
    );
    atomic_write(
        &dir.join("PROG.TXT"),
        &dir.join("PROG.TMP"),
        &dir.join("PROG.BAK"),
        body.as_bytes(),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LastOpen {
    pub book_id: String,
    pub chapter_pos: usize,
    pub page_index: usize,
}

pub fn save_last_open(
    root: &Path,
    book_id: &str,
    chapter_pos: usize,
    page_index: usize,
) -> Result<(), String> {
    let dir = root.join("WEREAD");
    let body = format!(
        "WRLAST1\nbook_id={}\nchapter={chapter_pos}\npage={page_index}\n",
        one_line(book_id)
    );
    atomic_write(
        &dir.join("LAST.TXT"),
        &dir.join("LAST.TMP"),
        &dir.join("LAST.BAK"),
        body.as_bytes(),
    )
}

pub fn load_last_open(root: &Path) -> Option<LastOpen> {
    let bytes = read_capped(&root.join("WEREAD").join("LAST.TXT"), 512).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    if !text
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "WRLAST1")
    {
        return None;
    }
    let book_id = field(&text, "book_id");
    if book_id.is_empty() {
        return None;
    }
    Some(LastOpen {
        book_id,
        chapter_pos: field(&text, "chapter").parse().unwrap_or(0),
        page_index: field(&text, "page").parse().unwrap_or(0),
    })
}

pub fn load_local_progress(root: &Path, book_id: &str) -> Option<(ReadingProgress, usize)> {
    let bytes = read_capped(&book_dir(root, book_id).join("PROG.TXT"), 1024).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    if !text
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "WRPG1")
    {
        return None;
    }
    Some((
        ReadingProgress {
            chapter_uid: field(&text, "chapter_uid"),
            chapter_offset: field(&text, "chapter_offset").parse().unwrap_or(0),
            progress: field(&text, "progress").parse::<u8>().unwrap_or(0).min(100),
        },
        field(&text, "page").parse().unwrap_or(0),
    ))
}

/// Eight hex digits plus `.TXT`. `CH{index:04}` grows past 8.3 once `chapterIdx` exceeds 9999.
#[must_use]
pub fn chapter_name(index: u32) -> String {
    format!("{index:08X}.TXT")
}

pub fn read_capped(path: &Path, max: usize) -> Result<Vec<u8>, &'static str> {
    let mut file = File::open(path).map_err(|_| "cache read failed")?;
    let len = file.metadata().map_err(|_| "cache read failed")?.len();
    if len > max as u64 {
        return Err("cache file exceeds the size limit");
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve(len as usize)
        .map_err(|_| "cache file exceeds the size limit")?;
    file.read_to_end(&mut bytes)
        .map_err(|_| "cache read failed")?;
    if bytes.len() > max {
        return Err("cache file exceeds the size limit");
    }
    Ok(bytes)
}

fn field(text: &str, key: &str) -> String {
    let prefix = format!("{key}=");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or("")
        .trim()
        .to_string()
}

fn one_line(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '\n' && *ch != '\r' && *ch != '\t')
        .take(MAX_TITLE_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        load_catalog, load_chapter, read_capped, save_catalog, save_chapter, CachedChapter,
        ChapterDownload, DownloadEvent,
    };
    use crate::weread::{decode::seal_plain, limits::DOWNLOAD_CHUNK_BYTES, parse::ChapterMeta};
    use std::fs;

    #[test]
    fn catalog_and_chapter_round_trip_and_oversize_read_fails() {
        let dir = std::env::temp_dir().join(format!("weread-offline-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let chapters = vec![ChapterMeta {
            uid: "2".into(),
            index: 2,
            title: "第二章".into(),
            word_count: 10,
            level: 1,
        }];
        save_catalog(
            &dir,
            "43208843",
            "持续交付",
            "乔梁",
            "epub",
            "ps",
            &chapters,
        )
        .unwrap();
        let loaded = load_catalog(&dir, "43208843").unwrap();
        assert_eq!(loaded[0].title, "第二章");
        save_chapter(
            &dir,
            "43208843",
            &CachedChapter {
                uid: "2".into(),
                index: 2,
                title: "第二章".into(),
                text: "你好".into(),
            },
        )
        .unwrap();
        assert_eq!(load_chapter(&dir, "43208843", 2).unwrap().text, "你好");
        assert_eq!(super::load_psvts(&dir, "43208843"), "ps");
        let meta = super::book_dir(&dir, "43208843").join("META.TXT");
        fs::write(&meta, vec![b'x'; super::MAX_META_BYTES + 1]).unwrap();
        assert!(super::load_psvts(&dir, "43208843").is_empty());
        let huge = dir.join("HUGE.TXT");
        fs::write(&huge, vec![b'x'; 64]).unwrap();
        assert!(read_capped(&huge, 16).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn chapter_filenames_stay_fat83_for_large_indexes() {
        use crate::wifi_transfer::is_fat83_component;
        for index in [0, 9_999, 10_000, u32::MAX] {
            let name = super::chapter_name(index);
            assert!(is_fat83_component(&name), "{name}");
            assert_eq!(name.len(), 12);
        }
        assert_eq!(super::chapter_name(0x10), "00000010.TXT");
    }

    #[test]
    fn last_open_round_trips_the_chapter_and_page() {
        let dir = std::env::temp_dir().join(format!("weread-last-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        super::save_last_open(&dir, "43208843", 4, 12).unwrap();
        let last = super::load_last_open(&dir).unwrap();
        assert_eq!(last.book_id, "43208843");
        assert_eq!(last.chapter_pos, 4);
        assert_eq!(last.page_index, 12);
        let _ = fs::remove_dir_all(&dir);
    }

    fn temp_book() -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "weread-stream-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (dir, "43208843".into())
    }

    fn write_part(download: &mut ChapterDownload, name: &'static str, bytes: &[u8]) {
        download.apply(DownloadEvent::BeginPart(name)).unwrap();
        for chunk in bytes.chunks(DOWNLOAD_CHUNK_BYTES) {
            download
                .apply(DownloadEvent::Chunk(chunk.to_vec()))
                .unwrap();
        }
        download.apply(DownloadEvent::EndPart).unwrap();
    }

    #[test]
    fn streamed_chunks_rename_into_place_and_reject_oversize_writes() {
        let (dir, book) = temp_book();
        let mut download = ChapterDownload::begin(&dir, &book, "2", 2).unwrap();
        let payload = b"raw-chapter-bytes".repeat(600);
        assert!(payload.len() > DOWNLOAD_CHUNK_BYTES);
        write_part(&mut download, "e0", &payload);
        let book_path = super::book_dir(&dir, &book);
        download.commit().unwrap();
        assert!(!book_path.join("CHAP.TMP").exists());
        let stored = fs::read(book_path.join("00000002.TXT")).unwrap();
        assert!(stored.starts_with(b"WRRAW1\n"));
        assert!(stored
            .windows(payload.len())
            .any(|window| window == payload));

        let mut rejected = ChapterDownload::begin(&dir, &book, "3", 3).unwrap();
        rejected.apply(DownloadEvent::BeginPart("e0")).unwrap();
        let too_big = vec![b'x'; DOWNLOAD_CHUNK_BYTES + 1];
        assert!(rejected
            .apply(DownloadEvent::Chunk(too_big))
            .unwrap_err()
            .contains("chunk"));
        rejected.abort();
        assert!(!book_path.join("CHAP.TMP").exists());
        assert!(!book_path.join("00000003.TXT").exists());

        let mut over_shard = ChapterDownload::begin(&dir, &book, "9", 9).unwrap();
        over_shard.apply(DownloadEvent::BeginPart("e0")).unwrap();
        let piece = vec![b'z'; DOWNLOAD_CHUNK_BYTES];
        for _ in 0..(super::MAX_SHARD_BYTES / DOWNLOAD_CHUNK_BYTES) {
            over_shard
                .apply(DownloadEvent::Chunk(piece.clone()))
                .unwrap();
        }
        assert!(over_shard
            .apply(DownloadEvent::Chunk(vec![b'z']))
            .unwrap_err()
            .contains("size limit"));
        over_shard.abort();
        assert!(!book_path.join("00000009.TXT").exists());

        let open = ChapterDownload::begin(&dir, &book, "4", 4).unwrap();
        assert!(open.commit().is_ok());
        let mut incomplete = ChapterDownload::begin(&dir, &book, "5", 5).unwrap();
        incomplete.apply(DownloadEvent::BeginPart("e0")).unwrap();
        assert!(incomplete.commit().is_err());
        assert!(!book_path.join("CHAP.TMP").exists());
        assert!(!book_path.join("00000005.TXT").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_chapter_decodes_when_opened_and_then_stores_plain_text() {
        let (dir, book) = temp_book();
        let plain = "<p>Hello 微信</p>";
        let shard = seal_plain(plain);
        let mut download = ChapterDownload::begin(&dir, &book, "2", 2).unwrap();
        let mut offset = 0usize;
        download.apply(DownloadEvent::BeginPart("e0")).unwrap();
        while offset < shard.len() {
            let end = (offset + 100).min(shard.len());
            assert!(end - offset <= DOWNLOAD_CHUNK_BYTES);
            download
                .apply(DownloadEvent::Chunk(shard.as_bytes()[offset..end].to_vec()))
                .unwrap();
            offset = end;
        }
        download.apply(DownloadEvent::EndPart).unwrap();
        download.commit().unwrap();

        let loaded = load_chapter(&dir, &book, 2).unwrap();
        assert!(loaded.text.contains("Hello"));
        assert!(loaded.text.contains("微信"));
        let stored = fs::read(super::book_dir(&dir, &book).join("00000002.TXT")).unwrap();
        assert!(stored.starts_with(b"WRCH1\n"));
        assert_eq!(load_chapter(&dir, &book, 2).unwrap().text, loaded.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_download_deletes_the_temp_file() {
        let (dir, book) = temp_book();
        let download = ChapterDownload::begin(&dir, &book, "2", 2).unwrap();
        let error = super::complete_download(download, false, None).unwrap_err();
        assert!(error.contains("failed"));
        assert!(!super::book_dir(&dir, &book).join("CHAP.TMP").exists());
        assert!(!super::chapter_cached(&dir, &book, 2));
        let _ = fs::remove_dir_all(&dir);
    }
}
