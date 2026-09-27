//! Whole-book cache on the SD card.
//!
//! Directory names are 8 hex characters so they stay FAT 8.3 safe. Chapter files
//! are 8 hex digits plus `.TXT`, which stays 8.3 for every `u32` index. Reads
//! refuse files larger than their cap before allocating.

use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use crate::weread::{
    limits::{MAX_CHAPTERS, MAX_CHAPTER_TEXT, MAX_META_BYTES, MAX_TITLE_CHARS},
    parse::{ChapterMeta, ReadingProgress},
    session::atomic_write,
};

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
    let bytes = read_capped(&path, MAX_CHAPTER_TEXT + 1024).ok()?;
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
    };
    use crate::weread::parse::ChapterMeta;
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
}
