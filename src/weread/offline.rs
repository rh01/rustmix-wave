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
        DOWNLOAD_CHUNK_BYTES, MAX_CHAPTERS, MAX_CHAPTER_IMAGES, MAX_CHAPTER_IMAGE_BYTES,
        MAX_CHAPTER_TEXT, MAX_META_BYTES, MAX_SHARD_BYTES, MAX_TITLE_CHARS,
    },
    parse::{ChapterMeta, ReadingProgress},
    session::atomic_write,
    text::{self, Block, ImageRef},
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
        fs::create_dir_all(&dir).map_err(|error| explain_storage_error(&error.to_string()))?;
        let temp_path = dir.join("CHAP.TMP");
        let mut file =
            File::create(&temp_path).map_err(|error| explain_storage_error(&error.to_string()))?;
        let header = format!("WRRAW1\nuid={}\nidx={index}\ntitle=\n---\n", one_line(uid));
        file.write_all(header.as_bytes())
            .map_err(|error| explain_storage_error(&error.to_string()))?;
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
            file.sync_all()
                .map_err(|error| explain_storage_error(&error.to_string()))?;
        }
        self.file.take();
        if self.final_path.exists() {
            let _ = fs::rename(&self.final_path, &self.backup_path);
        }
        fs::rename(&self.temp_path, &self.final_path)
            .map_err(|error| explain_storage_error(&error.to_string()))?;
        self.committed = true;
        Ok(())
    }

    pub fn abort(self) {}

    #[cfg(test)]
    pub fn replace_with_unsyncable_file(&mut self) {
        let (read, write) = std::io::pipe().expect("pipe");
        drop(read);
        use std::os::fd::{FromRawFd, IntoRawFd};
        self.file = Some(unsafe { File::from_raw_fd(write.into_raw_fd()) });
    }

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
            .map_err(|error| explain_storage_error(&error.to_string()))?;
        self.part_len_pos = Some(
            file.stream_position()
                .map_err(|error| explain_storage_error(&error.to_string()))?,
        );
        file.write_all(b"00000000\n")
            .map_err(|error| explain_storage_error(&error.to_string()))?;
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
        file.write_all(bytes)
            .map_err(|error| explain_storage_error(&error.to_string()))?;
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
        let end = file
            .stream_position()
            .map_err(|error| explain_storage_error(&error.to_string()))?;
        let pos = self
            .part_len_pos
            .ok_or("chapter download length placeholder is missing")?;
        file.seek(SeekFrom::Start(pos))
            .map_err(|error| explain_storage_error(&error.to_string()))?;
        file.write_all(format!("{:08X}\n", self.part_len).as_bytes())
            .map_err(|error| explain_storage_error(&error.to_string()))?;
        file.seek(SeekFrom::Start(end))
            .map_err(|error| explain_storage_error(&error.to_string()))?;
        self.part_open = false;
        self.part_len_pos = None;
        Ok(())
    }
}

impl DownloadFile for ChapterDownload {
    fn commit(self) -> Result<(), String> {
        ChapterDownload::commit(self)
    }

    fn abort(self) {
        ChapterDownload::abort(self);
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

/// One chapter image streamed beside the chapter text.
///
/// The temp file is removed unless [`ImageDownload::commit`] runs. A failure
/// here does not touch the chapter text file.
pub struct ImageDownload {
    file: Option<File>,
    temp_path: PathBuf,
    final_path: PathBuf,
    len: usize,
    part_open: bool,
    committed: bool,
}

impl ImageDownload {
    pub fn begin(
        root: &Path,
        book_id: &str,
        chapter_index: u32,
        slot: u16,
    ) -> Result<Self, String> {
        let dir = book_dir(root, book_id);
        fs::create_dir_all(&dir).map_err(|error| explain_storage_error(&error.to_string()))?;
        let temp_path = dir.join("IMG.TMP");
        let file =
            File::create(&temp_path).map_err(|error| explain_storage_error(&error.to_string()))?;
        Ok(Self {
            file: Some(file),
            temp_path,
            final_path: dir.join(image_name(chapter_index, slot)),
            len: 0,
            part_open: false,
            committed: false,
        })
    }

    pub fn apply(&mut self, event: DownloadEvent) -> Result<(), String> {
        match event {
            DownloadEvent::BeginPart(_) => {
                if self.part_open {
                    return Err("image download part already open".into());
                }
                self.part_open = true;
                Ok(())
            }
            DownloadEvent::Chunk(bytes) => self.write_chunk(&bytes),
            DownloadEvent::EndPart => {
                if !self.part_open {
                    return Err("image download part is not open".into());
                }
                self.part_open = false;
                Ok(())
            }
        }
    }

    pub fn commit(mut self) -> Result<(), String> {
        if self.part_open {
            return Err("image download incomplete".into());
        }
        if self.len == 0 {
            return Err("empty image".into());
        }
        if let Some(file) = self.file.as_mut() {
            file.sync_all()
                .map_err(|error| explain_storage_error(&error.to_string()))?;
        }
        self.file.take();
        fs::rename(&self.temp_path, &self.final_path)
            .map_err(|error| explain_storage_error(&error.to_string()))?;
        self.committed = true;
        Ok(())
    }

    pub fn abort(self) {}

    fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !self.part_open {
            return Err("image download chunk has no open part".into());
        }
        if bytes.len() > DOWNLOAD_CHUNK_BYTES {
            return Err("image download chunk exceeds the size limit".into());
        }
        if self.len.saturating_add(bytes.len()) > MAX_CHAPTER_IMAGE_BYTES {
            return Err("response exceeds size limit".into());
        }
        let file = self.file.as_mut().ok_or("image download file is closed")?;
        file.write_all(bytes)
            .map_err(|error| explain_storage_error(&error.to_string()))?;
        self.len += bytes.len();
        Ok(())
    }
}

impl DownloadFile for ImageDownload {
    fn commit(self) -> Result<(), String> {
        ImageDownload::commit(self)
    }

    fn abort(self) {
        ImageDownload::abort(self);
    }
}

impl Drop for ImageDownload {
    fn drop(&mut self) {
        self.file.take();
        if !self.committed {
            let _ = fs::remove_file(&self.temp_path);
        }
    }
}

/// A chapter shard or one chapter image being written by the main task.
pub enum CardDownload {
    Chapter(ChapterDownload),
    Image(ImageDownload),
}

impl From<ChapterDownload> for CardDownload {
    fn from(download: ChapterDownload) -> Self {
        Self::Chapter(download)
    }
}

impl From<ImageDownload> for CardDownload {
    fn from(download: ImageDownload) -> Self {
        Self::Image(download)
    }
}

impl CardDownload {
    pub fn apply(&mut self, event: DownloadEvent) -> Result<(), String> {
        match self {
            Self::Chapter(download) => download.apply(event),
            Self::Image(download) => download.apply(event),
        }
    }

    pub fn abort(self) {
        match self {
            Self::Chapter(download) => download.abort(),
            Self::Image(download) => download.abort(),
        }
    }
}

/// Commit a finished download, or delete the temp file when it failed.
pub fn complete_download(
    download: impl Into<CardDownload>,
    succeeded: bool,
    write_error: Option<String>,
) -> Result<(), String> {
    match download.into() {
        CardDownload::Chapter(download) => finish_download(download, succeeded, write_error),
        CardDownload::Image(download) => finish_download(download, succeeded, write_error),
    }
}

fn finish_download<D: DownloadFile>(
    download: D,
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

trait DownloadFile {
    fn commit(self) -> Result<(), String>;
    fn abort(self);
}

pub fn save_chapter(root: &Path, book_id: &str, chapter: &CachedChapter) -> Result<(), String> {
    let dir = book_dir(root, book_id);
    fs::create_dir_all(&dir).map_err(|error| explain_storage_error(&error.to_string()))?;
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
    .map_err(|error| explain_storage_error(&error))
}

/// Turn a filesystem error into a short status string.
///
/// A full card is named directly. Other FAT failures stay visible instead of
/// looking like a successful save.
#[must_use]
pub fn explain_storage_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("no space")
        || lower.contains("enospc")
        || lower.contains("disk full")
        || lower.contains("not enough space")
        || lower.contains("os error 28")
    {
        return "SD card full".into();
    }
    if lower.contains("sd card full") || lower.starts_with("sd card write failed") {
        return error.chars().take(80).collect();
    }
    let detail: String = error
        .chars()
        .filter(|ch| !ch.is_control())
        .take(60)
        .collect();
    format!("SD card write failed: {detail}")
}

/// Chapters that failed their retries. A reboot must not fetch them again.
///
/// Image skip lines already in `SKIP.TXT` are kept.
pub fn save_download_skip(root: &Path, book_id: &str, indexes: &[u32]) -> Result<(), String> {
    write_skip_file(root, book_id, indexes, &load_image_skips(root, book_id))
}

/// One image that failed its retries. The chapter text stays on the card.
pub fn record_image_skip(
    root: &Path,
    book_id: &str,
    chapter_index: u32,
    slot: u16,
) -> Result<(), String> {
    let mut images = load_image_skips(root, book_id);
    if !images.contains(&(chapter_index, slot)) {
        images.push((chapter_index, slot));
    }
    write_skip_file(root, book_id, &load_download_skip(root, book_id), &images)
}

fn write_skip_file(
    root: &Path,
    book_id: &str,
    indexes: &[u32],
    images: &[(u32, u16)],
) -> Result<(), String> {
    let dir = book_dir(root, book_id);
    fs::create_dir_all(&dir).map_err(|error| explain_storage_error(&error.to_string()))?;
    let mut body = String::from("WRSKIP1\n");
    for index in indexes.iter().take(MAX_CHAPTERS) {
        body.push_str(&index.to_string());
        body.push('\n');
    }
    for (chapter, slot) in images.iter().take(MAX_CHAPTERS) {
        body.push_str(&format!("I {chapter} {slot}\n"));
    }
    atomic_write(
        &dir.join("SKIP.TXT"),
        &dir.join("SKIP.TMP"),
        &dir.join("SKIP.BAK"),
        body.as_bytes(),
    )
    .map_err(|error| explain_storage_error(&error))
}

#[must_use]
pub fn load_download_skip(root: &Path, book_id: &str) -> Vec<u32> {
    let Ok(bytes) = read_capped(&book_dir(root, book_id).join("SKIP.TXT"), 16 * 1024) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    if !text
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "WRSKIP1")
    {
        return Vec::new();
    }
    let mut indexes = Vec::new();
    for line in text.lines().skip(1) {
        if indexes.len() >= MAX_CHAPTERS {
            break;
        }
        if let Ok(index) = line.trim().parse::<u32>() {
            if !indexes.contains(&index) {
                indexes.push(index);
            }
        }
    }
    indexes
}

/// Image slots recorded in `SKIP.TXT`. Chapter index lines are ignored here.
#[must_use]
pub fn load_image_skips(root: &Path, book_id: &str) -> Vec<(u32, u16)> {
    let Ok(bytes) = read_capped(&book_dir(root, book_id).join("SKIP.TXT"), 16 * 1024) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    if !text
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "WRSKIP1")
    {
        return Vec::new();
    }
    let mut images = Vec::new();
    for line in text.lines().skip(1) {
        if images.len() >= MAX_CHAPTERS {
            break;
        }
        let mut parts = line.split_whitespace();
        if parts.next() != Some("I") {
            continue;
        }
        let Some(chapter) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(slot) = parts.next().and_then(|value| value.parse::<u16>().ok()) else {
            continue;
        };
        if !images.contains(&(chapter, slot)) {
            images.push((chapter, slot));
        }
    }
    images
}

/// `{chapter:08X}.I{slot:02X}`. Stem and extension both stay inside FAT 8.3.
#[must_use]
pub fn image_name(chapter_index: u32, slot: u16) -> String {
    format!("{chapter_index:08X}.I{slot:02X}")
}

#[must_use]
pub fn image_cached(root: &Path, book_id: &str, chapter_index: u32, slot: u16) -> bool {
    fs::metadata(book_dir(root, book_id).join(image_name(chapter_index, slot)))
        .is_ok_and(|meta| meta.len() > 0 && meta.len() <= MAX_CHAPTER_IMAGE_BYTES as u64)
}

/// Read one stored chapter image. Empty files are failures so they are fetched again.
pub fn read_chapter_image(
    root: &Path,
    book_id: &str,
    chapter_index: u32,
    slot: u16,
) -> Result<Vec<u8>, &'static str> {
    let bytes = read_capped(
        &book_dir(root, book_id).join(image_name(chapter_index, slot)),
        MAX_CHAPTER_IMAGE_BYTES,
    )?;
    if bytes.is_empty() {
        return Err("empty image");
    }
    Ok(bytes)
}

/// Written after the image pass for a chapter, including when every image was skipped.
pub fn mark_images_settled(root: &Path, book_id: &str, chapter_index: u32) -> Result<(), String> {
    let dir = book_dir(root, book_id);
    fs::create_dir_all(&dir).map_err(|error| explain_storage_error(&error.to_string()))?;
    atomic_write(
        &dir.join(settled_name(chapter_index)),
        &dir.join("RDY.TMP"),
        &dir.join("RDY.BAK"),
        b"WRRDY1\n",
    )
    .map_err(|error| explain_storage_error(&error))
}

#[must_use]
pub fn images_settled(root: &Path, book_id: &str, chapter_index: u32) -> bool {
    book_dir(root, book_id)
        .join(settled_name(chapter_index))
        .is_file()
}

fn settled_name(chapter_index: u32) -> String {
    format!("{chapter_index:08X}.RDY")
}

/// Image URLs from a raw chapter file. Plain `WRCH1` text has already dropped them.
pub fn chapter_image_refs(root: &Path, book_id: &str, index: u32) -> Vec<ImageRef> {
    let path = book_dir(root, book_id).join(chapter_name(index));
    let Some(parts) = read_raw_parts(&path) else {
        return Vec::new();
    };
    let Ok(blocks) = blocks_from_parts(&parts) else {
        return Vec::new();
    };
    blocks
        .into_iter()
        .filter_map(|block| match block {
            Block::Image(image) => Some(image),
            Block::Text(_) => None,
        })
        .take(MAX_CHAPTER_IMAGES)
        .collect()
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
    let (header, parts) = read_raw_file(path)?;
    let uid = field(&header, "uid");
    let idx = field(&header, "idx").parse().unwrap_or(index);
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

fn read_raw_parts(path: &Path) -> Option<Vec<Vec<u8>>> {
    read_raw_file(path).map(|(_, parts)| parts)
}

fn read_raw_file(path: &Path) -> Option<(String, Vec<Vec<u8>>)> {
    let mut file = File::open(path).ok()?;
    let header = read_through_separator(&mut file, 1024)?;
    if !header
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "WRRAW1")
    {
        return None;
    }
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
    Some((header, parts))
}

fn materialize_parts(parts: &[Vec<u8>]) -> Result<String, &'static str> {
    Ok(text::plain_from_blocks(&blocks_from_parts(parts)?))
}

fn blocks_from_parts(parts: &[Vec<u8>]) -> Result<Vec<Block>, &'static str> {
    if parts.is_empty() {
        return Err("chapter shard was empty or failed its checksum");
    }
    if parts.len() == 1 && parts[0].starts_with(b"PK\x03\x04") {
        return text::blocks_from_zip(&parts[0]);
    }
    let owned: Vec<String> = parts
        .iter()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .collect();
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let bytes = decode::decode_shards(&refs)?;
    if bytes.starts_with(b"PK\x03\x04") {
        return text::blocks_from_zip(&bytes);
    }
    Ok(text::blocks_from_markup(&String::from_utf8_lossy(&bytes)))
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

    #[test]
    fn storage_errors_name_a_full_card_and_skipped_chapters_survive() {
        assert_eq!(
            super::explain_storage_error("No space left on device (os error 28)"),
            "SD card full"
        );
        assert_eq!(
            super::explain_storage_error("SD card write failed: busy"),
            "SD card write failed: busy"
        );
        let (dir, book) = temp_book();
        super::save_download_skip(&dir, &book, &[3, 9, 3]).unwrap();
        assert_eq!(super::load_download_skip(&dir, &book), vec![3, 9]);
        assert!(super::load_download_skip(&dir, "missing").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_reports_a_failed_sync_and_does_not_rename() {
        let (dir, book) = temp_book();
        let mut download = ChapterDownload::begin(&dir, &book, "2", 2).unwrap();
        download.apply(DownloadEvent::BeginPart("e0")).unwrap();
        download
            .apply(DownloadEvent::Chunk(b"hello".to_vec()))
            .unwrap();
        download.apply(DownloadEvent::EndPart).unwrap();
        download.replace_with_unsyncable_file();
        let error = download.commit().unwrap_err();
        assert!(error.starts_with("SD card"), "{error}");
        assert!(!super::chapter_cached(&dir, &book, 2));
        assert!(!super::book_dir(&dir, &book).join("CHAP.TMP").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_files_stay_beside_the_chapter_and_skips_do_not_drop_chapters() {
        use crate::wifi_transfer::is_fat83_component;
        let (dir, book) = temp_book();
        let html = "<p>Hello</p><img alt=\"图\" src=\"https://res.weread.qq.com/a.jpg\">";
        let shard = seal_plain(html);
        let mut download = ChapterDownload::begin(&dir, &book, "2", 2).unwrap();
        write_part(&mut download, "e0", shard.as_bytes());
        download.commit().unwrap();
        let refs = super::chapter_image_refs(&dir, &book, 2);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].slot, 0);
        assert!(refs[0].url.contains("res.weread.qq.com"));

        let name = super::image_name(2, 0);
        assert!(is_fat83_component(&name), "{name}");
        let mut image = super::ImageDownload::begin(&dir, &book, 2, 0).unwrap();
        let bytes = b"not-a-real-image-but-stored";
        image.apply(DownloadEvent::BeginPart("img")).unwrap();
        image.apply(DownloadEvent::Chunk(bytes.to_vec())).unwrap();
        image.apply(DownloadEvent::EndPart).unwrap();
        image.commit().unwrap();
        assert!(super::image_cached(&dir, &book, 2, 0));
        let mut empty = super::ImageDownload::begin(&dir, &book, 2, 3).unwrap();
        empty.apply(DownloadEvent::BeginPart("img")).unwrap();
        empty.apply(DownloadEvent::EndPart).unwrap();
        assert!(empty.commit().unwrap_err().contains("empty"));
        assert!(!super::image_cached(&dir, &book, 2, 3));
        assert!(!super::book_dir(&dir, &book).join("IMG.TMP").exists());
        let stored = fs::read(super::book_dir(&dir, &book).join(&name)).unwrap();
        assert_eq!(stored, bytes);

        let mut too_big = super::ImageDownload::begin(&dir, &book, 2, 1).unwrap();
        too_big.apply(DownloadEvent::BeginPart("img")).unwrap();
        let piece = vec![b'z'; DOWNLOAD_CHUNK_BYTES];
        for _ in 0..(super::MAX_CHAPTER_IMAGE_BYTES / DOWNLOAD_CHUNK_BYTES) {
            too_big.apply(DownloadEvent::Chunk(piece.clone())).unwrap();
        }
        assert!(too_big
            .apply(DownloadEvent::Chunk(vec![1]))
            .unwrap_err()
            .contains("size limit"));
        too_big.abort();
        assert!(!super::book_dir(&dir, &book).join("IMG.TMP").exists());

        super::save_download_skip(&dir, &book, &[3]).unwrap();
        super::record_image_skip(&dir, &book, 2, 1).unwrap();
        assert_eq!(super::load_download_skip(&dir, &book), vec![3]);
        assert_eq!(super::load_image_skips(&dir, &book), vec![(2, 1)]);
        super::save_download_skip(&dir, &book, &[3, 9]).unwrap();
        assert_eq!(super::load_image_skips(&dir, &book), vec![(2, 1)]);
        super::mark_images_settled(&dir, &book, 2).unwrap();
        assert!(super::images_settled(&dir, &book, 2));
        let _ = fs::remove_dir_all(&dir);
    }
}
