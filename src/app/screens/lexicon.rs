//! Multilingual lexicon lookup, entry, and source-attribution screens.

use core::convert::Infallible;

use embedded_graphics::{
    pixelcolor::BinaryColor,
    prelude::{Drawable, Point, Primitive, Size},
    primitives::{PrimitiveStyle, Rectangle},
};

use crate::{
    app::{
        state::AppState,
        typography::Text,
        widgets::{
            footer::draw_footer,
            header::draw_header,
            status_row::{draw_status_row, StatusRow},
            wrap::wrap_ui,
        },
    },
    lexicon::STATIC_CREDITS,
    orientation::OrientedFrameBuffer,
};

pub fn render_lexicon(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let lexicon = &state.lexicon;
    let body = state.display.body_style();
    let heading = state.display.heading_style();
    let detail = state.display.detail_style();
    let dict = lexicon
        .current_dict()
        .map(|item| item.id.as_str())
        .unwrap_or("NONE");
    let query = if lexicon.query.is_empty() {
        "_".to_string()
    } else {
        lexicon.query.clone()
    };

    draw_header(display, state.display, "LEXICON", "RMXLEX1 LOOKUP")?;
    draw_status_row(
        display,
        state.display,
        StatusRow {
            left: dict,
            middle: lexicon.mode.label(),
            right: lexicon.navigation_mode_label(),
        },
    )?;
    Text::new(&truncate(&query, 28), Point::new(22, 158), heading).draw(display)?;
    Text::new(&truncate(&lexicon.message, 42), Point::new(22, 196), body).draw(display)?;

    let mut y = 230;
    if lexicon.focus == crate::lexicon::ui::InputFocus::Results {
        for (index, hit) in lexicon.hits.iter().take(4).enumerate() {
            let marker = if index == lexicon.hit_index { ">" } else { " " };
            Text::new(
                &truncate(&format!("{marker} {}", hit.label), 32),
                Point::new(22, y),
                if index == lexicon.hit_index {
                    heading
                } else {
                    body
                },
            )
            .draw(display)?;
            y += 28;
        }
    }

    draw_keys(display, lexicon, body, heading, y + 8)?;
    draw_footer(
        display,
        state.display,
        "UP/DOWN MOVE  BOOT H/V  SELECT  HOLD BOOT BACK",
    )?;
    let _ = detail;
    Ok(())
}

pub fn render_lexicon_entry(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let body = state.display.body_style();
    let heading = state.display.heading_style();
    let detail = state.display.detail_style();
    let lexicon = &state.lexicon;
    draw_header(display, state.display, "ENTRY", "LEXICON")?;
    draw_status_row(
        display,
        state.display,
        StatusRow {
            left: lexicon
                .entry
                .as_ref()
                .map(|entry| entry.dict_id.as_str())
                .unwrap_or("NONE"),
            middle: &format!("PAGE {}", lexicon.entry_page + 1),
            right: if lexicon.pronounce_ready { "♪" } else { "" },
        },
    )?;

    let lines = entry_lines(lexicon);
    let page = lexicon.entry_page;
    let slice = lines.iter().skip(page * 6).take(6);
    let mut y = 160;
    for (index, line) in slice.enumerate() {
        let style = if index == 0 && page == 0 {
            heading
        } else {
            body
        };
        for wrapped in wrap_ui(line, 430, style).into_iter().take(2) {
            Text::new(&wrapped, Point::new(22, y), style).draw(display)?;
            y += 28;
            if y > 700 {
                break;
            }
        }
    }
    if lexicon.entry.is_none() {
        Text::new("No entry open", Point::new(22, 180), heading).draw(display)?;
    }
    Text::new(&truncate(&lexicon.message, 42), Point::new(22, 720), detail).draw(display)?;
    draw_footer(
        display,
        state.display,
        "UP/DOWN PAGE  BOOT SAY  SELECT SAVE  HOLD BACK",
    )?;
    Ok(())
}

pub fn render_lexicon_sources(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let body = state.display.body_style();
    let heading = state.display.heading_style();
    let detail = state.display.detail_style();
    draw_header(display, state.display, "SOURCES", "LICENSES AND CREDITS")?;
    draw_status_row(
        display,
        state.display,
        StatusRow {
            left: "CREDITS",
            middle: "META",
            right: "CC BY-SA",
        },
    )?;
    let mut y = 150;
    for line in STATIC_CREDITS {
        for wrapped in wrap_ui(line, 430, detail).into_iter().take(2) {
            Text::new(&wrapped, Point::new(22, y), detail).draw(display)?;
            y += 24;
        }
    }
    y += 8;
    if let Some(dict) = state.lexicon.dicts.get(state.lexicon.source_index) {
        Text::new(&truncate(&dict.title, 28), Point::new(22, y), heading).draw(display)?;
        y += 32;
        for line in [
            format!("license {}", dict.license),
            format!("attribution {}", dict.attribution),
            format!("url {}", dict.source_url),
            format!("date {}", dict.source_date),
        ] {
            for wrapped in wrap_ui(&line, 430, body).into_iter().take(2) {
                Text::new(&wrapped, Point::new(22, y), body).draw(display)?;
                y += 26;
                if y > 720 {
                    break;
                }
            }
        }
    } else {
        Text::new("No lexicon on SD", Point::new(22, y), heading).draw(display)?;
        Text::new(
            "Credits above still apply to generated packs",
            Point::new(22, y + 36),
            body,
        )
        .draw(display)?;
    }
    draw_footer(display, state.display, "UP/DOWN DICT  HOLD BOOT BACK")?;
    Ok(())
}

fn draw_keys(
    display: &mut OrientedFrameBuffer<'_>,
    lexicon: &crate::lexicon::LexiconUiState,
    body: crate::app::typography::UiTextStyle,
    heading: crate::app::typography::UiTextStyle,
    top: i32,
) -> Result<(), Infallible> {
    let keys = lexicon.keys();
    let columns = lexicon.columns().max(1);
    let rows = keys.len().div_ceil(columns).max(1) as i32;
    let bottom = 730;
    let cell_h = ((bottom - top) / rows).clamp(16, 40);
    let cell_w = 436 / columns as i32;
    for (index, label) in keys.iter().enumerate() {
        let column = index % columns;
        let row = index / columns;
        let left = 22 + column as i32 * cell_w;
        let key_top = top + row as i32 * cell_h;
        if key_top + cell_h > 740 {
            break;
        }
        let selected = lexicon.navigation.selected() == index
            && lexicon.focus == crate::lexicon::ui::InputFocus::Keyboard;
        Rectangle::new(
            Point::new(left, key_top),
            Size::new((cell_w - 4).max(8) as u32, (cell_h - 4).max(8) as u32),
        )
        .into_styled(PrimitiveStyle::with_stroke(
            BinaryColor::On,
            if selected { 3 } else { 1 },
        ))
        .draw(display)?;
        Text::new(
            label,
            Point::new(left + 4, key_top + (cell_h * 2 / 3).min(22)),
            if selected { heading } else { body },
        )
        .draw(display)?;
    }
    Ok(())
}

fn entry_lines(lexicon: &crate::lexicon::LexiconUiState) -> Vec<String> {
    let Some(entry) = &lexicon.entry else {
        return Vec::new();
    };
    let mut lines = vec![
        entry.headword.clone(),
        format!("{} {}", entry.reading, entry.phonetic),
        entry.pos.clone(),
    ];
    lines.extend(entry.defs.iter().cloned());
    lines.extend(entry.examples.iter().cloned());
    lines
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut output: String = value.chars().take(max_chars.saturating_sub(3)).collect();
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use super::{render_lexicon, render_lexicon_entry, render_lexicon_sources};
    use crate::{app::AppState, framebuffer::FrameBuffer, orientation::OrientedFrameBuffer};

    #[test]
    fn lexicon_screens_render_without_sd() {
        let state = AppState::default();
        for render in [render_lexicon, render_lexicon_entry, render_lexicon_sources] {
            let mut frame = FrameBuffer::new_white();
            let mut display = OrientedFrameBuffer::new(&mut frame, Default::default());
            render(&mut display, &state).unwrap();
        }
    }
}
