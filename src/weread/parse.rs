//! Parsers for shelf, catalog, login, progress, highlights, and notes.

use crate::weread::{
    jsonutil::{
        self, array_objects, array_objects_scanned, errcode, is_truthy, object_i64, object_string,
    },
    limits::{
        MAX_CHAPTERS, MAX_CHAPTER_OBJECT_BYTES, MAX_FIELD_CHARS, MAX_ID_CHARS, MAX_JSON_BYTES,
        MAX_NOTES, MAX_NOTE_CHARS, MAX_SHELF_BOOKS, MAX_TITLE_CHARS,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShelfBook {
    pub book_id: String,
    pub title: String,
    pub author: String,
    pub cover: String,
    pub progress: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChapterMeta {
    pub uid: String,
    pub index: u32,
    pub title: String,
    pub word_count: u32,
    pub level: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookDetail {
    pub book_id: String,
    pub title: String,
    pub author: String,
    pub intro: String,
    pub cover: String,
    pub format: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingProgress {
    pub chapter_uid: String,
    pub chapter_offset: u32,
    pub progress: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteLine {
    pub kind: &'static str,
    pub chapter_uid: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginPoll {
    Pending,
    Otp,
    Expired,
    Failed(String),
    Success {
        vid: String,
        access_token: String,
        refresh_token: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseClass {
    Ok,
    Expired,
    Upgrade,
    Rejected,
}

pub fn classify(status: u16, body: &str) -> ResponseClass {
    if status == 401 || errcode(body) == Some(-2012) || body.contains("登录超时") {
        return ResponseClass::Expired;
    }
    if jsonutil::upgrade_message(body).is_some() {
        return ResponseClass::Upgrade;
    }
    if let Some(code) = errcode(body) {
        if code != 0 {
            return ResponseClass::Rejected;
        }
    }
    if !(200..300).contains(&status) {
        return ResponseClass::Rejected;
    }
    ResponseClass::Ok
}

pub fn parse_login_uid(json: &str) -> Option<String> {
    scalar_id(json, "uid")
        .or_else(|| jsonutil::object_raw(json, "data").and_then(|data| scalar_id(data, "uid")))
}

pub fn parse_login_poll(json: &str) -> LoginPoll {
    let data = jsonutil::object_raw(json, "data").unwrap_or(json);
    let logic = object_string(data, "logicCode", 40)
        .or_else(|| object_string(json, "logicCode", 40))
        .unwrap_or_default();
    match logic.as_str() {
        "NEED_OTP" | "OTP_EXPIRED" | "OTP_NOT_MATCH" => return LoginPoll::Otp,
        "LOGIN_TIMEOUT" => return LoginPoll::Expired,
        other if !other.is_empty() && !succeeded(json, data) => {
            return LoginPoll::Failed(other.to_string());
        }
        _ => {}
    }
    if !succeeded(json, data) {
        return LoginPoll::Pending;
    }
    let vid = ["webLoginVid", "vid", "userVid", "user_vid"]
        .iter()
        .find_map(|key| scalar_id(data, key).or_else(|| scalar_id(json, key)))
        .unwrap_or_default();
    let token = object_string(data, "accessToken", 256)
        .or_else(|| object_string(json, "accessToken", 256))
        .unwrap_or_default();
    let refresh = object_string(data, "refreshToken", 512)
        .or_else(|| object_string(json, "refreshToken", 512))
        .unwrap_or_default();
    if vid.is_empty() || token.is_empty() {
        LoginPoll::Failed("login response missed credentials".into())
    } else {
        LoginPoll::Success {
            vid,
            access_token: token,
            refresh_token: refresh,
        }
    }
}

pub fn parse_shelf(json: &str) -> Vec<ShelfBook> {
    let objects = array_objects_scanned(json, "books", MAX_SHELF_BOOKS, 4 * 1024);
    objects
        .into_iter()
        .filter_map(parse_shelf_book)
        .take(MAX_SHELF_BOOKS)
        .collect()
}

pub fn parse_book_detail(json: &str, fallback_id: &str) -> Option<BookDetail> {
    let object = jsonutil::object_raw(json, "data").unwrap_or(json);
    let nested = jsonutil::object_raw(object, "book").unwrap_or(object);
    let title = object_string(nested, "title", MAX_TITLE_CHARS)
        .or_else(|| object_string(object, "title", MAX_TITLE_CHARS))?;
    let book_id = scalar_id(nested, "bookId")
        .or_else(|| scalar_id(object, "bookId"))
        .unwrap_or_else(|| fallback_id.to_string());
    if !valid_id(&book_id) {
        return None;
    }
    Some(BookDetail {
        book_id,
        title,
        author: object_string(nested, "author", MAX_FIELD_CHARS).unwrap_or_default(),
        intro: object_string(nested, "intro", MAX_NOTE_CHARS).unwrap_or_default(),
        cover: normalize_cover(&object_string(nested, "cover", 300).unwrap_or_default()),
        format: object_string(object, "format", 16)
            .or_else(|| object_string(nested, "format", 16))
            .unwrap_or_default(),
    })
}

pub fn parse_chapters(json: &str) -> Vec<ChapterMeta> {
    if catalog_truncated(json) {
        return Vec::new();
    }
    let record = jsonutil::object_raw(json, "data")
        .and_then(|data| {
            jsonutil::objects_in_array(data, 1, MAX_JSON_BYTES)
                .into_iter()
                .next()
        })
        .unwrap_or(json);
    let chapters = if jsonutil::object_raw(record, "updated").is_some() {
        array_objects_scanned(record, "updated", MAX_CHAPTERS, MAX_CHAPTER_OBJECT_BYTES)
    } else {
        array_objects_scanned(record, "chapters", MAX_CHAPTERS, MAX_CHAPTER_OBJECT_BYTES)
    };
    let mut parsed: Vec<ChapterMeta> = chapters.into_iter().filter_map(parse_chapter).collect();
    parsed.sort_by_key(|chapter| chapter.index);
    parsed.truncate(MAX_CHAPTERS);
    parsed
}

/// A catalog cut off at the response cap is not an empty book.
#[must_use]
pub fn catalog_truncated(json: &str) -> bool {
    !json.trim().is_empty() && !jsonutil::document_complete(json)
}

pub fn parse_progress(json: &str) -> Option<ReadingProgress> {
    if catalog_truncated(json) {
        return None;
    }
    progress_candidates(json)
        .into_iter()
        .find_map(progress_from)
}

fn progress_candidates(json: &str) -> Vec<&str> {
    let mut candidates = Vec::new();
    if let Some(data) = jsonutil::object_raw(json, "data") {
        if let Some(book) = jsonutil::object_raw(data, "book") {
            candidates.push(book);
        }
        if let Some(reading) = jsonutil::object_raw(data, "readingProgress") {
            candidates.push(reading);
        }
        candidates.push(data);
    }
    if let Some(book) = jsonutil::object_raw(json, "book") {
        candidates.push(book);
    }
    if let Some(reading) = jsonutil::object_raw(json, "readingProgress") {
        candidates.push(reading);
    }
    candidates.push(json);
    candidates
}

fn progress_from(object: &str) -> Option<ReadingProgress> {
    let progress = object_i64(object, "progress")?.clamp(0, 100) as u8;
    Some(ReadingProgress {
        chapter_uid: object_i64(object, "chapterUid")
            .map(|value| value.to_string())
            .or_else(|| object_string(object, "chapterUid", 16))
            .unwrap_or_default(),
        chapter_offset: object_i64(object, "chapterOffset").unwrap_or(0).max(0) as u32,
        progress,
    })
}

pub fn parse_notes(json: &str) -> Vec<NoteLine> {
    let mut lines = Vec::new();
    for object in array_objects(json, "updated", MAX_NOTES, 4 * 1024) {
        if let Some(text) = object_string(object, "markText", MAX_NOTE_CHARS) {
            if text.is_empty() {
                continue;
            }
            lines.push(NoteLine {
                kind: "Highlight",
                chapter_uid: object_i64(object, "chapterUid")
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                text,
            });
        }
    }
    for object in array_objects(json, "reviews", MAX_NOTES, 8 * 1024) {
        let review = jsonutil::object_raw(object, "review").unwrap_or(object);
        if let Some(text) = object_string(review, "content", MAX_NOTE_CHARS) {
            if text.is_empty() {
                continue;
            }
            lines.push(NoteLine {
                kind: "Note",
                chapter_uid: object_string(review, "chapterName", MAX_TITLE_CHARS)
                    .unwrap_or_default(),
                text,
            });
        }
    }
    lines.truncate(MAX_NOTES);
    lines
}

pub fn renewal_succeeded(json: &str) -> bool {
    if classify(200, json) == ResponseClass::Expired {
        return false;
    }
    is_truthy(json, "succ") || object_i64(json, "succ") == Some(1)
}

fn parse_shelf_book(object: &str) -> Option<ShelfBook> {
    let nested = jsonutil::object_raw(object, "book").unwrap_or(object);
    let book_id = scalar_id(nested, "bookId").or_else(|| scalar_id(object, "bookId"))?;
    if !valid_id(&book_id) {
        return None;
    }
    Some(ShelfBook {
        book_id,
        title: object_string(nested, "title", MAX_TITLE_CHARS)
            .or_else(|| object_string(object, "title", MAX_TITLE_CHARS))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Untitled".into()),
        author: object_string(nested, "author", MAX_FIELD_CHARS).unwrap_or_default(),
        cover: normalize_cover(&object_string(nested, "cover", 300).unwrap_or_default()),
        progress: object_i64(nested, "progress")
            .or_else(|| object_i64(object, "progress"))
            .map(|value| value.clamp(0, 100) as u8),
    })
}

fn parse_chapter(object: &str) -> Option<ChapterMeta> {
    let uid = object_i64(object, "chapterUid")
        .map(|value| value.to_string())
        .or_else(|| object_string(object, "chapterUid", 16))?;
    if !valid_id(&uid) {
        return None;
    }
    let title =
        object_string(object, "title", MAX_TITLE_CHARS).unwrap_or_else(|| format!("Chapter {uid}"));
    if title.is_empty() {
        return None;
    }
    Some(ChapterMeta {
        uid,
        index: object_i64(object, "chapterIdx").unwrap_or(0).max(0) as u32,
        title,
        word_count: object_i64(object, "wordCount").unwrap_or(0).max(0) as u32,
        level: object_i64(object, "level").unwrap_or(1).clamp(0, 9) as u8,
    })
}

fn succeeded(json: &str, data: &str) -> bool {
    is_truthy(data, "succeed") || is_truthy(json, "succeed")
}

fn scalar_id(object: &str, key: &str) -> Option<String> {
    object_string(object, key, MAX_ID_CHARS).filter(|value| valid_id(value))
}

pub fn valid_id(value: &str) -> bool {
    let len = value.len();
    (1..=MAX_ID_CHARS).contains(&len)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn normalize_cover(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    let mut replaced = String::new();
    let bytes = url.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b't') {
            let mut end = index + 2;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end < bytes.len() && bytes[end] == b'_' && end > index + 2 {
                replaced.push_str("/t9_");
                index = end + 1;
                continue;
            }
        }
        replaced.push(bytes[index] as char);
        index += 1;
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::{
        classify, parse_book_detail, parse_chapters, parse_login_poll, parse_login_uid,
        parse_notes, parse_progress, parse_shelf, renewal_succeeded, LoginPoll, ResponseClass,
    };

    #[test]
    fn parses_login_shelf_catalog_and_notes() {
        assert_eq!(
            parse_login_uid(r#"{"data":{"uid":"uid_1"}}"#).as_deref(),
            Some("uid_1")
        );
        match parse_login_poll(
            r#"{"data":{"succeed":true,"webLoginVid":"9","accessToken":"tok","refreshToken":"rt"}}"#,
        ) {
            LoginPoll::Success {
                vid, access_token, ..
            } => {
                assert_eq!(vid, "9");
                assert_eq!(access_token, "tok");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            parse_login_poll(r#"{"logicCode":"NEED_OTP"}"#),
            LoginPoll::Otp
        );
        let shelf = parse_shelf(
            r#"{"books":[{"bookId":"43208843","title":"持续交付","author":"乔梁","cover":"https://cdn.weread.qq.com/t6_cover.jpg","progress":12},{"bookId":"../evil","title":"no"}]}"#,
        );
        assert_eq!(shelf.len(), 1);
        assert_eq!(shelf[0].progress, Some(12));
        assert!(shelf[0].cover.contains("/t9_"));
        let detail = parse_book_detail(
            r#"{"bookId":"43208843","title":"持续交付","author":"乔梁","intro":"简介"}"#,
            "43208843",
        )
        .unwrap();
        assert_eq!(detail.author, "乔梁");
        let chapters = parse_chapters(
            r#"{"data":[{"bookId":"43208843","format":"epub","updated":[{"chapterUid":2,"chapterIdx":2,"title":"第二章","wordCount":10,"level":1},{"chapterUid":1,"chapterIdx":1,"title":"第一章","wordCount":3,"level":1}]}]}"#,
        );
        assert_eq!(chapters[0].uid, "1");
        assert_eq!(chapters[1].title, "第二章");
        let progress =
            parse_progress(r#"{"book":{"chapterUid":2,"chapterOffset":15,"progress":1}}"#).unwrap();
        assert_eq!(progress.progress, 1);
        assert_eq!(progress.chapter_offset, 15);
        let notes = parse_notes(
            r#"{"updated":[{"bookmarkId":"b","chapterUid":2,"markText":"划线"}],"reviews":[{"review":{"content":"想法","chapterName":"第二章"}}]}"#,
        );
        assert_eq!(notes.len(), 2);
        assert!(renewal_succeeded(r#"{"succ":1}"#));
        assert_eq!(
            classify(200, r#"{"errcode":-2012}"#),
            ResponseClass::Expired
        );
    }

    #[test]
    fn chapter_objects_larger_than_two_kib_are_kept() {
        for pad in [3 * 1024, 20 * 1024] {
            let anchors = "a".repeat(pad);
            let json = format!(
                r#"{{"data":[{{"bookId":"43208843","updated":[{{"chapterUid":7,"chapterIdx":1,"title":"锚点章","wordCount":4,"level":1,"anchors":["{anchors}"]}}]}}]}}"#
            );
            assert!(
                json.len() > 2 * 1024,
                "fixture must exceed the old 2 KiB object cap"
            );
            let chapters = parse_chapters(&json);
            assert_eq!(chapters.len(), 1, "pad {pad}");
            assert_eq!(chapters[0].uid, "7");
            assert_eq!(chapters[0].title, "锚点章");
            assert!(!chapters[0].title.contains('a'));
        }
    }

    #[test]
    fn truncated_catalog_is_not_an_empty_chapter_list() {
        let cut = r#"{"updated":[{"chapterUid":1,"chapterIdx":1,"title":"第一章""#;
        assert!(super::catalog_truncated(cut));
        assert!(parse_chapters(cut).is_empty());
        assert!(!super::catalog_truncated(
            r#"{"updated":[{"chapterUid":1,"chapterIdx":1,"title":"第一章","wordCount":1,"level":1}]}"#
        ));
    }

    #[test]
    fn progress_is_read_from_book_info_and_nested_records() {
        let nested =
            parse_progress(r#"{"data":{"book":{"chapterUid":9,"chapterOffset":4,"progress":40}}}"#)
                .unwrap();
        assert_eq!(nested.chapter_uid, "9");
        assert_eq!(nested.chapter_offset, 4);
        assert_eq!(nested.progress, 40);
        let info = parse_progress(
            r#"{"bookId":"43208843","title":"持续交付","progress":12,"chapterUid":3,"chapterOffset":1}"#,
        )
        .unwrap();
        assert_eq!(info.progress, 12);
        assert_eq!(info.chapter_uid, "3");
        let reading = parse_progress(
            r#"{"readingProgress":{"chapterUid":"8","chapterOffset":2,"progress":55}}"#,
        )
        .unwrap();
        assert_eq!(reading.progress, 55);
        assert_eq!(reading.chapter_uid, "8");
        assert!(parse_progress(r#"{"data":{"book":{"progress":12,"chapterUid":1}"#).is_none());
    }

    #[test]
    fn shelf_progress_survives_a_book_object_over_four_kib() {
        let intro = "x".repeat(5 * 1024);
        let shelf = parse_shelf(&format!(
            r#"{{"books":[{{"bookId":"43208843","title":"持续交付","intro":"{intro}","progress":18}}]}}"#
        ));
        assert_eq!(shelf.len(), 1);
        assert_eq!(shelf[0].progress, Some(18));
    }
}
