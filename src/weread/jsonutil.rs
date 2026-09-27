//! Bounded JSON field scanner.
//!
//! WeRead payloads are untrusted. This scanner never builds a generic tree and
//! refuses to retain more than the caller-supplied character cap.

use crate::weread::limits::MAX_FIELD_CHARS;

#[derive(Debug)]
pub struct JsonError;

pub fn object_string(object: &str, key: &str, max_chars: usize) -> Option<String> {
    let raw = object_raw(object, key)?;
    let raw = raw.trim();
    if let Some(text) = raw.strip_prefix('"') {
        parse_json_string(text, max_chars).ok()
    } else if raw == "null" {
        None
    } else if is_number(raw) {
        Some(truncate_chars(raw, max_chars))
    } else {
        None
    }
}

pub fn object_i64(object: &str, key: &str) -> Option<i64> {
    let raw = object_raw(object, key)?.trim().to_string();
    let raw = raw.trim_matches('"');
    if raw.is_empty() || !is_number(raw) {
        return None;
    }
    raw.parse::<i64>().ok()
}

pub fn object_bool(object: &str, key: &str) -> Option<bool> {
    let raw = object_raw(object, key)?.trim().to_string();
    match raw.trim_matches('"') {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

pub fn is_truthy(object: &str, key: &str) -> bool {
    object_bool(object, key).unwrap_or(false)
        || object_i64(object, key).is_some_and(|value| value == 1)
}

/// Raw JSON value for a top-level key inside one object, including nested values.
pub fn object_raw<'a>(object: &'a str, key: &str) -> Option<&'a str> {
    let bytes = object.as_bytes();
    let mut index = skip_ws(bytes, 0);
    if index >= bytes.len() || bytes[index] != b'{' {
        return None;
    }
    index += 1;
    loop {
        index = skip_ws(bytes, index);
        if index >= bytes.len() {
            return None;
        }
        if bytes[index] == b'}' {
            return None;
        }
        if bytes[index] != b'"' {
            return None;
        }
        let (found, next) = parse_key(bytes, index).ok()?;
        index = skip_ws(bytes, next);
        if index >= bytes.len() || bytes[index] != b':' {
            return None;
        }
        index = skip_ws(bytes, index + 1);
        let value_start = index;
        index = skip_value(bytes, index, 0).ok()?;
        if found == key {
            return object.get(value_start..index);
        }
        index = skip_ws(bytes, index);
        if index < bytes.len() && bytes[index] == b',' {
            index += 1;
            continue;
        }
        if index < bytes.len() && bytes[index] == b'}' {
            return None;
        }
        return None;
    }
}

pub fn array_objects<'a>(
    json: &'a str,
    key: &str,
    max_objects: usize,
    max_object_bytes: usize,
) -> Vec<&'a str> {
    collect_array_objects(json, key, max_objects, max_object_bytes, false)
}

/// Like [`array_objects`], but objects larger than `max_object_bytes` are kept
/// as slices of the already-bounded response. Callers copy only the scalar
/// fields they need, so a chapter `anchors` array is not retained.
pub fn array_objects_scanned<'a>(
    json: &'a str,
    key: &str,
    max_objects: usize,
    max_object_bytes: usize,
) -> Vec<&'a str> {
    collect_array_objects(json, key, max_objects, max_object_bytes, true)
}

fn collect_array_objects<'a>(
    json: &'a str,
    key: &str,
    max_objects: usize,
    max_object_bytes: usize,
    scan_large: bool,
) -> Vec<&'a str> {
    let Some(raw) = object_raw(json, key)
        .or_else(|| object_raw(json, "data").and_then(|data| object_raw(data, key).or(Some(data))))
    else {
        return Vec::new();
    };
    objects_in_array_mode(raw, max_objects, max_object_bytes, scan_large)
}

/// True when `json` is one complete value with only trailing whitespace.
/// A response cut off at `MAX_JSON_BYTES` fails this check.
#[must_use]
pub fn document_complete(json: &str) -> bool {
    let bytes = json.as_bytes();
    let start = skip_ws(bytes, 0);
    if start >= bytes.len() {
        return false;
    }
    let Ok(end) = skip_value(bytes, start, 0) else {
        return false;
    };
    skip_ws(bytes, end) >= bytes.len()
}

pub fn objects_in_array<'a>(
    raw: &'a str,
    max_objects: usize,
    max_object_bytes: usize,
) -> Vec<&'a str> {
    objects_in_array_mode(raw, max_objects, max_object_bytes, false)
}

fn objects_in_array_mode<'a>(
    raw: &'a str,
    max_objects: usize,
    max_object_bytes: usize,
    scan_large: bool,
) -> Vec<&'a str> {
    let bytes = raw.as_bytes();
    let mut index = skip_ws(bytes, 0);
    if index >= bytes.len() || bytes[index] != b'[' {
        if index < bytes.len() && bytes[index] == b'{' {
            return vec![raw.trim()];
        }
        return Vec::new();
    }
    index += 1;
    let mut objects = Vec::new();
    while objects.len() < max_objects {
        index = skip_ws(bytes, index);
        if index >= bytes.len() || bytes[index] == b']' {
            break;
        }
        if bytes[index] != b'{' {
            let Ok(next) = skip_value(bytes, index, 0) else {
                break;
            };
            index = next;
        } else {
            let start = index;
            let Ok(next) = skip_value(bytes, index, 0) else {
                break;
            };
            let size = next.saturating_sub(start);
            if size <= max_object_bytes || scan_large {
                if let Some(slice) = raw.get(start..next) {
                    objects.push(slice);
                }
            }
            index = next;
        }
        index = skip_ws(bytes, index);
        if index < bytes.len() && bytes[index] == b',' {
            index += 1;
        }
    }
    objects
}

pub fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub fn errcode(json: &str) -> Option<i64> {
    object_i64(json, "errcode").or_else(|| object_i64(json, "errCode"))
}

pub fn upgrade_message(json: &str) -> Option<String> {
    let info = object_raw(json, "upgrade_info")?;
    object_string(info, "message", MAX_FIELD_CHARS)
        .or_else(|| object_string(json, "upgrade_info", MAX_FIELD_CHARS))
}

fn parse_key(bytes: &[u8], start: usize) -> Result<(String, usize), JsonError> {
    let (text, next) = scan_string(bytes, start, 64)?;
    Ok((text, next))
}

fn parse_json_string(after_quote: &str, max_chars: usize) -> Result<String, JsonError> {
    let mut input = String::from("\"");
    input.push_str(after_quote);
    let (text, _) = scan_string(input.as_bytes(), 0, max_chars)?;
    Ok(text)
}

fn scan_string(bytes: &[u8], start: usize, max_chars: usize) -> Result<(String, usize), JsonError> {
    if start >= bytes.len() || bytes[start] != b'"' {
        return Err(JsonError);
    }
    let mut index = start + 1;
    let mut out = String::new();
    let mut chars = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'"' {
            return Ok((out, index + 1));
        }
        if byte == b'\\' {
            index += 1;
            if index >= bytes.len() {
                return Err(JsonError);
            }
            let escaped = match bytes[index] {
                b'"' => '"',
                b'\\' => '\\',
                b'/' => '/',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'u' => {
                    if index + 4 >= bytes.len() {
                        return Err(JsonError);
                    }
                    let hex =
                        std::str::from_utf8(&bytes[index + 1..index + 5]).map_err(|_| JsonError)?;
                    let code = u32::from_str_radix(hex, 16).map_err(|_| JsonError)?;
                    index += 4;
                    char::from_u32(code).unwrap_or('\u{FFFD}')
                }
                _ => bytes[index] as char,
            };
            if chars < max_chars {
                out.push(escaped);
                chars += 1;
            }
            index += 1;
            continue;
        }
        let width = utf8_width(byte).ok_or(JsonError)?;
        if index + width > bytes.len() {
            return Err(JsonError);
        }
        let text = std::str::from_utf8(&bytes[index..index + width]).map_err(|_| JsonError)?;
        if chars < max_chars {
            out.push_str(text);
            chars += 1;
        }
        index += width;
    }
    Err(JsonError)
}

fn skip_value(bytes: &[u8], mut index: usize, depth: usize) -> Result<usize, JsonError> {
    if depth > 16 || index >= bytes.len() {
        return Err(JsonError);
    }
    index = skip_ws(bytes, index);
    if index >= bytes.len() {
        return Err(JsonError);
    }
    match bytes[index] {
        b'"' => {
            let (_, next) = scan_string(bytes, index, 0)?;
            Ok(next)
        }
        b'{' | b'[' => {
            let open = bytes[index];
            let close = if open == b'{' { b'}' } else { b']' };
            index += 1;
            loop {
                index = skip_ws(bytes, index);
                if index >= bytes.len() {
                    return Err(JsonError);
                }
                if bytes[index] == close {
                    return Ok(index + 1);
                }
                if open == b'{' {
                    if bytes[index] != b'"' {
                        return Err(JsonError);
                    }
                    let (_, next) = scan_string(bytes, index, 0)?;
                    index = skip_ws(bytes, next);
                    if index >= bytes.len() || bytes[index] != b':' {
                        return Err(JsonError);
                    }
                    index += 1;
                }
                index = skip_value(bytes, index, depth + 1)?;
                index = skip_ws(bytes, index);
                if index < bytes.len() && bytes[index] == b',' {
                    index += 1;
                }
            }
        }
        b't' => skip_literal(bytes, index, b"true"),
        b'f' => skip_literal(bytes, index, b"false"),
        b'n' => skip_literal(bytes, index, b"null"),
        b'-' | b'0'..=b'9' => {
            index += 1;
            while index < bytes.len()
                && matches!(bytes[index], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
            {
                index += 1;
            }
            Ok(index)
        }
        _ => Err(JsonError),
    }
}

fn skip_literal(bytes: &[u8], index: usize, literal: &[u8]) -> Result<usize, JsonError> {
    if bytes.get(index..index + literal.len()) == Some(literal) {
        Ok(index + literal.len())
    } else {
        Err(JsonError)
    }
}

fn skip_ws(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\n' | b'\r' | b'\t') {
        index += 1;
    }
    index
}

fn is_number(value: &str) -> bool {
    let mut chars = value.bytes();
    let Some(first) = chars.next() else {
        return false;
    };
    if first == b'-' && chars.next().is_none() {
        return false;
    }
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'.'))
        && value.bytes().any(|byte| byte.is_ascii_digit())
}

fn utf8_width(byte: u8) -> Option<usize> {
    if byte < 0x80 {
        Some(1)
    } else if byte & 0xE0 == 0xC0 {
        Some(2)
    } else if byte & 0xF0 == 0xE0 {
        Some(3)
    } else if byte & 0xF8 == 0xF0 {
        Some(4)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{array_objects, errcode, object_i64, object_string, truncate_chars};

    #[test]
    fn reads_nested_fields_and_stops_at_the_char_cap() {
        let json = r#"{"data":{"uid":"abc","n":12},"books":[{"bookId":"1","title":"你好世界"}]}"#;
        assert_eq!(object_string(json, "uid", 8), None);
        let data_books = array_objects(json, "books", 4, 200);
        assert_eq!(data_books.len(), 1);
        assert_eq!(
            object_string(data_books[0], "title", 2).as_deref(),
            Some("你好")
        );
        assert_eq!(object_i64(data_books[0], "bookId"), Some(1));
        assert_eq!(errcode(r#"{"errcode":-2012}"#), Some(-2012));
        assert_eq!(truncate_chars("abcdef", 3), "abc");
    }

    #[test]
    fn long_string_does_not_grow_past_the_cap() {
        let mut json = String::from(r#"{"title":""#);
        json.push_str(&"字".repeat(50));
        json.push_str(r#""}"#);
        let title = object_string(&json, "title", 4).unwrap();
        assert_eq!(title.chars().count(), 4);
    }
}
