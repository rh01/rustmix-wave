//! Vocabulary deck list, flip card, and review statistics.

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
    orientation::OrientedFrameBuffer,
    vocab::ui::{CardFace, RATINGS},
};

pub fn render_vocab(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let vocab = &state.vocab;
    let body = state.display.body_style();
    let heading = state.display.heading_style();
    let (due, new_cap) = vocab.due_and_new_counts();
    draw_header(display, state.display, "VOCABULARY", "FSRS-6 / SM-2")?;
    draw_status_row(
        display,
        state.display,
        StatusRow {
            left: if vocab.today.is_some() {
                "READY"
            } else {
                "NO CLOCK"
            },
            middle: &format!("due {due}"),
            right: &format!("new {new_cap}"),
        },
    )?;
    Text::new(&truncate(&vocab.message, 40), Point::new(22, 150), body).draw(display)?;
    let mut y = 190;
    let rows = vocab.decks.len() + 1;
    for index in 0..rows.min(8) {
        let selected = index == vocab.deck_index;
        let label = if index < vocab.decks.len() {
            format!("{} {}", vocab.decks[index].name, vocab.decks[index].title)
        } else {
            "Stats".to_string()
        };
        Rectangle::new(Point::new(18, y - 22), Size::new(444, 36))
            .into_styled(PrimitiveStyle::with_stroke(
                BinaryColor::On,
                if selected { 3 } else { 1 },
            ))
            .draw(display)?;
        Text::new(
            &truncate(&label, 28),
            Point::new(28, y),
            if selected { heading } else { body },
        )
        .draw(display)?;
        y += 48;
    }
    draw_footer(
        display,
        state.display,
        "UP/DOWN DECK  SELECT START  HOLD BOOT BACK",
    )?;
    Ok(())
}

pub fn render_vocab_session(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let vocab = &state.vocab;
    let body = state.display.body_style();
    let heading = state.display.heading_style();
    let large = state.display.large_style();
    let face = match vocab.face {
        CardFace::Front => "FRONT",
        CardFace::Back => "BACK",
    };
    draw_header(display, state.display, "REVIEW", face)?;
    draw_status_row(
        display,
        state.display,
        StatusRow {
            left: vocab.settings.algo.label(),
            middle: &format!("{}", vocab.completed_today),
            right: face,
        },
    )?;
    let prompt = vocab
        .current_item()
        .map(|item| {
            let mark = if vocab.pronounce_ready { " ♪" } else { "" };
            format!("slot {} #{}{mark}", item.dict_slot, item.entry_id)
        })
        .unwrap_or_else(|| "Session complete".into());
    Text::new(&truncate(&prompt, 24), Point::new(22, 180), large).draw(display)?;
    Text::new(&truncate(&vocab.message, 40), Point::new(22, 230), body).draw(display)?;
    if vocab.face == CardFace::Back {
        let mut y = 300;
        for (index, rating) in RATINGS.iter().enumerate() {
            let selected = index == vocab.rating_cursor;
            Rectangle::new(Point::new(22, y - 28), Size::new(200, 40))
                .into_styled(PrimitiveStyle::with_stroke(
                    BinaryColor::On,
                    if selected { 3 } else { 1 },
                ))
                .draw(display)?;
            Text::new(
                rating,
                Point::new(36, y),
                if selected { heading } else { body },
            )
            .draw(display)?;
            y += 56;
        }
    } else {
        Text::new("SELECT flips the card", Point::new(22, 320), heading).draw(display)?;
    }
    draw_footer(
        display,
        state.display,
        "BOOT SAY  SELECT FLIP/RATE  UP/DOWN RATING",
    )?;
    Ok(())
}

pub fn render_vocab_stats(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let summary = state.vocab.stats();
    let body = state.display.body_style();
    let heading = state.display.heading_style();
    draw_header(display, state.display, "STATS", "VOCABULARY")?;
    draw_status_row(
        display,
        state.display,
        StatusRow {
            left: "TODAY",
            middle: &format!("{}", summary.reviewed_today),
            right: &format!("streak {}", summary.streak),
        },
    )?;
    let lines = [
        format!("new {}", summary.new_count),
        format!("learning {}", summary.learning),
        format!("review {}", summary.review),
        format!("relearning {}", summary.relearning),
        format!(
            "next 7 days {} {} {} {} {} {} {}",
            summary.due_next_7[0],
            summary.due_next_7[1],
            summary.due_next_7[2],
            summary.due_next_7[3],
            summary.due_next_7[4],
            summary.due_next_7[5],
            summary.due_next_7[6],
        ),
        state.vocab.message.clone(),
    ];
    let mut y = 170;
    for (index, line) in lines.iter().enumerate() {
        let style = if index == 0 { heading } else { body };
        for wrapped in wrap_ui(line, 430, style).into_iter().take(2) {
            Text::new(&wrapped, Point::new(22, y), style).draw(display)?;
            y += 36;
        }
    }
    draw_footer(display, state.display, "HOLD BOOT BACK")?;
    Ok(())
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
    use super::{render_vocab, render_vocab_session, render_vocab_stats};
    use crate::{app::AppState, framebuffer::FrameBuffer, orientation::OrientedFrameBuffer};

    #[test]
    fn vocab_screens_render_without_sd_or_clock() {
        let state = AppState::default();
        for render in [render_vocab, render_vocab_session, render_vocab_stats] {
            let mut frame = FrameBuffer::new_white();
            let mut display = OrientedFrameBuffer::new(&mut frame, Default::default());
            render(&mut display, &state).unwrap();
        }
    }
}
