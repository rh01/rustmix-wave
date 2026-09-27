//! XHTML/TXT cleanup, tiny ZIP HTML extraction, and Reader pagination.
//!
//! Image markers `[[weread-img:N|alt]]` survive a round trip through plain text
//! so an offline chapter can place the file saved beside it.

use miniz_oxide::inflate::decompress_to_vec_with_limit;

use crate::{
    reader::{paginate_plain_text, ReaderLayout, ReaderPageLine},
    weread::limits::{MAX_CHAPTER_TEXT, MAX_PAGES},
};

/// Top of the chapter body on the WeRead screen, under the title.
pub const BODY_TOP: i32 = 160;
/// Last y used for chapter body pixels, above the footer.
pub const BODY_BOTTOM: i32 = 700;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextBlock {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub alt: String,
    pub url: String,
    /// Position among the chapter's images. Matches the SD file slot.
    pub slot: u16,
}

/// Display size of one decoded image, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageMeasure {
    pub width: u16,
    pub height: u16,
}

/// One item in a paginated chapter page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlowItem {
    Line(ReaderPageLine),
    Image { slot: u16, width: u16, height: u16 },
}

impl FlowItem {
    #[must_use]
    pub fn line_text(&self) -> &str {
        match self {
            Self::Line(line) => line.text.as_str(),
            Self::Image { .. } => "",
        }
    }
}

/// Characters of real chapter text. Placeholders and pictures contribute nothing,
/// so a saved offset still lands on the same words after an image decodes.
#[must_use]
pub fn flow_text_chars(item: &FlowItem) -> usize {
    match item {
        FlowItem::Line(line) if is_image_placeholder(&line.text) => 0,
        FlowItem::Line(line) => line.text.chars().count(),
        FlowItem::Image { .. } => 0,
    }
}

/// Page whose text contains `offset`, counting only [`flow_text_chars`].
#[must_use]
pub fn page_for_text_offset(pages: &[Vec<FlowItem>], offset: u32) -> usize {
    if pages.is_empty() {
        return 0;
    }
    let mut seen = 0u32;
    for (index, page) in pages.iter().enumerate() {
        let chars = page
            .iter()
            .map(flow_text_chars)
            .fold(0u32, |sum, chars| sum.saturating_add(chars as u32));
        // An image-only page has no characters, so the following text shares
        // this offset. Stay on the picture instead of skipping to that text.
        if chars == 0 {
            if seen == offset {
                return index;
            }
        } else if seen.saturating_add(chars) > offset {
            return index;
        }
        seen = seen.saturating_add(chars);
    }
    pages.len() - 1
}

#[must_use]
pub fn is_image_placeholder(text: &str) -> bool {
    text.starts_with("[image:") && text.ends_with(']')
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
    let mut next_slot = 0u16;
    while index < bytes.len() && text.len() + blocks.len() < MAX_CHAPTER_TEXT {
        if let Some((slot, alt, consumed)) = parse_image_marker(&input[index..]) {
            flush_text(&mut text, &mut blocks);
            blocks.push(Block::Image(ImageRef {
                alt,
                url: String::new(),
                slot,
            }));
            next_slot = next_slot.max(slot.saturating_add(1));
            index += consumed;
            continue;
        }
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
                let slot = next_slot;
                next_slot = next_slot.saturating_add(1);
                blocks.push(Block::Image(ImageRef {
                    alt: attr_value(tag, "alt").unwrap_or_else(|| "image".into()),
                    url: attr_value(tag, "src").unwrap_or_default(),
                    slot,
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

/// Vertical step used by the chapter screen for one text line.
#[must_use]
pub fn line_step_for(layout: ReaderLayout) -> u32 {
    let style = crate::app::reader_typography::reader_body_style(
        layout.book_font,
        layout.font_size,
        crate::reader::ReadingTheme::Classic,
    );
    u32::from(style.line_height())
        .saturating_add(2)
        .saturating_add(u32::from(layout.line_spacing_px))
        .max(1)
}

/// Reader content box after margins, including immersive mode.
///
/// Portrait chrome is 432×594 and landscape chrome is 752×300. Immersive mode
/// uses the full panel. Width matches [`ReaderLayout::max_line_width_px`].
#[must_use]
pub fn content_box_px(layout: ReaderLayout) -> (u32, u32) {
    let width = u32::try_from(layout.max_line_width_px.max(1)).unwrap_or(u32::MAX);
    (width, content_height_px(layout))
}

/// Vertical room for one chapter page after margins.
#[must_use]
pub fn content_height_px(layout: ReaderLayout) -> u32 {
    use crate::reader::ReaderOrientation;
    let base = match (layout.immersive, layout.orientation) {
        (true, ReaderOrientation::Portrait) => 800i32,
        (true, ReaderOrientation::Landscape) => 480,
        (false, ReaderOrientation::Portrait) => 594,
        (false, ReaderOrientation::Landscape) => 300,
    };
    (base - i32::from(layout.margin_top_px) - i32::from(layout.margin_bottom_px)).max(80) as u32
}

/// Pixel height of one chapter page, in whole line steps, inside the content box.
#[must_use]
pub fn page_budget_px(layout: ReaderLayout) -> u32 {
    let step = line_step_for(layout);
    let visible = content_height_px(layout).max(step);
    let lines = (visible / step).max(1);
    lines * step
}

/// Placeholder shown when an image is missing, skipped, or too large to decode.
#[must_use]
pub fn image_placeholder(alt: &str) -> String {
    let label: String = if alt.is_empty() {
        "image".into()
    } else {
        alt.chars().take(40).collect()
    };
    format!("[image: {label}]")
}

/// Paginate text and images in reading order.
///
/// `measures` is indexed by [`ImageRef::slot`]. `None` becomes a one-line placeholder.
pub fn paginate_blocks(
    blocks: &[Block],
    layout: ReaderLayout,
    measures: &[Option<ImageMeasure>],
) -> Vec<Vec<FlowItem>> {
    let step = line_step_for(layout);
    let budget = page_budget_px(layout);
    let mut pack = PagePack {
        pages: Vec::new(),
        current: Vec::new(),
        used: 0,
        budget,
    };
    for block in blocks {
        if pack.pages.len() >= MAX_PAGES {
            break;
        }
        match block {
            Block::Text(text) => {
                for line in wrap_lines(&text.text, layout) {
                    if pack.pages.len() >= MAX_PAGES {
                        break;
                    }
                    pack.push(FlowItem::Line(line), step);
                }
            }
            Block::Image(image) => {
                if pack.pages.len() >= MAX_PAGES {
                    break;
                }
                if let Some(measure) = measures
                    .get(usize::from(image.slot))
                    .and_then(|measure| *measure)
                {
                    let height = u32::from(measure.height).clamp(1, budget);
                    pack.push(
                        FlowItem::Image {
                            slot: image.slot,
                            width: measure.width.max(1),
                            height: height as u16,
                        },
                        height,
                    );
                } else {
                    pack.push(
                        FlowItem::Line(ReaderPageLine::new(image_placeholder(&image.alt), true)),
                        step,
                    );
                }
            }
        }
    }
    pack.finish()
}

struct PagePack {
    pages: Vec<Vec<FlowItem>>,
    current: Vec<FlowItem>,
    used: u32,
    budget: u32,
}

impl PagePack {
    fn push(&mut self, item: FlowItem, cost: u32) {
        if self.pages.len() >= MAX_PAGES {
            return;
        }
        let cost = cost.max(1).min(self.budget);
        if self.used.saturating_add(cost) > self.budget && !self.current.is_empty() {
            self.flush();
        }
        if self.pages.len() >= MAX_PAGES {
            return;
        }
        self.current.push(item);
        self.used = self.used.saturating_add(cost);
    }

    fn flush(&mut self) {
        if self.current.is_empty() || self.pages.len() >= MAX_PAGES {
            self.current.clear();
            self.used = 0;
            return;
        }
        self.pages.push(std::mem::take(&mut self.current));
        self.used = 0;
    }

    fn finish(mut self) -> Vec<Vec<FlowItem>> {
        self.flush();
        if self.pages.is_empty() {
            self.pages
                .push(vec![FlowItem::Line(ReaderPageLine::new(String::new(), true))]);
        }
        self.pages.truncate(MAX_PAGES);
        self.pages
    }
}

fn wrap_lines(text: &str, layout: ReaderLayout) -> Vec<ReaderPageLine> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut wide = layout;
    wide.lines_per_page = 8_192;
    let mut lines = Vec::new();
    for page in paginate_plain_text(text, wide, MAX_PAGES) {
        for line in page {
            if lines.len() >= 8_192 {
                return lines;
            }
            lines.push(line);
        }
    }
    lines
}

/// Extract HTML members from a non-ZIP64 archive. Stored and raw-deflate entries are accepted.
pub fn zip_html_text(bytes: &[u8]) -> Result<String, &'static str> {
    let blocks = blocks_from_zip(bytes)?;
    let text = plain_from_blocks(&blocks);
    if text.is_empty() {
        return Err("zip chapter had no html");
    }
    Ok(text)
}

/// HTML members of a chapter zip, including inline image URLs.
pub fn blocks_from_zip(bytes: &[u8]) -> Result<Vec<Block>, &'static str> {
    if bytes.len() < 30 || &bytes[0..4] != b"PK\x03\x04" {
        return Err("not a zip chapter");
    }
    let mut out = Vec::new();
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
                out.extend(blocks_from_markup(text));
            }
        }
        if out.len() >= 4_096 {
            break;
        }
        index = data_end;
    }
    assign_image_slots(&mut out);
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
                text.push('\n');
                text.push_str(&image_marker(image.slot, &image.alt));
                text.push('\n');
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

fn assign_image_slots(blocks: &mut [Block]) {
    let mut slot = 0u16;
    for block in blocks {
        if let Block::Image(image) = block {
            image.slot = slot;
            slot = slot.saturating_add(1);
        }
    }
}

fn image_marker(slot: u16, alt: &str) -> String {
    format!("[[weread-img:{slot}|{}]]", marker_alt(alt))
}

fn marker_alt(alt: &str) -> String {
    alt.chars()
        .filter(|ch| *ch != '|' && *ch != '[' && *ch != ']' && !ch.is_control())
        .take(40)
        .collect()
}

fn parse_image_marker(input: &str) -> Option<(u16, String, usize)> {
    let rest = input.strip_prefix("[[weread-img:")?;
    let end = rest.find("]]")?;
    if end > 80 {
        return None;
    }
    let body = &rest[..end];
    let (slot, alt) = body.split_once('|').unwrap_or((body, ""));
    let slot = slot.parse().ok()?;
    let consumed = "[[weread-img:".len() + end + 2;
    Some((slot, alt.to_string(), consumed))
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
    use super::{
        blocks_from_markup, flow_text_chars, image_placeholder, page_for_text_offset,
        paginate_blocks, plain_from_blocks, zip_html_text, Block, FlowItem, ImageMeasure,
    };
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
        assert!(text.contains("[[weread-img:0|图1]]"));
        assert!(text.contains("第二段"));
        let again = blocks_from_markup(&text);
        assert!(matches!(again[1], Block::Image(_)));
        let pages = paginate_blocks(&blocks, ReaderPreferences::default().layout(), &[]);
        assert!(pages.iter().flatten().any(|item| {
            item.line_text().contains("你好")
                || item.line_text().contains(&image_placeholder("图1"))
        }));
    }

    #[test]
    fn images_stay_inline_and_move_to_the_next_page_when_they_do_not_fit() {
        let blocks = blocks_from_markup(
            "<p>Before the picture.</p><img alt=\"fig\" src=\"https://res.weread.qq.com/a.jpg\"><p>After the picture.</p>",
        );
        let layout = ReaderPreferences::default().layout();
        let short = paginate_blocks(
            &blocks,
            layout,
            &[Some(ImageMeasure {
                width: 40,
                height: 8,
            })],
        );
        let flat: Vec<&FlowItem> = short.iter().flatten().collect();
        let image_at = flat
            .iter()
            .position(|item| matches!(item, FlowItem::Image { .. }))
            .expect("image");
        assert!(flat[..image_at]
            .iter()
            .any(|item| item.line_text().contains("Before")));
        assert!(flat[image_at + 1..]
            .iter()
            .any(|item| item.line_text().contains("After")));
        assert_eq!(short.len(), 1, "a short image stays on the first page");

        let tall = paginate_blocks(
            &blocks,
            layout,
            &[Some(ImageMeasure {
                width: 40,
                height: 4_000,
            })],
        );
        assert!(tall.len() > 1);
        assert!(tall.iter().flatten().any(|item| matches!(
            item,
            FlowItem::Image { height, .. } if u32::from(*height) <= super::page_budget_px(layout)
        )));
        let missing = paginate_blocks(&blocks, layout, &[None]);
        assert!(missing
            .iter()
            .flatten()
            .any(|item| item.line_text().contains("[image: fig]")));

        let offset = text_chars_before(&tall, "After");
        assert_eq!(offset, text_chars_before(&missing, "After"));
        assert_ne!(offset, 0);
        let tall_page = page_for_text_offset(&tall, offset);
        assert!(
            tall[tall_page]
                .iter()
                .any(|item| matches!(item, FlowItem::Image { .. })),
            "a zero-character image page keeps the offset"
        );
        assert!(page_has(
            &missing,
            page_for_text_offset(&missing, offset),
            "After"
        ));
    }

    #[test]
    fn text_offset_stays_on_an_image_only_page() {
        let line = |text: &str| {
            FlowItem::Line(crate::reader::ReaderPageLine {
                text: text.into(),
                paragraph_end: true,
            })
        };
        let pages = vec![
            vec![line("Hi")],
            vec![FlowItem::Image {
                slot: 0,
                width: 8,
                height: 8,
            }],
            vec![line("Yo")],
        ];
        assert_eq!(page_for_text_offset(&pages, 2), 1);
        assert_eq!(page_for_text_offset(&pages, 0), 0);
        assert_eq!(page_for_text_offset(&pages, 3), 2);
    }

    fn text_chars_before(pages: &[Vec<FlowItem>], needle: &str) -> u32 {
        let mut seen = 0u32;
        for item in pages.iter().flatten() {
            if item.line_text().contains(needle) {
                return seen;
            }
            seen = seen.saturating_add(flow_text_chars(item) as u32);
        }
        seen
    }

    fn page_has(pages: &[Vec<FlowItem>], index: usize, needle: &str) -> bool {
        pages
            .get(index)
            .is_some_and(|page| page.iter().any(|item| item.line_text().contains(needle)))
    }

    #[test]
    fn long_cjk_text_paginates_across_pages() {
        let text = "字".repeat(400);
        let blocks = blocks_from_markup(&text);
        let pages = paginate_blocks(&blocks, ReaderPreferences::default().layout(), &[]);
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
