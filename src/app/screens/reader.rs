//! Reader landing, library, bookmarks, loading, TXT / EPUB page, TOC and options screens.

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
        widgets::{
            footer::draw_footer,
            header::draw_header,
            status_row::{draw_status_row, StatusRow},
        },
    },
    orientation::OrientedFrameBuffer,
    reader::{
        BookFormat, ParagraphAlignment, ReaderLibraryTab, ReaderLoadingStage, ReaderOption,
        ReadingPreference, ReadingTheme,
    },
};

pub fn render_continue_reading(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    draw_header(
        display,
        state.display,
        "CONTINUE READING",
        "PERSISTENT BOOK RESUME",
    )?;
    let heading = state.display.heading_style();
    let body = state.display.body_style();
    if let Some(session) = state.reader.session.as_ref() {
        Text::new(
            &heading.truncate(&session.book.title, 38, 420),
            Point::new(24, 190),
            heading,
        )
        .draw(display)?;
        Text::new(
            &format!(
                "Runtime page {} is ready.",
                session.current_absolute_page() + 1
            ),
            Point::new(24, 240),
            body,
        )
        .draw(display)?;
        Text::new("SELECT resumes the open page.", Point::new(24, 284), body).draw(display)?;
    } else if let Some(resume) = state.reader.resume.as_ref() {
        Text::new(
            &heading.truncate(&resume.title, 38, 420),
            Point::new(24, 190),
            heading,
        )
        .draw(display)?;
        Text::new(
            &format!("Saved page {} is ready to restore.", resume.page_index + 1),
            Point::new(24, 240),
            body,
        )
        .draw(display)?;
        Text::new(
            "SELECT loads the saved position.",
            Point::new(24, 284),
            body,
        )
        .draw(display)?;
    } else {
        Text::new("No saved book", Point::new(24, 190), heading).draw(display)?;
        Text::new(
            "Open Library and choose a TXT book.",
            Point::new(24, 240),
            body,
        )
        .draw(display)?;
        Text::new(
            "The last-read page is stored on the SD card.",
            Point::new(24, 284),
            body,
        )
        .draw(display)?;
    }
    draw_footer(display, state.display, "SELECT RESUME  HOLD BOOT BACK")
}

pub fn render_library(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let reader = &state.reader;
    let body = state.display.body_style();
    let heading = state.display.heading_style();
    let detail = state.display.detail_style();
    draw_header(display, state.display, "LIBRARY", "TXT / REFLOWABLE EPUB")?;
    let visible = reader.visible_entries();
    let status = library_status(reader.library_tab, visible.len());
    draw_status_row(
        display,
        state.display,
        StatusRow {
            left: status.left,
            middle: &status.middle,
            right: status.right,
        },
    )?;
    draw_tabs(display, state, reader.library_tab)?;

    let control_selected = reader.library_selected == 0;
    draw_row(
        display,
        state,
        188,
        control_selected,
        "Change tab",
        "SELECT",
        "TABS",
    )?;
    if visible.is_empty() {
        let message = reader
            .library_error
            .as_deref()
            .unwrap_or(match reader.library_tab {
                ReaderLibraryTab::Recent => "No recent books yet.",
                ReaderLibraryTab::Bookmarks => "No saved bookmarks yet.",
                _ => "Copy TXT or EPUB books into /RUSTMIX/BOOKS.",
            });
        Text::new(&truncate(message, 54), Point::new(26, 302), body).draw(display)?;
    }
    for (index, entry) in visible.iter().take(7).enumerate() {
        let selected = reader.library_selected == index + 1;
        let columns = library_entry_columns(reader, entry);
        draw_row(
            display,
            state,
            248 + index as i32 * 58,
            selected,
            &heading.truncate(&entry.book.title, 25, 270),
            columns.badge.as_str(),
            columns.suffix.as_str(),
        )?;
    }
    if reader.library_tab != ReaderLibraryTab::Bookmarks {
        Text::new(
            "TXT and EPUB open with staged first-page loading.",
            Point::new(24, 716),
            detail,
        )
        .draw(display)?;
    }
    draw_footer(
        display,
        state.display,
        "MOVE  SELECT OPEN/TAB  HOLD BOOT BACK",
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LibraryStatus {
    left: &'static str,
    middle: String,
    right: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LibraryEntryColumns {
    badge: String,
    suffix: String,
}

fn library_status(tab: ReaderLibraryTab, entry_count: usize) -> LibraryStatus {
    if tab == ReaderLibraryTab::Bookmarks {
        LibraryStatus {
            left: "Bookmarks",
            middle: format!("{entry_count} saved"),
            right: "MARKS.TXT",
        }
    } else {
        LibraryStatus {
            left: tab.label(),
            middle: format!("{entry_count} books"),
            right: "SD BOOKS",
        }
    }
}

fn library_entry_columns(
    reader: &crate::reader::ReaderUiState,
    entry: &crate::reader::ReaderLibraryEntry,
) -> LibraryEntryColumns {
    if reader.library_tab == ReaderLibraryTab::Bookmarks {
        let page = entry
            .location
            .as_ref()
            .map_or(1, |bookmark| reader.bookmark_display_page(bookmark));
        if let Some(chapter) = entry
            .location
            .as_ref()
            .and_then(|bookmark| reader.bookmark_display_chapter_page(bookmark))
        {
            LibraryEntryColumns {
                badge: format!("CH {}", chapter.chapter_text()),
                suffix: format!("P {}", chapter.page_text()),
            }
        } else {
            LibraryEntryColumns {
                badge: "PAGE".into(),
                suffix: page.to_string(),
            }
        }
    } else {
        LibraryEntryColumns {
            badge: entry.book.format.badge().into(),
            suffix: "OPEN".into(),
        }
    }
}

fn bookmark_entry_columns(
    reader: &crate::reader::ReaderUiState,
    bookmark: &crate::reader::ReaderLocation,
) -> LibraryEntryColumns {
    if let Some(chapter) = reader.bookmark_display_chapter_page(bookmark) {
        LibraryEntryColumns {
            badge: format!("CH {}", chapter.chapter_text()),
            suffix: format!("P {}", chapter.page_text()),
        }
    } else {
        LibraryEntryColumns {
            badge: "PAGE".into(),
            suffix: reader.bookmark_display_page(bookmark).to_string(),
        }
    }
}

pub fn render_bookmarks(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    draw_header(
        display,
        state.display,
        "BOOKMARKS",
        "PERSISTENT READER MARKS",
    )?;
    draw_status_row(
        display,
        state.display,
        StatusRow {
            left: "MARKS.TXT",
            middle: &format!("{} saved", state.reader.bookmarks.len()),
            right: "SD FILE",
        },
    )?;
    let body = state.display.body_style();
    let heading = state.display.heading_style();
    if state.reader.bookmarks.is_empty() {
        Text::new(
            "No saved bookmarks",
            Point::new(24, 210),
            state.display.heading_style(),
        )
        .draw(display)?;
        Text::new(
            "Open a Reader page, choose Reader Options,",
            Point::new(24, 264),
            body,
        )
        .draw(display)?;
        Text::new(
            "then select Add / Remove Bookmark.",
            Point::new(24, 306),
            body,
        )
        .draw(display)?;
    } else {
        for (index, bookmark) in state.reader.bookmarks.iter().take(8).enumerate() {
            let top = 164 + index as i32 * 64;
            let columns = bookmark_entry_columns(&state.reader, bookmark);
            draw_row(
                display,
                state,
                top,
                state.reader.bookmarks_selected == index,
                &heading.truncate(&bookmark.title, 23, 250),
                columns.badge.as_str(),
                columns.suffix.as_str(),
            )?;
        }
    }
    draw_footer(display, state.display, "MOVE  SELECT OPEN  HOLD BOOT BACK")
}

pub fn render_loading(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    draw_header(
        display,
        state.display,
        "OPENING BOOK",
        "RESPONSIVE FIRST-PAGE-FIRST CACHE",
    )?;
    let body = state.display.body_style();
    let heading = state.display.heading_style();
    let loading = state.reader.loading.as_ref();
    let title = loading.map_or("Book", |value| value.book.title.as_str());
    let stage = loading.map_or(ReaderLoadingStage::OpeningFile, |value| value.stage);
    Text::new(
        &heading.truncate(title, 36, 420),
        Point::new(24, 176),
        heading,
    )
    .draw(display)?;
    Text::new(stage.label(), Point::new(24, 238), body).draw(display)?;
    draw_progress(display, stage.progress())?;
    let message = loading.map_or("Preparing reader...", |value| value.message.as_str());
    Text::new(&truncate(message, 52), Point::new(24, 356), body).draw(display)?;
    Text::new(
        "The current chapter opens before the rest of the book.",
        Point::new(24, 410),
        body,
    )
    .draw(display)?;
    draw_footer(display, state.display, "HOLD BOOT CANCEL")
}

pub fn render_page(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let Some(session) = state.reader.session.as_ref() else {
        return render_continue_reading(display, state);
    };
    let size = display.orientation().logical_size();
    let dark = state.reader.preferences.dark_mode;
    let ink = if dark {
        BinaryColor::Off
    } else {
        BinaryColor::On
    };
    if dark {
        Rectangle::new(Point::new(0, 0), Size::new(size.width, size.height))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(display)?;
    }
    let width = size.width as i32;
    let height = size.height as i32;
    let landscape = width > height;
    let immersive = session.layout.immersive;
    let (show_header, show_status, show_footer) =
        reading_chrome(immersive, state.reading_status_overlay);
    let header_height = if landscape { 52 } else { 70 };
    let status_top = header_height + 10;
    let status_height = if landscape { 34 } else { 42 };
    let footer_line = height - 54;
    let mut body = if immersive {
        ReaderBodyGeometry::edge_to_edge(width, height, session.layout)
    } else {
        ReaderBodyGeometry::new(width, status_top, status_height, footer_line)
    };
    let body_style = reader_body_style(
        state.reader.preferences.book_font,
        state.reader.preferences.font_size,
        state.reader.preferences.theme,
    )
    .with_tracking(session.layout.letter_spacing_px)
    .with_color(ink);
    let ui_body = state.display.body_style();
    let ui_detail = state.display.detail_style();
    let ui_ink = if dark {
        state.display.text_style(UiTextRole::Body, BinaryColor::Off)
    } else {
        ui_body
    };
    let heading = state.display.header_title_style();

    if show_header {
        Rectangle::new(
            Point::new(0, 0),
            Size::new(size.width, header_height as u32),
        )
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(display)?;
        Text::new(
            &heading.truncate(
                &session.book.title,
                if landscape { 52 } else { 27 },
                if landscape { 760 } else { 444 },
            ),
            Point::new(18, if landscape { 28 } else { 32 }),
            state.display.header_title_style(),
        )
        .draw(display)?;
        Text::new(
            if session.book.format == BookFormat::Text {
                "TXT READER"
            } else {
                "EPUB REFLOWABLE"
            },
            Point::new(18, if landscape { 48 } else { 60 }),
            state.display.header_subtitle_style(),
        )
        .draw(display)?;
    }

    if show_status && !immersive {
        Rectangle::new(
            Point::new(14, status_top),
            Size::new((width - 28) as u32, status_height as u32),
        )
        .into_styled(PrimitiveStyle::with_stroke(ink, 1))
        .draw(display)?;
        let status_baseline = status_top + status_height - 10;
        let status_label = reading_status_label(state, session);
        Text::new(
            &ui_ink.truncate(&status_label, if landscape { 70 } else { 36 }, width - 48),
            Point::new(24, status_baseline),
            ui_ink,
        )
        .draw(display)?;
    }

    if !immersive {
        body.text.left += i32::from(session.layout.margin_left_px);
        body.text.right -= i32::from(session.layout.margin_right_px);
        body.text.top += i32::from(session.layout.margin_top_px);
        body.text.bottom -= i32::from(session.layout.margin_bottom_px);
    }
    if state.reader.preferences.theme == ReadingTheme::HighContrast {
        Rectangle::new(
            Point::new(body.frame.left, body.frame.top),
            Size::new(body.frame.width() as u32, body.frame.height() as u32),
        )
        .into_styled(PrimitiveStyle::with_stroke(ink, 2))
        .draw(display)?;
    }

    if let Some(page) = session.current_cached_page() {
        let line_step =
            i32::from(body_style.line_height()) + 2 + i32::from(session.layout.line_spacing_px);
        let first_baseline = body.text.top + i32::from(body_style.line_height());
        for (index, line) in page
            .lines
            .iter()
            .take(session.layout.lines_per_page)
            .enumerate()
        {
            let baseline = first_baseline + index as i32 * line_step;
            if baseline >= body.text.bottom {
                break;
            }
            let mut bounds = body.text;
            if line.first_line_indent {
                bounds.left += session.layout.indent_px();
            }
            let (rendered, left) = aligned_reader_line(
                line.text.as_str(),
                line.paragraph_end,
                state.reader.preferences.effective_alignment(),
                body_style,
                bounds,
            );
            let shown = state.reader.preferences.display_line(&rendered);
            Text::new(shown.as_str(), Point::new(left, baseline), body_style)
                .draw_clipped(display, body.text)?;
        }
    } else {
        let baseline = body.text.top + i32::from(body_style.line_height());
        Text::new(
            "Preparing page...",
            Point::new(body.text.left, baseline),
            body_style,
        )
        .draw_clipped(display, body.text)?;
    }

    if show_footer {
        Rectangle::new(
            Point::new(14, footer_line),
            Size::new((width - 28) as u32, 1),
        )
        .into_styled(PrimitiveStyle::with_fill(ink))
        .draw(display)?;
        let footer_hint = page_turn_hint(landscape, state.reader.preferences.swap_page_keys);
        let footer_style = if dark {
            if landscape {
                state
                    .display
                    .text_style(UiTextRole::Detail, BinaryColor::Off)
            } else {
                ui_ink
            }
        } else if landscape {
            ui_detail
        } else {
            ui_body
        };
        Text::new(footer_hint, Point::new(18, height - 18), footer_style).draw(display)?;
    }
    if show_status && immersive {
        draw_immersive_status(display, state, &immersive_status_label(state, session))?;
    }
    Ok(())
}

/// Header, persistent status, and footer visibility for a reading page.
/// Immersive pages hide all three until a long-press asks for the status overlay.
pub(crate) fn reading_chrome(immersive: bool, status_overlay: bool) -> (bool, bool, bool) {
    if immersive {
        (false, status_overlay, false)
    } else {
        (true, true, true)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReaderBodyGeometry {
    text: TextBounds,
    frame: ReaderFrameBounds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReaderFrameBounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl ReaderFrameBounds {
    #[must_use]
    const fn width(self) -> i32 {
        self.right - self.left
    }

    #[must_use]
    const fn height(self) -> i32 {
        self.bottom - self.top
    }
}

impl ReaderBodyGeometry {
    /// Shared Reader body rectangle used by Classic and High Contrast. The
    /// stronger High Contrast frame stays outside this viewport, so switching
    /// themes never changes TXT pagination or cache fingerprints.
    #[must_use]
    const fn new(width: i32, status_top: i32, status_height: i32, footer_line: i32) -> Self {
        let text = TextBounds::new(
            24,
            status_top + status_height + 18,
            width - 24,
            footer_line - 12,
        );
        let frame = ReaderFrameBounds {
            left: text.left - 8,
            top: text.top - 8,
            right: text.right + 8,
            bottom: text.bottom + 8,
        };
        Self { text, frame }
    }

    /// Full-panel text box. Side insets come from the immersive layout; top and bottom are 0.
    #[must_use]
    fn edge_to_edge(width: i32, height: i32, layout: crate::reader::ReaderLayout) -> Self {
        let text = TextBounds::new(
            i32::from(layout.margin_left_px),
            i32::from(layout.margin_top_px),
            width - i32::from(layout.margin_right_px),
            height - i32::from(layout.margin_bottom_px),
        );
        let frame = ReaderFrameBounds {
            left: text.left,
            top: text.top,
            right: text.right,
            bottom: text.bottom,
        };
        Self { text, frame }
    }
}

pub fn render_options(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    draw_header(display, state.display, "READER OPTIONS", "READER ACTIONS")?;
    for (index, option) in ReaderOption::ALL.iter().copied().enumerate() {
        let badge = match option {
            ReaderOption::Bookmark if state.reader.current_page_is_bookmarked() => "REMOVE",
            ReaderOption::Bookmark => "ADD",
            ReaderOption::Bookmarks => "LIST",
            ReaderOption::TableOfContents if state.reader.has_structured_toc() => "LIST",
            _ => option.badge(),
        };
        draw_row(
            display,
            state,
            146 + index as i32 * 66,
            state.reader.options_selected == index,
            option.label(),
            badge,
            "",
        )?;
    }
    draw_footer(
        display,
        state.display,
        "MOVE  SELECT ACTIVATE  HOLD BOOT BACK",
    )
}

pub fn render_preferences(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    let subtitle = match state.reader.preference_menu {
        crate::reader::PreferenceMenu::Root => "UP/DOWN MOVE  SELECT OPEN",
        crate::reader::PreferenceMenu::Typography => "TYPOGRAPHY",
        crate::reader::PreferenceMenu::Page => "PAGE",
        crate::reader::PreferenceMenu::Display => "DISPLAY",
        crate::reader::PreferenceMenu::Status => "STATUS BAR",
        crate::reader::PreferenceMenu::Controls => "CONTROLS",
    };
    draw_header(display, state.display, "READING PREFERENCES", subtitle)?;
    let font_label = state.reader.book_font_display_label();
    for (index, preference) in state.reader.preference_rows().iter().copied().enumerate() {
        let owned;
        let badge = match preference {
            ReadingPreference::TypographyMenu
            | ReadingPreference::PageMenu
            | ReadingPreference::DisplayMenu
            | ReadingPreference::StatusMenu
            | ReadingPreference::ControlsMenu => ">>>",
            ReadingPreference::Presets => {
                owned = state
                    .reader
                    .preferences
                    .matching_preset()
                    .map_or("Custom", |preset| preset.label())
                    .to_string();
                owned.as_str()
            }
            ReadingPreference::ChineseScript => state.reader.preferences.chinese_script.label(),
            ReadingPreference::ReadingTheme => state.reader.preferences.theme.label(),
            ReadingPreference::Immersive => on_off(state.reader.preferences.immersive),
            ReadingPreference::DarkMode => on_off(state.reader.preferences.dark_mode),
            ReadingPreference::FullRefresh => state.reader.preferences.full_refresh.label(),
            ReadingPreference::Orientation => state.reader.preferences.orientation.label(),
            ReadingPreference::BookFontSize => state.reader.preferences.font_size.label(),
            ReadingPreference::BookFont => font_label.as_str(),
            ReadingPreference::LetterSpacing => state.reader.preferences.letter_spacing.label(),
            ReadingPreference::LineSpacing => state.reader.preferences.line_spacing.label(),
            ReadingPreference::ParagraphSpacing => {
                state.reader.preferences.paragraph_spacing.label()
            }
            ReadingPreference::FirstLineIndent => {
                on_off(state.reader.preferences.first_line_indent)
            }
            ReadingPreference::Justified => on_off(state.reader.preferences.justified),
            ReadingPreference::MarginTop => state.reader.preferences.margin_top.label(),
            ReadingPreference::MarginBottom => state.reader.preferences.margin_bottom.label(),
            ReadingPreference::MarginLeft => state.reader.preferences.margin_left.label(),
            ReadingPreference::MarginRight => state.reader.preferences.margin_right.label(),
            ReadingPreference::ParagraphAlignment => {
                state.reader.preferences.paragraph_alignment.label()
            }
            ReadingPreference::StatusPage => on_off(state.reader.preferences.status_page),
            ReadingPreference::StatusChapter => on_off(state.reader.preferences.status_chapter),
            ReadingPreference::StatusTime => on_off(state.reader.preferences.status_time),
            ReadingPreference::StatusBattery => on_off(state.reader.preferences.status_battery),
            ReadingPreference::SwapPageKeys => on_off(state.reader.preferences.swap_page_keys),
            ReadingPreference::LongPressChapter => {
                on_off(state.reader.preferences.long_press_chapter)
            }
            ReadingPreference::AutoPageTurn => state.reader.preferences.auto_page_turn.label(),
        };
        draw_row(
            display,
            state,
            148 + index as i32 * 68,
            state.reader.preferences_selected == index,
            preference.label(),
            badge,
            "",
        )?;
    }
    draw_footer(
        display,
        state.display,
        "UP/DOWN MOVE  SELECT CHANGE  HOLD BOOT BACK",
    )
}

fn on_off(value: bool) -> &'static str {
    if value {
        "On"
    } else {
        "Off"
    }
}

fn page_turn_hint(landscape: bool, swap: bool) -> &'static str {
    match (landscape, swap) {
        (true, false) => "UP PREV  DOWN NEXT  SELECT OPTIONS",
        (true, true) => "UP NEXT  DOWN PREV  SELECT OPTIONS",
        (false, false) => "UP previous   DOWN next   SELECT options",
        (false, true) => "UP next   DOWN previous   SELECT options",
    }
}

fn reading_status_label(state: &AppState, session: &crate::reader::ReaderSession) -> String {
    let prefs = state.reader.preferences;
    let mut parts = Vec::new();
    if prefs.status_page {
        parts.push(format!("PAGE {}", session.page_label()));
    }
    if prefs.status_chapter {
        if let Some(chapter) = session.current_epub_chapter_page_label() {
            parts.push(format!(
                "CH {} {}",
                chapter.chapter_number,
                chapter.page_text()
            ));
        } else {
            parts.push(format!("{}%", session.progress_percent()));
        }
    }
    if prefs.status_time {
        parts.push(state.board.time_label(state.regional));
    }
    if prefs.status_battery {
        parts.push(state.board.battery_label());
    }
    if state.reader.current_page_is_bookmarked() {
        parts.push("MARKED".into());
    }
    if parts.is_empty() {
        return format!(
            "{}  {}",
            state.reader.book_font_display_label(),
            session.content_badge()
        );
    }
    parts.join("  ")
}

fn immersive_status_label(state: &AppState, session: &crate::reader::ReaderSession) -> String {
    let mut parts = vec![format!("PAGE {}", session.page_label())];
    if let Some(chapter) = session.current_epub_chapter_page_label() {
        parts.push(format!(
            "CH {} {}",
            chapter.chapter_number,
            chapter.page_text()
        ));
    } else {
        parts.push(format!("{}%", session.progress_percent()));
    }
    parts.push(state.board.time_label(state.regional));
    parts.push(state.board.battery_label());
    if state.reader.current_page_is_bookmarked() {
        parts.push("MARKED".into());
    }
    parts.push("SELECT menu".into());
    parts.join("  ")
}

pub(crate) fn draw_immersive_status(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
    label: &str,
) -> Result<(), Infallible> {
    let size = display.orientation().logical_size();
    let width = size.width as i32;
    let height = size.height as i32;
    let dark = state.reader.preferences.dark_mode;
    let bar_fill = if dark {
        BinaryColor::Off
    } else {
        BinaryColor::On
    };
    let bar_ink = if dark {
        BinaryColor::On
    } else {
        BinaryColor::Off
    };
    let top = height - 44;
    Rectangle::new(Point::new(0, top), Size::new(size.width, 44))
        .into_styled(PrimitiveStyle::with_fill(bar_fill))
        .draw(display)?;
    let style = state.display.text_style(UiTextRole::Body, bar_ink);
    let shown = style.truncate(label, if width > height { 72 } else { 34 }, width - 16);
    Text::new(&shown, Point::new(8, height - 14), style).draw(display)?;
    Ok(())
}

pub fn render_toc(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
) -> Result<(), Infallible> {
    draw_header(
        display,
        state.display,
        "TABLE OF CONTENTS",
        if state.reader.has_structured_toc() {
            "EPUB NAVIGATION"
        } else {
            "TXT FOUNDATION"
        },
    )?;
    let heading = state.display.heading_style();
    let body = state.display.body_style();
    let toc = state.reader.toc_entries();
    if toc.is_empty() {
        Text::new("No structured TOC", Point::new(24, 200), heading).draw(display)?;
        Text::new(
            "Ordinary TXT files do not provide a formal",
            Point::new(24, 258),
            body,
        )
        .draw(display)?;
        Text::new(
            "table of contents. EPUB books expose their",
            Point::new(24, 300),
            body,
        )
        .draw(display)?;
        Text::new(
            "navigation entries on this screen.",
            Point::new(24, 342),
            body,
        )
        .draw(display)?;
        return draw_footer(display, state.display, "HOLD BOOT BACK");
    }

    draw_status_row(
        display,
        state.display,
        StatusRow {
            left: "EPUB TOC",
            middle: &format!("{} entries", toc.len()),
            right: "SELECT OPEN",
        },
    )?;
    let first = state.reader.toc_selected.saturating_sub(7);
    for (row, entry) in toc.iter().skip(first).take(8).enumerate() {
        let index = first + row;
        draw_row(
            display,
            state,
            166 + row as i32 * 64,
            state.reader.toc_selected == index,
            &truncate(&entry.label, 27),
            "CH",
            &(entry.spine_index + 1).to_string(),
        )?;
    }
    draw_footer(display, state.display, "MOVE  SELECT OPEN  HOLD BOOT BACK")
}

pub(crate) fn aligned_reader_line(
    line: &str,
    paragraph_end: bool,
    alignment: ParagraphAlignment,
    style: crate::app::typography::UiTextStyle,
    bounds: TextBounds,
) -> (String, i32) {
    let width = style.text_width(line);
    let available = bounds.width().max(0);
    match alignment {
        ParagraphAlignment::Left => (line.into(), bounds.left),
        ParagraphAlignment::Center => (line.into(), bounds.left + (available - width).max(0) / 2),
        ParagraphAlignment::Right => (line.into(), bounds.left + (available - width).max(0)),
        ParagraphAlignment::Justified if !paragraph_end => {
            (justify_reader_line(line, style, available), bounds.left)
        }
        ParagraphAlignment::Justified => (line.into(), bounds.left),
    }
}

fn justify_reader_line(
    line: &str,
    style: crate::app::typography::UiTextStyle,
    available: i32,
) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.len() < 2 {
        return line.into();
    }
    let base = words.join(" ");
    let space = style.text_width(" ").max(1);
    let extra_spaces = ((available - style.text_width(base.as_str())).max(0) / space) as usize;
    let gaps = words.len() - 1;
    let mut output = String::new();
    for (index, word) in words.iter().enumerate() {
        output.push_str(word);
        if index < gaps {
            let remainder = if index < extra_spaces % gaps { 1 } else { 0 };
            let count = 1 + extra_spaces / gaps + remainder;
            output.extend(core::iter::repeat(' ').take(count));
        }
    }
    output
}

fn draw_tabs(
    display: &mut OrientedFrameBuffer<'_>,
    state: &AppState,
    active: ReaderLibraryTab,
) -> Result<(), Infallible> {
    let body = state.display.body_style();
    let tabs = [
        ReaderLibraryTab::Recent,
        ReaderLibraryTab::Books,
        ReaderLibraryTab::Files,
        ReaderLibraryTab::Bookmarks,
    ];
    for (index, tab) in tabs.iter().copied().enumerate() {
        let left = 10 + index as i32 * 117;
        if tab == active {
            Rectangle::new(Point::new(left, 132), Size::new(112, 42))
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(display)?;
            Text::new(
                tab.label(),
                Point::new(left + 8, 161),
                state.display.text_style(UiTextRole::Body, BinaryColor::Off),
            )
            .draw(display)?;
        } else {
            Text::new(tab.label(), Point::new(left + 8, 161), body).draw(display)?;
        }
    }
    Ok(())
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
    Text::new(
        if selected { ">" } else { " " },
        Point::new(32, top + 32),
        body,
    )
    .draw(display)?;
    Text::new(label, Point::new(58, top + 32), body).draw(display)?;
    Text::new(badge, Point::new(338, top + 32), body).draw(display)?;
    Text::new(suffix, Point::new(402, top + 32), body).draw(display)?;
    Ok(())
}

fn draw_progress(display: &mut OrientedFrameBuffer<'_>, percent: u8) -> Result<(), Infallible> {
    Rectangle::new(Point::new(24, 282), Size::new(432, 38))
        .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 2))
        .draw(display)?;
    let width = 4 * percent as u32;
    Rectangle::new(Point::new(30, 288), Size::new(width.min(420), 26))
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(display)?;
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
    use super::{
        aligned_reader_line, bookmark_entry_columns, library_entry_columns, library_status,
        reading_chrome, render_bookmarks, render_continue_reading, render_library, render_loading,
        render_options, render_preferences, render_toc, ReaderBodyGeometry,
    };
    use crate::{
        app::AppState,
        framebuffer::FrameBuffer,
        orientation::OrientedFrameBuffer,
        reader::{
            BookFormat, ParagraphAlignment, PendingReaderOpen, ReaderBook, ReaderChapterPageLabel,
            ReaderLibraryEntry, ReaderLibraryTab, ReaderLoadingStage, ReaderLocation,
        },
    };

    #[test]
    fn high_contrast_frame_stays_outside_shared_text_viewport() {
        let body = ReaderBodyGeometry::new(480, 80, 42, 746);
        assert!(body.frame.left < body.text.left);
        assert!(body.frame.top < body.text.top);
        assert!(body.frame.right > body.text.right);
        assert!(body.frame.bottom > body.text.bottom);
        assert_eq!(body.text.left, 24);
        assert_eq!(body.text.right, 456);
    }

    #[test]
    fn immersive_page_hides_chrome_until_the_status_overlay() {
        assert_eq!(reading_chrome(false, false), (true, true, true));
        assert_eq!(reading_chrome(false, true), (true, true, true));
        assert_eq!(reading_chrome(true, false), (false, false, false));
        assert_eq!(reading_chrome(true, true), (false, true, false));
        let mut prefs = crate::reader::ReaderPreferences::default();
        prefs.immersive = true;
        let body = ReaderBodyGeometry::edge_to_edge(480, 800, prefs.layout());
        assert_eq!(body.text.left, 8);
        assert_eq!(body.text.right, 472);
        assert_eq!(body.text.top, 0);
        assert_eq!(body.text.bottom, 800);
    }

    #[test]
    fn library_bookmarks_tab_uses_saved_status_and_page_columns() {
        let bookmark = ReaderLocation {
            path: "POIROT~1.TXT".into(),
            title: "POIROT~1".into(),
            format: BookFormat::Text,
            size_bytes: 123,
            modified_seconds: 456,
            byte_offset: 789,
            page_index: 11,
            epub_chapter: None,
        };
        let mut reader = crate::reader::ReaderUiState::default();
        reader.library_tab = ReaderLibraryTab::Bookmarks;
        let entry = ReaderLibraryEntry {
            book: bookmark.as_book(),
            location: Some(bookmark),
        };
        assert_eq!(
            library_status(ReaderLibraryTab::Bookmarks, 9),
            super::LibraryStatus {
                left: "Bookmarks",
                middle: "9 saved".into(),
                right: "MARKS.TXT",
            }
        );
        assert_eq!(
            library_entry_columns(&reader, &entry),
            super::LibraryEntryColumns {
                badge: "PAGE".into(),
                suffix: "12".into(),
            }
        );
    }
    #[test]
    fn epub_bookmark_columns_show_chapter_and_chapter_page_total() {
        let bookmark = ReaderLocation {
            path: "NOVEL.EPU".into(),
            title: "Novel".into(),
            format: BookFormat::Epub,
            size_bytes: 123,
            modified_seconds: 456,
            byte_offset: 789,
            page_index: 11,
            epub_chapter: Some(ReaderChapterPageLabel {
                chapter_number: 4,
                chapter_count: 10,
                page_number: 3,
                page_count: 12,
                approximate: false,
            }),
        };
        let reader = crate::reader::ReaderUiState::default();
        assert_eq!(
            bookmark_entry_columns(&reader, &bookmark),
            super::LibraryEntryColumns {
                badge: "CH 4/10".into(),
                suffix: "P 3/12".into(),
            }
        );
    }

    #[test]
    fn library_books_and_files_tabs_keep_format_and_open_columns() {
        let entry = ReaderLibraryEntry {
            book: ReaderBook {
                path: "POIROT~1.TXT".into(),
                title: "POIROT~1".into(),
                format: BookFormat::Text,
                size_bytes: 123,
                modified_seconds: 456,
            },
            location: None,
        };
        for tab in [ReaderLibraryTab::Books, ReaderLibraryTab::Files] {
            let mut reader = crate::reader::ReaderUiState::default();
            reader.library_tab = tab;
            assert_eq!(
                library_entry_columns(&reader, &entry),
                super::LibraryEntryColumns {
                    badge: "TXT".into(),
                    suffix: "OPEN".into(),
                }
            );
        }
    }

    #[test]
    fn reader_screens_render_without_sd_card() {
        let mut state = AppState::default();
        let mut frame = FrameBuffer::new_white();
        let mut display = OrientedFrameBuffer::new(&mut frame, Default::default());
        render_continue_reading(&mut display, &state).unwrap();
        render_library(&mut display, &state).unwrap();
        render_bookmarks(&mut display, &state).unwrap();
        render_options(&mut display, &state).unwrap();
        render_preferences(&mut display, &state).unwrap();
        render_toc(&mut display, &state).unwrap();
        state.reader.loading = Some(PendingReaderOpen {
            book: ReaderBook {
                path: "a.txt".into(),
                title: "A".into(),
                format: BookFormat::Text,
                size_bytes: 1,
                modified_seconds: 0,
            },
            stage: ReaderLoadingStage::OpeningFile,
            encoding: None,
            epub_document: None,
            resume: None,
            epub_open: Default::default(),
            message: "Preparing".into(),
        });
        render_loading(&mut display, &state).unwrap();
    }
    #[test]
    fn paragraph_alignment_moves_or_justifies_reader_lines_inside_bounds() {
        let style = AppState::default().display.body_style();
        let bounds = crate::app::typography::TextBounds::new(20, 0, 220, 100);
        let (_, left) =
            aligned_reader_line("short line", true, ParagraphAlignment::Left, style, bounds);
        let (_, center) = aligned_reader_line(
            "short line",
            true,
            ParagraphAlignment::Center,
            style,
            bounds,
        );
        let (_, right) =
            aligned_reader_line("short line", true, ParagraphAlignment::Right, style, bounds);
        assert!(left < center);
        assert!(center < right);
        let (justified, _) = aligned_reader_line(
            "one two three",
            false,
            ParagraphAlignment::Justified,
            style,
            bounds,
        );
        assert!(justified.len() > "one two three".len());
    }
}
