//! WeRead shelf, QR login, book, chapter, notes, and download screens.

use core::convert::Infallible;

use embedded_graphics::{
    pixelcolor::BinaryColor,
    prelude::{Drawable, Point, Primitive, Size},
    primitives::{PrimitiveStyle, Rectangle},
};

use crate::{
    app::{
        reader_typography::reader_body_style,
        state::AppState,
        typography::{Text, TextBounds, UiTextRole},
        widgets::{footer::draw_footer, header::draw_header},
    },
    orientation::OrientedFrameBuffer,
    weread::qr::qr_grid,
};

pub fn render_shelf(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let weread = &state.weread;
    draw_header(display, state.display, "WEREAD", "SHELF")?;
    let sign_in = if weread.session.web_signed_in() || weread.session.has_api_key() {
        "Refresh shelf"
    } else {
        "Sign in"
    };
    draw_row(
        display,
        state,
        132,
        weread.shelf_cursor == 0,
        sign_in,
        "",
        "QR",
    )?;
    draw_row(
        display,
        state,
        190,
        weread.shelf_cursor == 1,
        "Covers",
        "",
        if weread.session.covers { "ON" } else { "OFF" },
    )?;
    let start = shelf_start(weread.shelf_cursor, weread.books.len());
    if weread.books.is_empty() {
        Text::new(
            "No books yet. Sign in or add a wrk- key.",
            Point::new(26, 270),
            state.display.body_style(),
        )
        .draw(display)?;
    }
    for (offset, book) in weread.books.iter().enumerate().skip(start).take(6) {
        let row = offset - start;
        let selected = weread.shelf_cursor == offset + 2;
        let title = state.display.heading_style().truncate(&book.title, 18, 250);
        let badge = book
            .progress
            .map(|value| format!("{value}%"))
            .unwrap_or_default();
        draw_row(
            display,
            state,
            248 + row as i32 * 58,
            selected,
            &title,
            &badge,
            "OPEN",
        )?;
        if weread.session.covers {
            if let Some(bitmap) = weread.covers.get(offset).and_then(Option::as_ref) {
                draw_bitmap(display, bitmap, 360, 252 + row as i32 * 58, 48, 48, false)?;
            }
        }
    }
    Text::new(
        &truncate(&weread.status, 46),
        Point::new(24, 690),
        state.display.detail_style(),
    )
    .draw(display)?;
    draw_footer(
        display,
        state.display,
        footer(weread.busy, "MOVE  SELECT OPEN  HOLD BOOT BACK"),
    )
}

pub fn render_login(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    draw_header(display, state.display, "WEREAD", "WECHAT SIGN IN")?;
    if let Ok(grid) = qr_grid(&state.weread.qr_url) {
        let module = (360 / grid.size.max(1)).clamp(3, 8);
        let span = grid.size * module;
        let left = (480 - span as i32) / 2;
        let top = 150;
        for y in 0..grid.size {
            for x in 0..grid.size {
                if grid.dark_at(x, y) {
                    Rectangle::new(
                        Point::new(left + (x * module) as i32, top + (y * module) as i32),
                        Size::new(module as u32, module as u32),
                    )
                    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                    .draw(display)?;
                }
            }
        }
    }
    Text::new(
        &truncate(&state.weread.status, 42),
        Point::new(24, 680),
        state.display.body_style(),
    )
    .draw(display)?;
    draw_footer(
        display,
        state.display,
        footer(state.weread.busy, "SELECT NEW QR  HOLD BOOT BACK"),
    )
}

pub fn render_book(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let weread = &state.weread;
    let title = weread
        .detail
        .as_ref()
        .map(|detail| detail.title.as_str())
        .unwrap_or("Book");
    draw_header(display, state.display, "WEREAD", "BOOK")?;
    Text::new(
        &state.display.heading_style().truncate(title, 22, 420),
        Point::new(24, 130),
        state.display.heading_style(),
    )
    .draw(display)?;
    let author = weread
        .detail
        .as_ref()
        .map(|detail| detail.author.as_str())
        .unwrap_or("");
    Text::new(
        &truncate(author, 36),
        Point::new(24, 168),
        state.display.body_style(),
    )
    .draw(display)?;
    let progress = weread
        .progress
        .as_ref()
        .map(|progress| format!("{}%", progress.progress))
        .unwrap_or_else(|| "progress unknown".into());
    Text::new(&progress, Point::new(24, 206), state.display.detail_style()).draw(display)?;
    for (index, label) in ["Read", "Contents", "Notes", "Download", "Refresh"]
        .into_iter()
        .enumerate()
    {
        draw_row(
            display,
            state,
            236 + index as i32 * 58,
            weread.book_cursor == index,
            label,
            "",
            "SELECT",
        )?;
    }
    Text::new(
        &truncate(&weread.status, 46),
        Point::new(24, 690),
        state.display.detail_style(),
    )
    .draw(display)?;
    draw_footer(
        display,
        state.display,
        footer(weread.busy, "MOVE  SELECT  HOLD BOOT BACK"),
    )
}

pub fn render_toc(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    draw_header(display, state.display, "WEREAD", "CONTENTS")?;
    let weread = &state.weread;
    if weread.chapters.is_empty() {
        Text::new(
            "No chapters.",
            Point::new(24, 180),
            state.display.body_style(),
        )
        .draw(display)?;
    }
    let start = window_start(weread.toc_cursor, weread.chapters.len(), 8);
    for (offset, chapter) in weread.chapters.iter().enumerate().skip(start).take(8) {
        let row = offset - start;
        let title = state
            .display
            .heading_style()
            .truncate(&chapter.title, 22, 360);
        draw_row(
            display,
            state,
            132 + row as i32 * 58,
            weread.toc_cursor == offset,
            &title,
            "",
            "OPEN",
        )?;
    }
    draw_footer(
        display,
        state.display,
        footer(weread.busy, "MOVE  SELECT OPEN  HOLD BOOT BACK"),
    )
}

fn render_immersive_chapter(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
    weread: &crate::weread::WereadUi,
    _title: &str,
    dark: bool,
    layout: crate::reader::ReaderLayout,
) -> Result<(), Infallible> {
    let size = display.orientation().logical_size();
    let width = size.width as i32;
    let height = size.height as i32;
    let body = reader_body_style(
        state.reader.preferences.book_font,
        state.reader.preferences.font_size,
        state.reader.preferences.theme,
    )
    .with_tracking(state.reader.preferences.letter_spacing.pixels())
    .with_color(if dark {
        BinaryColor::Off
    } else {
        BinaryColor::On
    });
    let bounds = TextBounds::new(
        i32::from(layout.margin_left_px),
        i32::from(layout.margin_top_px),
        width - i32::from(layout.margin_right_px),
        height - i32::from(layout.margin_bottom_px),
    );
    if weread.pages.get(weread.page_index).is_some() {
        draw_chapter_flow(display, state, weread, dark, layout, body, bounds)?;
    } else {
        let ink = if dark {
            state.display.text_style(UiTextRole::Body, BinaryColor::Off)
        } else {
            state.display.body_style()
        };
        Text::new(
            &truncate(&weread.status, 40),
            Point::new(left, top + 48),
            ink,
        )
        .draw(display)?;
    }
    if state.reading_status_overlay {
        let progress = weread_progress_label(
            &state.reader.preferences,
            weread.page_index,
            weread.pages.len(),
            weread.chapter_pos,
            weread.chapters.len(),
            &state.board.time_label(state.regional),
            &state.board.battery_label(),
        );
        let label = if progress.is_empty() {
            "SELECT menu".to_string()
        } else {
            format!("{progress}  SELECT menu")
        };
        super::reader::draw_immersive_status(display, state, &label)?;
    }
    Ok(())
}

pub fn render_read(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let weread = &state.weread;
    let title = weread
        .chapters
        .get(weread.chapter_pos)
        .map(|chapter| chapter.title.as_str())
        .or_else(|| weread.detail.as_ref().map(|detail| detail.title.as_str()))
        .unwrap_or("WeRead");
    let dark = state.reader.preferences.dark_mode;
    if dark {
        let size = display.orientation().logical_size();
        Rectangle::new(Point::new(0, 0), Size::new(size.width, size.height))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(display)?;
    }
    let chrome = if dark {
        state.display.text_style(UiTextRole::Body, BinaryColor::Off)
    } else {
        state.display.body_style()
    };
    let layout = state.reader.preferences.layout();
    if layout.immersive {
        return render_immersive_chapter(display, state, weread, title, dark, layout);
    }
    draw_header(display, state.display, "WEREAD", "CHAPTER")?;
    Text::new(
        &state.display.heading_style().truncate(title, 24, 420),
        Point::new(24, 128),
        chrome,
    )
    .draw(display)?;
    let progress = weread_progress_label(
        &state.reader.preferences,
        weread.page_index,
        weread.pages.len(),
        weread.chapter_pos,
        weread.chapters.len(),
        &state.board.time_label(state.regional),
        &state.board.battery_label(),
    );
    if !progress.is_empty() {
        let shown = chrome.truncate(&progress, 22, 168);
        Text::new(&shown, Point::new(300, 128), chrome).draw(display)?;
    }
    let body = reader_body_style(
        state.reader.preferences.book_font,
        state.reader.preferences.font_size,
        state.reader.preferences.theme,
    )
    .with_tracking(state.reader.preferences.letter_spacing.pixels())
    .with_color(if dark {
        BinaryColor::Off
    } else {
        BinaryColor::On
    });
    let size = display.orientation().logical_size();
    let width = size.width as i32;
    let landscape = width > size.height as i32;
    let left = if landscape {
        24 + i32::from(layout.margin_left_px)
    } else {
        24 + i32::from(layout.margin_left_px)
    };
    let right = width - 24 - i32::from(layout.margin_right_px);
    let top = if landscape {
        88 + i32::from(layout.margin_top_px)
    } else {
        160 + i32::from(layout.margin_top_px)
    };
    let bottom = top + crate::weread::text::content_height_px(layout) as i32;
    let bounds = TextBounds::new(left, top, right, bottom);
    if weread.pages.get(weread.page_index).is_some() {
        draw_chapter_flow(display, state, weread, dark, layout, body, bounds)?;
    } else {
        Text::new(&truncate(&weread.status, 40), Point::new(24, 220), chrome).draw(display)?;
    }
    if dark {
        Text::new(
            footer(weread.busy, "MOVE PAGE  SELECT PREFS  HOLD BOOT BOOK"),
            Point::new(24, 760),
            chrome,
        )
        .draw(display)?;
        Ok(())
    } else {
        draw_footer(
            display,
            state.display,
            footer(weread.busy, "MOVE PAGE  SELECT PREFS  HOLD BOOT BOOK"),
        )
    }
}

pub fn render_notes(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    draw_header(display, state.display, "WEREAD", "HIGHLIGHTS AND NOTES")?;
    let weread = &state.weread;
    if weread.notes.is_empty() {
        Text::new(
            &truncate(&weread.status, 42),
            Point::new(24, 180),
            state.display.body_style(),
        )
        .draw(display)?;
    }
    let start = window_start(weread.note_cursor, weread.notes.len(), 7);
    for (offset, note) in weread.notes.iter().enumerate().skip(start).take(7) {
        let row = offset - start;
        let label = state.display.heading_style().truncate(&note.text, 24, 340);
        draw_row(
            display,
            state,
            132 + row as i32 * 70,
            weread.note_cursor == offset,
            &label,
            note.kind,
            "",
        )?;
    }
    draw_footer(
        display,
        state.display,
        footer(weread.busy, "MOVE  HOLD BOOT BACK"),
    )
}

pub fn render_download(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    draw_header(display, state.display, "WEREAD", "OFFLINE DOWNLOAD")?;
    let weread = &state.weread;
    let total = weread.chapters.len().max(1);
    let percent = ((weread.download_done.min(total) * 100) / total) as u8;
    Text::new(
        &format!(
            "Saved {} of {} chapters.",
            weread.download_done,
            weread.chapters.len()
        ),
        Point::new(24, 180),
        state.display.body_style(),
    )
    .draw(display)?;
    Rectangle::new(Point::new(24, 230), Size::new(432, 38))
        .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 2))
        .draw(display)?;
    Rectangle::new(Point::new(30, 236), Size::new(4 * percent as u32, 26))
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(display)?;
    Text::new(
        &truncate(&weread.status, 42),
        Point::new(24, 310),
        state.display.body_style(),
    )
    .draw(display)?;
    Text::new(
        "SELECT cancels the current request.",
        Point::new(24, 360),
        state.display.detail_style(),
    )
    .draw(display)?;
    draw_footer(
        display,
        state.display,
        footer(weread.busy, "SELECT STOP  HOLD BOOT BACK"),
    )
}

fn footer<'a>(busy: bool, idle: &'a str) -> &'a str {
    if busy {
        "WORKING  SELECT CANCEL"
    } else {
        idle
    }
}

fn shelf_start(cursor: usize, books: usize) -> usize {
    if cursor < 2 {
        0
    } else {
        window_start(cursor - 2, books, 6)
    }
}

fn window_start(cursor: usize, len: usize, window: usize) -> usize {
    if len <= window || cursor < window {
        0
    } else if cursor + 1 >= len {
        len - window
    } else {
        cursor + 1 - window
    }
}

fn draw_row(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
    top: i32,
    selected: bool,
    label: &str,
    badge: &str,
    suffix: &str,
) -> Result<(), Infallible> {
    let body = if selected {
        state.display.text_style(UiTextRole::Body, BinaryColor::Off)
    } else {
        state.display.body_style()
    };
    let style = if selected {
        PrimitiveStyle::with_fill(BinaryColor::On)
    } else {
        PrimitiveStyle::with_stroke(BinaryColor::On, 1)
    };
    Rectangle::new(Point::new(20, top), Size::new(440, 50))
        .into_styled(style)
        .draw(display)?;
    Text::new(label, Point::new(32, top + 32), body).draw(display)?;
    Text::new(badge, Point::new(300, top + 32), body).draw(display)?;
    Text::new(suffix, Point::new(390, top + 32), body).draw(display)?;
    Ok(())
}

fn weread_progress_label(
    prefs: &crate::reader::ReaderPreferences,
    page_index: usize,
    page_count: usize,
    chapter_index: usize,
    chapter_count: usize,
    time: &str,
    battery: &str,
) -> String {
    let mut parts = Vec::new();
    if prefs.status_page && page_count > 0 {
        let page = page_index.min(page_count - 1) + 1;
        parts.push(format!("Page {page} / {page_count}"));
    }
    if prefs.status_chapter && chapter_count > 0 {
        parts.push(format!(
            "CH {}/{chapter_count}",
            chapter_index.saturating_add(1)
        ));
    }
    if prefs.status_time && !time.is_empty() {
        parts.push(time.to_string());
    }
    if prefs.status_battery && !battery.is_empty() {
        parts.push(battery.to_string());
    }
    parts.join("  ")
}

fn draw_chapter_flow(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
    weread: &crate::weread::WereadUi,
    dark: bool,
    layout: crate::reader::ReaderLayout,
    body: crate::app::typography::UiTextStyle,
    bounds: TextBounds,
) -> Result<(), Infallible> {
    let Some(page) = weread.pages.get(weread.page_index) else {
        return Ok(());
    };
    let line_step = crate::weread::text::line_step_for(layout) as i32;
    let mut top = bounds.top;
    for item in page {
        if top >= bounds.bottom {
            break;
        }
        match item {
            crate::weread::text::FlowItem::Line(line) => {
                let baseline = top + i32::from(body.line_height());
                if baseline > bounds.bottom {
                    break;
                }
                let mut line_bounds = bounds;
                line_bounds.top = top;
                if line.first_line_indent {
                    line_bounds.left += layout.indent_px();
                }
                let shown = state.reader.preferences.display_line(&line.text);
                let (rendered, x) = super::reader::aligned_reader_line(
                    shown.as_str(),
                    line.paragraph_end,
                    state.reader.preferences.effective_alignment(),
                    body,
                    line_bounds,
                );
                Text::new(rendered.as_str(), Point::new(x, baseline), body).draw(display)?;
                top += line_step;
            }
            crate::weread::text::FlowItem::Image {
                slot,
                width,
                height,
            } => {
                let width = u32::from(*width);
                let height = u32::from(*height);
                if let Some(bitmap) = weread
                    .images
                    .get(usize::from(*slot))
                    .and_then(|image| image.bitmap.as_ref())
                {
                    draw_scaled_bitmap(display, bitmap, bounds.left, top, width, height, dark)?;
                }
                top += height as i32;
            }
        }
    }
    Ok(())
}

fn draw_scaled_bitmap(
    display: &mut OrientedFrameBuffer<'_>,
    bitmap: &crate::weread::bitmap::MonoBitmap,
    left: i32,
    top: i32,
    dest_width: u32,
    dest_height: u32,
    dark: bool,
) -> Result<(), Infallible> {
    if dest_width == 0 || dest_height == 0 || bitmap.width == 0 || bitmap.height == 0 {
        return Ok(());
    }
    if dark {
        Rectangle::new(Point::new(left, top), Size::new(dest_width, dest_height))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
            .draw(display)?;
    }
    for y in 0..dest_height {
        for x in 0..dest_width {
            let source_x = (x * u32::from(bitmap.width) / dest_width) as u16;
            let source_y = (y * u32::from(bitmap.height) / dest_height) as u16;
            if bitmap.bit(source_x, source_y) {
                Rectangle::new(Point::new(left + x as i32, top + y as i32), Size::new(1, 1))
                    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                    .draw(display)?;
            }
        }
    }
    Ok(())
}

fn draw_bitmap(
    display: &mut OrientedFrameBuffer<'_>,
    bitmap: &crate::weread::bitmap::MonoBitmap,
    left: i32,
    top: i32,
    max_width: u32,
    max_height: u32,
    dark: bool,
) -> Result<(), Infallible> {
    let width = u32::from(bitmap.width).min(max_width);
    let height = u32::from(bitmap.height).min(max_height);
    if dark && width > 0 && height > 0 {
        Rectangle::new(Point::new(left, top), Size::new(width, height))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
            .draw(display)?;
    }
    for y in 0..height {
        for x in 0..width {
            if bitmap.bit(x as u16, y as u16) {
                Rectangle::new(Point::new(left + x as i32, top + y as i32), Size::new(1, 1))
                    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                    .draw(display)?;
            }
        }
    }
    Ok(())
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.into();
    }
    let mut output: String = value.chars().take(max_chars.saturating_sub(3)).collect();
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use super::{weread_progress_label, window_start};

    #[test]
    fn toc_window_keeps_the_cursor_visible() {
        assert_eq!(window_start(0, 20, 8), 0);
        assert_eq!(window_start(10, 20, 8), 3);
        assert_eq!(window_start(19, 20, 8), 12);
    }

    #[test]
    fn weread_progress_honors_status_toggles() {
        let mut prefs = crate::reader::ReaderPreferences::default();
        let label = weread_progress_label(&prefs, 0, 4, 1, 9, "12:00", "80%");
        assert_eq!(label, "Page 1 / 4  CH 2/9");
        prefs.status_page = false;
        prefs.status_chapter = false;
        prefs.status_time = true;
        prefs.status_battery = true;
        assert_eq!(
            weread_progress_label(&prefs, 2, 4, 1, 9, "12:00", "80%"),
            "12:00  80%"
        );
        prefs.status_time = false;
        prefs.status_battery = false;
        assert!(weread_progress_label(&prefs, 2, 4, 1, 9, "12:00", "80%").is_empty());
    }

    #[test]
    fn dark_mode_chapter_image_keeps_a_white_backing() {
        use embedded_graphics::{
            pixelcolor::BinaryColor,
            prelude::{Drawable, OriginDimensions, Point, Primitive},
            primitives::{PrimitiveStyle, Rectangle},
        };
        use image::{codecs::png::PngEncoder, GrayImage, ImageEncoder};

        use crate::{
            framebuffer::FrameBuffer,
            orientation::{DisplayOrientation, OrientedFrameBuffer},
        };

        let gray = GrayImage::from_raw(8, 1, vec![0, 255, 255, 255, 255, 255, 255, 255])
            .expect("gray image");
        let mut png = Vec::new();
        PngEncoder::new(std::io::Cursor::new(&mut png))
            .write_image(gray.as_raw(), 8, 1, image::ColorType::L8)
            .expect("png");
        let bitmap = crate::weread::bitmap::decode_mono(&png, 8, 1).expect("bitmap");
        assert!(bitmap.bit(0, 0));
        assert!(!bitmap.bit(1, 0));

        let mut frame = FrameBuffer::new_white();
        {
            let mut display = OrientedFrameBuffer::new(&mut frame, DisplayOrientation::Landscape);
            Rectangle::new(Point::new(0, 0), display.size())
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(&mut display)
                .expect("fill");
            super::draw_bitmap(&mut display, &bitmap, 0, 0, 8, 1, true).expect("bitmap");
        }
        let packed = frame.as_bytes()[0];
        assert_eq!(packed & 0x80, 0, "ink pixel stays black");
        assert_eq!(packed & 0x40, 0x40, "white backing shows the clear pixel");
    }
}
