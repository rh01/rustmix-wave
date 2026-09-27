//! XHTML/TXT cleanup, tiny ZIP HTML extraction, and Reader pagination.

use miniz_oxide::inflate::decompress_to_vec_with_limit;

use crate::{
    reader::{paginate_plain_text, ReaderLayout, ReaderPageLine},
    weread::limits::{MAX_CHAPTER_TEXT, MAX_PAGES},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextBlock {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub alt: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Text(TextBlock),
    Image(ImageRef),
}

#[must_use]
pub fn blocks_from_markup(input: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut text = String::new();
    let bytes = input.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() && text.len() + blocks.len() < MAX_CHAPTER_TEXT {
        if bytes[index] == b'<' {
            let end = bytes[index..]
                .iter()
                .position(|byte| *byte == b'>')
                .map(|offset| index + offset)
                .unwrap_or(bytes.len().saturating_sub(1));
            let tag = input.get(index..=end).unwrap_or("");
            let lower = tag.to_ascii_lowercase();
            if lower.starts_with("<img") {
                flush_text(&mut text, &mut blocks);
                blocks.push(Block::Image(ImageRef {
                    alt: attr_value(tag, "alt").unwrap_or_else(|| "image".into()),
                    url: attr_value(tag, "src").unwrap_or_default(),
                }));
            } else if lower.starts_with("<p")
                || lower.starts_with("</p")
                || lower.starts_with("<br")
                || lower.starts_with("<div")
                || lower.starts_with("</div")
                || lower.starts_with("<h")
                || lower.starts_with("</h")
            {
                push_break(&mut text);
            }
            index = end.saturating_add(1);
            continue;
        }
        if bytes[index] == b'&' {
            if let Some((ch, next)) = decode_entity(input, index) {
                text.push(ch);
                index = next;
                continue;
            }
        }
        let Some(ch) = input[index..].chars().next() else {
            break;
        };
        text.push(ch);
        index += ch.len_utf8();
        if text.len() >= MAX_CHAPTER_TEXT {
            break;
        }
    }
    flush_text(&mut text, &mut blocks);
    if blocks.is_empty() {
        blocks.push(Block::Text(TextBlock {
            text: String::new(),
        }));
    }
    blocks
}

pub fn paginate_blocks(blocks: &[Block], layout: ReaderLayout) -> Vec<Vec<ReaderPageLine>> {
    let mut pages = Vec::new();
    for block in blocks {
        if pages.len() >= MAX_PAGES {
            break;
        }
        match block {
            Block::Text(text) => {
                let portion = paginate_plain_text(&text.text, layout, MAX_PAGES - pages.len());
                pages.extend(portion);
            }
            Block::Image(image) => {
                let label = format!(
                    "[image: {}]",
                    image.alt.chars().take(40).collect::<String>()
                );
                let portion = paginate_plain_text(&label, layout, MAX_PAGES - pages.len());
                pages.extend(portion);
            }
        }
    }
    if pages.is_empty() {
        pages.push(vec![ReaderPageLine::new(String::new(), true)]);
    }
    pages.truncate(MAX_PAGES);
    pages
}

/// Extract HTML members from a non-ZIP64 archive. Stored and raw-deflate entries are accepted.
pub fn zip_html_text(bytes: &[u8]) -> Result<String, &'static str> {
    if bytes.len() < 30 || &bytes[0..4] != b"PK\x03\x04" {
        return Err("not a zip chapter");
    }
    let mut out = String::new();
    let mut index = 0usize;
    while index + 30 <= bytes.len() && &bytes[index..index + 4] == b"PK\x03\x04" {
        let method = u16::from_le_bytes([bytes[index + 8], bytes[index + 9]]);
        let flags = u16::from_le_bytes([bytes[index + 6], bytes[index + 7]]);
        let compressed =
            u32::from_le_bytes(bytes[index + 18..index + 22].try_into().unwrap()) as usize;
        let name_len = u16::from_le_bytes([bytes[index + 26], bytes[index + 27]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[index + 28], bytes[index + 29]]) as usize;
        let name_start = index + 30;
        let data_start = name_start
            .saturating_add(name_len)
            .saturating_add(extra_len);
        if name_start + name_len > bytes.len() || data_start > bytes.len() {
            break;
        }
        if flags & 0x08 != 0 || compressed > bytes.len() - data_start {
            return Err("zip entry is not size-bounded");
        }
        let name = String::from_utf8_lossy(&bytes[name_start..name_start + name_len]);
        let data_end = data_start + compressed;
        if data_end > bytes.len() {
            return Err("zip entry exceeds the buffer");
        }
        let payload = &bytes[data_start..data_end];
        if is_html_name(&name) {
            let plain = match method {
                0 => payload.to_vec(),
                8 => decompress_to_vec_with_limit(payload, MAX_CHAPTER_TEXT)
                    .map_err(|_| "zip deflate failed")?,
                _ => Vec::new(),
            };
            if plain.len() > MAX_CHAPTER_TEXT {
                return Err("zip html exceeds the chapter limit");
            }
            if let Ok(text) = std::str::from_utf8(&plain) {
                for block in blocks_from_markup(text) {
                    if let Block::Text(part) = block {
                        out.push_str(&part.text);
                        out.push('\n');
                    }
                }
            }
        }
        if out.len() >= MAX_CHAPTER_TEXT {
            break;
        }
        index = data_end;
    }
    if out.is_empty() {
        return Err("zip chapter had no html");
    }
    Ok(out)
}

pub fn plain_from_blocks(blocks: &[Block]) -> String {
    let mut text = String::new();
    for block in blocks {
        match block {
            Block::Text(part) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&part.text);
            }
            Block::Image(image) => {
                text.push_str("\n[image: ");
                text.push_str(&image.alt);
                text.push_str("]\n");
            }
        }
        if text.len() >= MAX_CHAPTER_TEXT {
            break;
        }
    }
    text.truncate(MAX_CHAPTER_TEXT);
    text
}

fn flush_text(text: &mut String, blocks: &mut Vec<Block>) {
    let cleaned = cleanup_text(text);
    text.clear();
    if !cleaned.is_empty() {
        blocks.push(Block::Text(TextBlock { text: cleaned }));
    }
}

fn push_break(text: &mut String) {
    if !text.ends_with('\n') {
        text.push('\n');
    }
}

fn cleanup_text(text: &str) -> String {
    let mut out = String::new();
    let mut newlines = 0u8;
    for ch in text.chars() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            newlines = newlines.saturating_add(1);
            if newlines <= 2 {
                out.push('\n');
            }
            continue;
        }
        if ch.is_control() {
            continue;
        }
        newlines = 0;
        out.push(ch);
        if out.len() >= MAX_CHAPTER_TEXT {
            break;
        }
    }
    out.trim().to_string()
}

fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let start = lower.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let body = &rest[quote.len_utf8()..];
        let end = body.find(quote)?;
        Some(body[..end].chars().take(120).collect())
    } else {
        None
    }
}

fn decode_entity(text: &str, index: usize) -> Option<(char, usize)> {
    let rest = &text[index..];
    let end = rest.find(';')?;
    if end > 10 {
        return None;
    }
    let body = &rest[1..end];
    let ch = if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
        char::from_u32(u32::from_str_radix(hex, 16).ok()?)?
    } else if let Some(decimal) = body.strip_prefix('#') {
        char::from_u32(decimal.parse().ok()?)?
    } else {
        match body {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "nbsp" => ' ',
            _ => return None,
        }
    };
    Some((ch, index + end + 1))
}

fn is_html_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".xhtml") || lower.ends_with(".html") || lower.ends_with(".htm")
}

#[cfg(test)]
mod tests {
    use super::{blocks_from_markup, paginate_blocks, plain_from_blocks, zip_html_text, Block};
    use crate::reader::ReaderPreferences;

    #[test]
    fn xhtml_keeps_cjk_paragraphs_and_image_refs() {
        let blocks = blocks_from_markup(
            r#"<p>你好&amp;世界</p><img alt="图1" src="https://res.weread.qq.com/a.jpg"><p>第二段</p>"#,
        );
        assert!(matches!(blocks[0], Block::Text(_)));
        assert!(matches!(blocks[1], Block::Image(_)));
        let text = plain_from_blocks(&blocks);
        assert!(text.contains("你好&世界"));
        assert!(text.contains("[image: 图1]"));
        assert!(text.contains("第二段"));
        let pages = paginate_blocks(&blocks, ReaderPreferences::default().layout());
        assert!(!pages.is_empty());
    }

    #[test]
    fn long_cjk_text_paginates_across_pages() {
        let text = "字".repeat(400);
        let blocks = blocks_from_markup(&text);
        let pages = paginate_blocks(&blocks, ReaderPreferences::default().layout());
        assert!(pages.len() > 1);
        assert!(pages.iter().all(|page| !page.is_empty()));
    }

    #[test]
    fn stored_zip_html_is_bounded_and_extracted() {
        let html = b"<p>Zip chapter</p>";
        let zip = stored_zip("OEBPS/c1.xhtml", html);
        let text = zip_html_text(&zip).unwrap();
        assert!(text.contains("Zip chapter"));
        let mut hostile = zip.clone();
        hostile[18..22].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(zip_html_text(&hostile).is_err());
    }

    fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
        let name = name.as_bytes();
        let mut out = Vec::new();
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(data);
        out
    }
}
