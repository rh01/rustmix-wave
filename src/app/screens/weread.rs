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
        typography::{Text, UiTextRole},
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
                draw_bitmap(display, bitmap, 360, 252 + row as i32 * 58, 48, 48)?;
            }
        }
    }
    Text::new(
        &truncate(&weread.status, 46),
        Point::new(24, 690),
        state.display.detail_style(),
    )
    .draw(display)?;
    draw_footer(display, state.display, "MOVE  SELECT OPEN  HOLD BOOT BACK")
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
    draw_footer(display, state.display, "SELECT NEW QR  HOLD BOOT BACK")
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
    draw_footer(display, state.display, "MOVE  SELECT  HOLD BOOT BACK")
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
    draw_footer(display, state.display, "MOVE  SELECT OPEN  HOLD BOOT BACK")
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
    draw_header(display, state.display, "WEREAD", "CHAPTER")?;
    Text::new(
        &state.display.heading_style().truncate(title, 24, 420),
        Point::new(24, 128),
        state.display.body_style(),
    )
    .draw(display)?;
    let page_count = weread.pages.len().max(1);
    let page_label = format!("Page {} / {page_count}", weread.page_index + 1);
    Text::new(
        &page_label,
        Point::new(300, 128),
        state.display.detail_style(),
    )
    .draw(display)?;
    let mut top = 160;
    if weread.page_index == 0 {
        if let Some(bitmap) = weread.images.iter().find_map(|image| image.bitmap.as_ref()) {
            draw_bitmap(display, bitmap, 24, top, 432, 160)?;
            top += 168;
        }
    }
    let body = reader_body_style(
        state.reader.preferences.book_font,
        state.reader.preferences.font_size,
        state.reader.preferences.theme,
    );
    let line_step = i32::from(body.line_height()) + 2;
    if let Some(page) = weread.pages.get(weread.page_index) {
        for (index, line) in page.iter().enumerate() {
            let baseline = top + i32::from(body.line_height()) + index as i32 * line_step;
            if baseline > 700 {
                break;
            }
            Text::new(&line.text, Point::new(24, baseline), body).draw(display)?;
        }
    } else {
        Text::new(
            &truncate(&weread.status, 40),
            Point::new(24, 220),
            state.display.body_style(),
        )
        .draw(display)?;
    }
    draw_footer(
        display,
        state.display,
        "MOVE PAGE  SELECT BOOK  HOLD BOOT BACK",
    )
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
    draw_footer(display, state.display, "MOVE  HOLD BOOT BACK")
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
        "SELECT stops between chapters.",
        Point::new(24, 360),
        state.display.detail_style(),
    )
    .draw(display)?;
    draw_footer(display, state.display, "SELECT STOP  HOLD BOOT BACK")
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

fn draw_bitmap(
    display: &mut OrientedFrameBuffer<'_>,
    bitmap: &crate::weread::bitmap::MonoBitmap,
    left: i32,
    top: i32,
    max_width: u32,
    max_height: u32,
) -> Result<(), Infallible> {
    let width = u32::from(bitmap.width).min(max_width);
    let height = u32::from(bitmap.height).min(max_height);
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
    use super::window_start;

    #[test]
    fn toc_window_keeps_the_cursor_visible() {
        assert_eq!(window_start(0, 20, 8), 0);
        assert_eq!(window_start(10, 20, 8), 3);
        assert_eq!(window_start(19, 20, 8), 12);
    }
}
