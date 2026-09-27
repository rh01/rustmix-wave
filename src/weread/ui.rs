//! WeRead shelf, login, book, and reading state.
//!
//! One network job is handed to the main loop at a time. The device polls that
//! job instead of joining it, so buttons and sleep stay live. Login polls and
//! whole-book downloads also return between requests.

use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
};

use crate::{
    app::router::ScreenRoute,
    buttons::ButtonEvent,
    reader::{ReaderLayout, ReaderPageLine},
    weread::{
        bitmap::{self, MonoBitmap},
        client::{ChapterImage, Job, JobError, JobOutput, Report, Work},
        limits::{
            DOWNLOAD_ATTEMPTS, DOWNLOAD_RETRY_MS, LOGIN_POLL_MS, LOGIN_TIMEOUT_MS,
            MIN_REQUEST_GAP_MS, PROGRESS_DELAY_MS,
        },
        nvs,
        offline::{self, CachedChapter},
        parse::{BookDetail, ChapterMeta, NoteLine, ReadingProgress, ShelfBook},
        session::{self, Session},
        text,
    },
};

const SD_ROOT: &str = "/sdcard/RUSTMIX";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Shelf,
    Login,
    Book,
    Toc,
    Read,
    Notes,
    Download,
}

#[derive(Clone, Eq, PartialEq)]
pub struct WereadUi {
    pub session: Session,
    pub phase: Phase,
    pub status: String,
    pub shelf_cursor: usize,
    pub book_cursor: usize,
    pub toc_cursor: usize,
    pub note_cursor: usize,
    pub books: Vec<ShelfBook>,
    pub covers: Vec<Option<MonoBitmap>>,
    pub book_id: String,
    pub detail: Option<BookDetail>,
    pub chapters: Vec<ChapterMeta>,
    pub progress: Option<ReadingProgress>,
    pub psvts: String,
    pub chapter_pos: usize,
    pub pages: Vec<Vec<ReaderPageLine>>,
    pub page_index: usize,
    pub images: Vec<ChapterImage>,
    pub notes: Vec<NoteLine>,
    pub download_done: usize,
    pub qr_url: String,
    /// Plain chapter text kept so a font change can repaginate without a refetch.
    pub chapter_source: String,
    /// Layout last used to build `pages`. A later font or size change rebuilds.
    paginated_layout: Option<ReaderLayout>,
    pending: Option<Job>,
    next_due_ms: u64,
    generation: u64,
    login_started_ms: u64,
    qr_uid: String,
    qr_cookie: String,
    progress_arm: bool,
    progress_not_before: u64,
    download_cancel: bool,
    download_attempts: u8,
    download_skip: Vec<u32>,
    download_last_error: String,
    read_after_contents: bool,
    toc_after_contents: bool,
    session_dirty: bool,
    pub busy: bool,
    cancel_requested: bool,
}

impl Default for WereadUi {
    fn default() -> Self {
        Self {
            session: Session::default(),
            phase: Phase::Shelf,
            status: "Sign in with WeChat, or save a wrk- key in the Wi-Fi portal.".into(),
            shelf_cursor: 0,
            book_cursor: 0,
            toc_cursor: 0,
            note_cursor: 0,
            books: Vec::new(),
            covers: Vec::new(),
            book_id: String::new(),
            detail: None,
            chapters: Vec::new(),
            progress: None,
            psvts: String::new(),
            chapter_pos: 0,
            pages: Vec::new(),
            page_index: 0,
            images: Vec::new(),
            notes: Vec::new(),
            download_done: 0,
            qr_url: String::new(),
            chapter_source: String::new(),
            paginated_layout: None,
            pending: None,
            next_due_ms: 0,
            generation: 0,
            login_started_ms: 0,
            qr_uid: String::new(),
            qr_cookie: String::new(),
            progress_arm: false,
            progress_not_before: 0,
            download_cancel: false,
            download_attempts: 0,
            download_skip: Vec::new(),
            download_last_error: String::new(),
            read_after_contents: false,
            toc_after_contents: false,
            session_dirty: false,
            busy: false,
            cancel_requested: false,
        }
    }
}

impl std::fmt::Debug for WereadUi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WereadUi")
            .field("phase", &self.phase)
            .field("status", &self.status)
            .field("books", &self.books.len())
            .field("chapters", &self.chapters.len())
            .field("page", &self.page_index)
            .field("web", &self.session.web_signed_in())
            .field("api_key", &self.session.has_api_key())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceOutcome {
    pub refresh: bool,
    pub route: Option<ScreenRoute>,
    /// Restart the panel idle timer. Set while a job is running and when it returns.
    pub touch_activity: bool,
}

impl WereadUi {
    pub fn enter(&mut self, mounted: bool) {
        let root = sd_root();
        if mounted {
            let mut loaded = session::load_session(&root);
            if !loaded.web_signed_in() {
                nvs::overlay(&mut loaded);
            }
            self.session = loaded;
        } else if !self.session.web_signed_in() {
            nvs::overlay(&mut self.session);
        }
        self.phase = Phase::Shelf;
        self.pending = None;
        self.cancel_requested = self.busy;
        self.shelf_cursor = 0;
        self.progress_arm = false;
        if self.session.web_signed_in() || self.session.has_api_key() {
            self.queue(Job::Shelf, 0);
            self.status = "Loading shelf...".into();
        } else {
            self.status = "Sign in with WeChat, or save a wrk- key in the Wi-Fi portal.".into();
        }
    }

    #[must_use]
    pub fn holds_panel(&self) -> bool {
        self.busy || matches!(self.phase, Phase::Login | Phase::Download) || self.pending.is_some()
    }

    /// True while a shelf, login, chapter, download, or progress upload still needs Wi-Fi.
    ///
    /// A download stays true for the whole book, including the pause between
    /// chapters and the main-task SD write. Power management uses this for the
    /// station, the active SD clock, and blocking auto deep sleep.
    /// `HttpJobs::busy` is only the in-flight HTTPS call.
    #[must_use]
    pub fn needs_radio(&self) -> bool {
        self.holds_panel() || self.progress_arm
    }

    /// Save the open chapter before deep sleep. Offline text is already on the card.
    pub fn persist_before_sleep(&mut self, mounted: bool) {
        if !mounted || self.book_id.is_empty() {
            return;
        }
        self.remember_local_progress(mounted);
        let _ =
            offline::save_last_open(&sd_root(), &self.book_id, self.chapter_pos, self.page_index);
    }

    /// Reopen the chapter that was on screen before deep sleep.
    pub fn restore_after_deep_sleep(&mut self, mounted: bool, layout: ReaderLayout) -> bool {
        if !mounted {
            return false;
        }
        let Some(last) = offline::load_last_open(&sd_root()) else {
            return false;
        };
        let root = sd_root();
        let mut loaded = session::load_session(&root);
        if !loaded.web_signed_in() {
            nvs::overlay(&mut loaded);
        }
        self.session = loaded;
        self.book_id = last.book_id;
        self.chapters = offline::load_catalog(&root, &self.book_id).unwrap_or_default();
        self.psvts = offline::load_psvts(&root, &self.book_id);
        if let Some((progress, page)) = offline::load_local_progress(&root, &self.book_id) {
            self.progress = Some(progress);
            self.page_index = page;
        } else {
            self.page_index = last.page_index;
        }
        self.chapter_pos = last.chapter_pos;
        self.phase = Phase::Read;
        self.pending = None;
        self.busy = false;
        self.begin_read(layout, mounted)
    }

    pub fn note_route(&mut self, previous: ScreenRoute, current: ScreenRoute) {
        if previous == ScreenRoute::WeReadLogin && current != ScreenRoute::WeReadLogin {
            self.clear_login_job();
        }
        if previous == ScreenRoute::WeReadDownload && current != ScreenRoute::WeReadDownload {
            self.download_cancel = true;
            if matches!(self.pending, Some(Job::Chapter { .. })) {
                self.pending = None;
            }
        }
        // Reading Preferences is the shared TXT/EPUB screen. Keep the open
        // chapter so BOOT can return to it and repaginate.
        if !current.is_weread() && current != ScreenRoute::ReaderPreferences {
            self.pending = None;
            self.progress_arm = false;
            self.download_cancel = true;
            self.read_after_contents = false;
            self.toc_after_contents = false;
            if self.busy {
                self.cancel_requested = true;
            }
            self.release_settled_download();
            return;
        }
        if current.is_weread() {
            self.phase = phase_from_route(current);
        }
        self.release_settled_download();
    }

    /// Drop the download radio hold once the book is no longer being saved.
    ///
    /// `Phase::Download` keeps Wi-Fi up and blocks auto deep sleep. Leaving the
    /// screen, cancelling, or finishing must clear it. An in-flight HTTPS call
    /// still holds the radio through `busy` until the client closes.
    fn release_settled_download(&mut self) {
        if self.phase == Phase::Download
            && self.download_cancel
            && !self.busy
            && self.pending.is_none()
        {
            self.phase = Phase::Book;
        }
    }

    #[must_use]
    pub fn on_button(
        &mut self,
        route: ScreenRoute,
        event: ButtonEvent,
        layout: ReaderLayout,
        mounted: bool,
    ) -> Option<ScreenRoute> {
        self.phase = phase_from_route(route);
        if self.busy {
            if event == ButtonEvent::Select {
                if self.phase == Phase::Book && self.book_cursor == 0 {
                    self.read_after_contents = true;
                    self.toc_after_contents = false;
                    self.status = "Loading contents...".into();
                    return None;
                }
                self.cancel_requested = true;
                self.download_cancel = true;
                self.status = if self.phase == Phase::Download {
                    "Download stopped. Saved chapters stay on the SD card.".into()
                } else {
                    "Cancelling...".into()
                };
            }
            return None;
        }
        let next = match self.phase {
            Phase::Shelf => self.on_shelf(event),
            Phase::Login => self.on_login(event),
            Phase::Book => self.on_book(event, layout, mounted),
            Phase::Toc => self.on_toc(event, layout, mounted),
            Phase::Read => self.on_read(event, layout, mounted),
            Phase::Notes => self.on_notes(event),
            Phase::Download => self.on_download(event),
        };
        self.release_settled_download();
        next
    }

    #[must_use]
    pub fn shelf_rows(&self) -> usize {
        2 + self.books.len()
    }

    fn on_shelf(&mut self, event: ButtonEvent) -> Option<ScreenRoute> {
        let count = self.shelf_rows().max(1);
        match event {
            ButtonEvent::Up => {
                self.shelf_cursor = self.shelf_cursor.checked_sub(1).unwrap_or(count - 1);
                None
            }
            ButtonEvent::Down => {
                self.shelf_cursor = (self.shelf_cursor + 1) % count;
                None
            }
            ButtonEvent::Select => {
                if self.shelf_cursor == 0 {
                    if self.session.web_signed_in() || self.session.has_api_key() {
                        self.queue(Job::Shelf, 0);
                        self.status = "Refreshing shelf...".into();
                        None
                    } else {
                        self.begin_login(0);
                        Some(ScreenRoute::WeReadLogin)
                    }
                } else if self.shelf_cursor == 1 {
                    self.session.covers = !self.session.covers;
                    self.session_dirty = true;
                    self.status = if self.session.covers {
                        "Covers on. Thumbnails load one at a time.".into()
                    } else {
                        "Covers off.".into()
                    };
                    if !self.session.covers {
                        self.covers.clear();
                    }
                    None
                } else {
                    let index = self.shelf_cursor - 2;
                    let Some(book) = self.books.get(index).cloned() else {
                        return None;
                    };
                    self.open_book_row(&book);
                    Some(ScreenRoute::WeReadBook)
                }
            }
        }
    }

    fn on_login(&mut self, event: ButtonEvent) -> Option<ScreenRoute> {
        if event == ButtonEvent::Select {
            self.begin_login(0);
        }
        None
    }

    fn on_book(
        &mut self,
        event: ButtonEvent,
        layout: ReaderLayout,
        mounted: bool,
    ) -> Option<ScreenRoute> {
        const ROWS: usize = 5;
        match event {
            ButtonEvent::Up => {
                self.book_cursor = self.book_cursor.checked_sub(1).unwrap_or(ROWS - 1);
                None
            }
            ButtonEvent::Down => {
                self.book_cursor = (self.book_cursor + 1) % ROWS;
                None
            }
            ButtonEvent::Select => match self.book_cursor {
                0 => {
                    if self.chapters.is_empty() {
                        self.queue_contents(true);
                        None
                    } else {
                        self.position_for_read();
                        self.begin_read(layout, mounted)
                            .then_some(ScreenRoute::WeReadRead)
                    }
                }
                1 => {
                    if self.chapters.is_empty() {
                        self.queue_contents(false);
                        None
                    } else {
                        self.toc_cursor =
                            self.chapter_pos.min(self.chapters.len().saturating_sub(1));
                        Some(ScreenRoute::WeReadToc)
                    }
                }
                2 => {
                    self.queue(
                        Job::Notes {
                            book_id: self.book_id.clone(),
                        },
                        0,
                    );
                    self.status = "Loading highlights and notes...".into();
                    Some(ScreenRoute::WeReadNotes)
                }
                3 => {
                    if !mounted {
                        self.status = "Insert the SD card before downloading.".into();
                        None
                    } else if self.chapters.is_empty() {
                        self.status = "This book has no chapters to save.".into();
                        None
                    } else {
                        self.download_cancel = false;
                        self.download_attempts = 0;
                        self.download_skip = offline::load_download_skip(&sd_root(), &self.book_id);
                        self.download_last_error.clear();
                        self.download_done = self.count_cached(mounted);
                        self.phase = Phase::Download;
                        self.queue_next_download(mounted, 0);
                        Some(ScreenRoute::WeReadDownload)
                    }
                }
                _ => {
                    self.queue(
                        Job::OpenBook {
                            book_id: self.book_id.clone(),
                        },
                        0,
                    );
                    self.status = "Refreshing book...".into();
                    None
                }
            },
        }
    }

    fn on_toc(
        &mut self,
        event: ButtonEvent,
        layout: ReaderLayout,
        mounted: bool,
    ) -> Option<ScreenRoute> {
        let count = self.chapters.len().max(1);
        match event {
            ButtonEvent::Up => {
                self.toc_cursor = self.toc_cursor.checked_sub(1).unwrap_or(count - 1);
                None
            }
            ButtonEvent::Down => {
                self.toc_cursor = (self.toc_cursor + 1) % count;
                None
            }
            ButtonEvent::Select => {
                if self.chapters.is_empty() {
                    self.queue_contents(false);
                    return None;
                }
                self.chapter_pos = self.toc_cursor.min(self.chapters.len() - 1);
                self.page_index = 0;
                self.begin_read(layout, mounted);
                Some(ScreenRoute::WeReadRead)
            }
        }
    }

    fn on_read(
        &mut self,
        event: ButtonEvent,
        layout: ReaderLayout,
        mounted: bool,
    ) -> Option<ScreenRoute> {
        match event {
            ButtonEvent::Up => {
                if self.page_index > 0 {
                    self.page_index -= 1;
                    self.arm_progress(0);
                } else if self.chapter_pos > 0 {
                    self.chapter_pos -= 1;
                    self.page_index = usize::MAX;
                    self.begin_read(layout, mounted);
                }
                None
            }
            ButtonEvent::Down => {
                if !self.pages.is_empty() && self.page_index + 1 < self.pages.len() {
                    self.page_index += 1;
                    self.arm_progress(0);
                } else if self.chapter_pos + 1 < self.chapters.len() {
                    self.chapter_pos += 1;
                    self.page_index = 0;
                    self.begin_read(layout, mounted);
                }
                None
            }
            ButtonEvent::Select => Some(ScreenRoute::ReaderPreferences),
        }
    }

    fn on_notes(&mut self, event: ButtonEvent) -> Option<ScreenRoute> {
        let count = self.notes.len().max(1);
        match event {
            ButtonEvent::Up => {
                self.note_cursor = self.note_cursor.checked_sub(1).unwrap_or(count - 1);
            }
            ButtonEvent::Down => self.note_cursor = (self.note_cursor + 1) % count,
            ButtonEvent::Select => {}
        }
        None
    }

    fn on_download(&mut self, event: ButtonEvent) -> Option<ScreenRoute> {
        if event == ButtonEvent::Select {
            self.download_cancel = true;
            if matches!(self.pending, Some(Job::Chapter { .. })) {
                self.pending = None;
            }
            self.status = "Download stopped. Saved chapters stay on the SD card.".into();
            return Some(ScreenRoute::WeReadBook);
        }
        None
    }

    fn begin_login(&mut self, now_ms: u64) {
        self.qr_url.clear();
        self.qr_uid.clear();
        self.qr_cookie.clear();
        self.login_started_ms = now_ms;
        self.phase = Phase::Login;
        self.queue(Job::LoginUid, now_ms);
        self.status = "Requesting a WeChat login code...".into();
    }

    fn open_book_row(&mut self, book: &ShelfBook) {
        let root = sd_root();
        self.book_id = book.book_id.clone();
        self.book_cursor = 0;
        self.detail = Some(BookDetail {
            book_id: book.book_id.clone(),
            title: book.title.clone(),
            author: book.author.clone(),
            intro: String::new(),
            cover: book.cover.clone(),
            format: String::new(),
        });
        self.chapters = offline::load_catalog(&root, &book.book_id).unwrap_or_default();
        self.psvts = offline::load_psvts(&root, &book.book_id);
        if let Some((progress, page)) = offline::load_local_progress(&root, &book.book_id) {
            self.progress = Some(progress);
            self.page_index = page;
        } else {
            self.progress = book.progress.map(|progress| ReadingProgress {
                chapter_uid: String::new(),
                chapter_offset: 0,
                progress,
            });
        }
        self.align_chapter_pos();
        self.queue(
            Job::OpenBook {
                book_id: book.book_id.clone(),
            },
            0,
        );
        self.status = "Opening book...".into();
        self.phase = Phase::Book;
    }

    fn queue_contents(&mut self, then_read: bool) {
        self.read_after_contents = then_read;
        self.toc_after_contents = !then_read;
        self.queue(
            Job::OpenBook {
                book_id: self.book_id.clone(),
            },
            0,
        );
        self.status = "Loading contents...".into();
    }

    fn position_for_read(&mut self) {
        let saved = self.progress.as_ref().is_some_and(|progress| {
            !progress.chapter_uid.is_empty()
                && self
                    .chapters
                    .iter()
                    .any(|chapter| chapter.uid == progress.chapter_uid)
        });
        if saved {
            self.align_chapter_pos();
            return;
        }
        self.chapter_pos = 0;
        self.page_index = 0;
    }

    fn begin_read(&mut self, layout: ReaderLayout, mounted: bool) -> bool {
        if self.chapters.is_empty() {
            self.status = "This book has no chapters yet.".into();
            self.pages.clear();
            self.chapter_source.clear();
            self.paginated_layout = None;
            return false;
        }
        self.chapter_pos = self.chapter_pos.min(self.chapters.len() - 1);
        let chapter = self.chapters[self.chapter_pos].clone();
        if mounted {
            if let Some(cached) = offline::load_chapter(&sd_root(), &self.book_id, chapter.index) {
                self.show_text(&cached.text, layout);
                if self.page_index == usize::MAX {
                    self.page_index = self.pages.len().saturating_sub(1);
                }
                self.page_index = self.page_index.min(self.pages.len().saturating_sub(1));
                self.arm_progress(0);
                self.remember_local_progress(mounted);
                self.status = chapter.title.clone();
                return true;
            }
        }
        if !self.session.web_signed_in() {
            self.status =
                "Chapter text needs a QR sign-in. An API key covers shelf, progress, and notes."
                    .into();
            self.pages.clear();
            return false;
        }
        self.pages.clear();
        self.images.clear();
        self.queue(
            Job::Chapter {
                book_id: self.book_id.clone(),
                chapter_uid: chapter.uid,
                chapter_idx: chapter.index,
                psvts: self.psvts.clone(),
                fetch_images: true,
            },
            0,
        );
        self.status = format!("Loading {}", chapter.title);
        true
    }

    fn show_text(&mut self, text: &str, layout: ReaderLayout) {
        let blocks = text::blocks_from_markup(text);
        self.chapter_source = text.to_string();
        self.pages = text::paginate_blocks(&blocks, layout);
        self.paginated_layout = Some(layout);
        self.images.clear();
        if self.page_index == usize::MAX {
            self.page_index = self.pages.len().saturating_sub(1);
        }
        self.page_index = self.page_index.min(self.pages.len().saturating_sub(1));
    }

    /// Rebuild the open chapter after the shared reader font or size changes.
    pub fn repaginate(&mut self, layout: ReaderLayout) {
        if self.chapter_source.is_empty() {
            self.paginated_layout = None;
            return;
        }
        let page = self.page_index;
        let source = std::mem::take(&mut self.chapter_source);
        self.show_text(&source, layout);
        self.chapter_source = source;
        if self.pages.is_empty() {
            self.page_index = 0;
        } else {
            self.page_index = page.min(self.pages.len() - 1);
        }
    }

    /// Rebuild when Reading Preferences changed the layout after the chapter loaded.
    #[must_use]
    pub fn sync_layout(&mut self, layout: ReaderLayout) -> bool {
        if self.chapter_source.is_empty() {
            self.paginated_layout = None;
            return false;
        }
        if self.paginated_layout == Some(layout) {
            return false;
        }
        self.repaginate(layout);
        true
    }

    fn arm_progress(&mut self, now_ms: u64) {
        if !self.session.web_signed_in() || self.chapters.is_empty() {
            return;
        }
        self.progress_arm = true;
        self.progress_not_before = now_ms;
    }

    fn remember_local_progress(&mut self, mounted: bool) {
        if !mounted {
            return;
        }
        let Some(chapter) = self.chapters.get(self.chapter_pos) else {
            return;
        };
        let progress = ReadingProgress {
            chapter_uid: chapter.uid.clone(),
            chapter_offset: self.chapter_offset(),
            progress: self.percent(),
        };
        self.progress = Some(progress.clone());
        let _ = offline::save_progress(&sd_root(), &self.book_id, &progress, self.page_index);
    }

    fn chapter_offset(&self) -> u32 {
        self.pages
            .iter()
            .take(self.page_index)
            .flat_map(|page| page.iter())
            .map(|line| line.text.chars().count())
            .sum::<usize>()
            .min(u32::MAX as usize) as u32
    }

    fn percent(&self) -> u8 {
        let chapters = self.chapters.len().max(1);
        let pages = self.pages.len().max(1);
        let chapter = self.chapter_pos.min(chapters - 1);
        let page = self.page_index.min(pages - 1);
        let done = chapter.saturating_mul(pages).saturating_add(page);
        let total = chapters.saturating_mul(pages).max(1);
        ((done.saturating_mul(100)) / total).min(100) as u8
    }

    fn align_chapter_pos(&mut self) {
        if let Some(progress) = &self.progress {
            if let Some(pos) = self
                .chapters
                .iter()
                .position(|chapter| chapter.uid == progress.chapter_uid)
            {
                self.chapter_pos = pos;
            }
        }
    }

    fn queue(&mut self, job: Job, due_ms: u64) {
        self.pending = Some(job);
        self.next_due_ms = due_ms;
    }

    fn clear_login_job(&mut self) {
        if matches!(self.pending, Some(Job::LoginUid | Job::PollLogin { .. })) {
            self.pending = None;
        }
    }

    fn count_cached(&self, mounted: bool) -> usize {
        if !mounted {
            return 0;
        }
        let root = sd_root();
        self.chapters
            .iter()
            .filter(|chapter| offline::chapter_cached(&root, &self.book_id, chapter.index))
            .count()
    }

    fn queue_next_download(&mut self, mounted: bool, due_ms: u64) {
        if self.download_cancel || !mounted {
            return;
        }
        let root = sd_root();
        if let Some((pos, chapter)) = self.chapters.iter().enumerate().find(|(_, chapter)| {
            !self.download_skip.contains(&chapter.index)
                && !offline::chapter_cached(&root, &self.book_id, chapter.index)
        }) {
            self.chapter_pos = pos;
            self.download_attempts = 0;
            self.queue(
                Job::Chapter {
                    book_id: self.book_id.clone(),
                    chapter_uid: chapter.uid.clone(),
                    chapter_idx: chapter.index,
                    psvts: self.psvts.clone(),
                    fetch_images: false,
                },
                due_ms,
            );
            self.status = format!(
                "Saving {} / {}",
                self.download_done + 1,
                self.chapters.len()
            );
            return;
        }
        self.pending = None;
        self.download_done = self.count_cached(mounted);
        self.download_cancel = true;
        self.status = if self.download_skip.is_empty() {
            format!("Saved {} chapters on the SD card.", self.download_done)
        } else {
            format!(
                "Saved {} of {} chapters. {} failed: {}",
                self.download_done,
                self.chapters.len(),
                self.download_skip.len(),
                self.download_last_error
            )
        };
        self.release_settled_download();
    }

    fn requeue_current_chapter(&mut self, due_ms: u64) {
        let Some(chapter) = self.chapters.get(self.chapter_pos).cloned() else {
            return;
        };
        self.queue(
            Job::Chapter {
                book_id: self.book_id.clone(),
                chapter_uid: chapter.uid,
                chapter_idx: chapter.index,
                psvts: self.psvts.clone(),
                fetch_images: false,
            },
            due_ms,
        );
    }

    fn retry_or_skip_download(&mut self, now_ms: u64, mounted: bool, message: &str) {
        self.download_last_error = message.to_string();
        self.download_attempts = self.download_attempts.saturating_add(1);
        if self.download_attempts < DOWNLOAD_ATTEMPTS {
            self.status = format!(
                "Retrying chapter ({}/{}): {message}",
                self.download_attempts,
                DOWNLOAD_ATTEMPTS - 1
            );
            self.requeue_current_chapter(now_ms.saturating_add(DOWNLOAD_RETRY_MS));
            return;
        }
        if let Some(chapter) = self.chapters.get(self.chapter_pos) {
            let index = chapter.index;
            if !self.download_skip.contains(&index) {
                self.download_skip.push(index);
                if mounted {
                    let _ =
                        offline::save_download_skip(&sd_root(), &self.book_id, &self.download_skip);
                }
            }
        }
        self.download_attempts = 0;
        self.queue_next_download(mounted, now_ms.saturating_add(MIN_REQUEST_GAP_MS));
    }

    pub fn take_work(&mut self, now_ms: u64, mounted: bool) -> Option<Work> {
        self.arm_background(now_ms, mounted);
        if self.pending.is_none() || now_ms < self.next_due_ms {
            return None;
        }
        let job = self.pending.take()?;
        self.generation = self.generation.saturating_add(1);
        Some(Work {
            generation: self.generation,
            job,
            session: self.session.clone(),
        })
    }

    fn arm_background(&mut self, now_ms: u64, mounted: bool) {
        if self.pending.is_some() {
            return;
        }
        if self.progress_arm && self.session.web_signed_in() {
            if self.progress_not_before == 0 {
                self.progress_not_before = now_ms.saturating_add(PROGRESS_DELAY_MS);
                return;
            }
            if now_ms >= self.progress_not_before {
                self.progress_arm = false;
                self.progress_not_before = 0;
                if let Some(chapter) = self.chapters.get(self.chapter_pos).cloned() {
                    let summary = self
                        .pages
                        .get(self.page_index)
                        .and_then(|page| page.first())
                        .map(|line| line.text.chars().take(20).collect())
                        .unwrap_or_default();
                    self.queue(
                        Job::UploadProgress {
                            book_id: self.book_id.clone(),
                            chapter_uid: chapter.uid,
                            chapter_idx: chapter.index,
                            chapter_offset: self.chapter_offset(),
                            summary,
                            progress: self.percent(),
                            psvts: self.psvts.clone(),
                        },
                        now_ms,
                    );
                }
                return;
            }
        }
        if self.phase == Phase::Download && !self.download_cancel {
            self.queue_next_download(mounted, now_ms.saturating_add(MIN_REQUEST_GAP_MS));
            return;
        }
        if self.phase == Phase::Shelf && self.session.covers {
            self.queue_cover();
        }
    }

    fn queue_cover(&mut self) {
        let Some((book, _)) = self
            .books
            .iter()
            .zip(self.covers.iter())
            .find(|(book, cover)| {
                cover.is_none() && !book.cover.is_empty() && bitmap::allowed_asset_url(&book.cover)
            })
        else {
            return;
        };
        self.queue(
            Job::Cover {
                book_id: book.book_id.clone(),
                url: book.cover.clone(),
            },
            0,
        );
    }

    pub fn apply_report(
        &mut self,
        report: Report,
        layout: ReaderLayout,
        mounted: bool,
        now_ms: u64,
    ) -> ServiceOutcome {
        if report.generation != self.generation {
            return ServiceOutcome {
                refresh: false,
                route: None,
                touch_activity: false,
            };
        }
        self.session = report.session;
        match report.result {
            Ok(output) => self.apply_output(output, layout, mounted, now_ms),
            Err(error) => self.apply_error(error, now_ms, mounted),
        }
    }

    fn apply_output(
        &mut self,
        output: JobOutput,
        layout: ReaderLayout,
        mounted: bool,
        now_ms: u64,
    ) -> ServiceOutcome {
        match output {
            JobOutput::Qr { uid, url, cookie } => {
                self.qr_uid = uid.clone();
                self.qr_url = url;
                self.qr_cookie = cookie.clone();
                self.login_started_ms = now_ms;
                self.phase = Phase::Login;
                self.queue(
                    Job::PollLogin { uid, cookie },
                    now_ms.saturating_add(LOGIN_POLL_MS),
                );
                self.status = "Scan the QR code with WeChat.".into();
                ServiceOutcome {
                    refresh: true,
                    route: Some(ScreenRoute::WeReadLogin),
                    touch_activity: false,
                }
            }
            JobOutput::LoginPending => {
                if now_ms.saturating_sub(self.login_started_ms) >= LOGIN_TIMEOUT_MS {
                    self.status = "The QR code expired. Press SELECT to refresh it.".into();
                    self.pending = None;
                    ServiceOutcome {
                        refresh: true,
                        route: None,
                        touch_activity: false,
                    }
                } else if self.phase == Phase::Login {
                    self.queue(
                        Job::PollLogin {
                            uid: self.qr_uid.clone(),
                            cookie: self.qr_cookie.clone(),
                        },
                        now_ms.saturating_add(LOGIN_POLL_MS),
                    );
                    ServiceOutcome {
                        refresh: false,
                        route: None,
                        touch_activity: false,
                    }
                } else {
                    ServiceOutcome {
                        refresh: false,
                        route: None,
                        touch_activity: false,
                    }
                }
            }
            JobOutput::SignedIn { name } => {
                self.session_dirty = true;
                self.qr_cookie.clear();
                self.phase = Phase::Shelf;
                self.queue(Job::Shelf, now_ms);
                self.status = if name.is_empty() {
                    "Signed in.".into()
                } else {
                    format!("Signed in as {name}.")
                };
                ServiceOutcome {
                    refresh: true,
                    route: Some(ScreenRoute::WeRead),
                    touch_activity: false,
                }
            }
            JobOutput::Shelf { books } => {
                self.books = books;
                self.covers = vec![None; self.books.len()];
                self.shelf_cursor = self.shelf_cursor.min(self.shelf_rows().saturating_sub(1));
                self.status = format!("{} books on the shelf.", self.books.len());
                ServiceOutcome {
                    refresh: true,
                    route: None,
                    touch_activity: false,
                }
            }
            JobOutput::Book {
                detail,
                chapters,
                progress,
                psvts,
            } => {
                self.detail = Some(detail.clone());
                self.chapters = chapters.clone();
                if let Some(progress) = progress {
                    let chapter_changed = self
                        .progress
                        .as_ref()
                        .is_some_and(|current| current.chapter_uid != progress.chapter_uid);
                    self.progress = Some(progress);
                    if chapter_changed {
                        self.page_index = 0;
                    }
                }
                if !psvts.is_empty() {
                    self.psvts = psvts.clone();
                }
                self.align_chapter_pos();
                if mounted {
                    let _ = offline::save_catalog(
                        &sd_root(),
                        &detail.book_id,
                        &detail.title,
                        &detail.author,
                        &detail.format,
                        &self.psvts,
                        &chapters,
                    );
                }
                let open_read = self.read_after_contents;
                let open_toc = self.toc_after_contents;
                self.read_after_contents = false;
                self.toc_after_contents = false;
                if open_read || open_toc {
                    if self.chapters.is_empty() {
                        self.status = "WeRead returned no chapters for this book.".into();
                        return ServiceOutcome {
                            refresh: true,
                            route: None,
                            touch_activity: false,
                        };
                    }
                    if open_read {
                        self.position_for_read();
                        let route = self
                            .begin_read(layout, mounted)
                            .then_some(ScreenRoute::WeReadRead);
                        return ServiceOutcome {
                            refresh: true,
                            route,
                            touch_activity: false,
                        };
                    }
                    self.toc_cursor = self.chapter_pos.min(self.chapters.len().saturating_sub(1));
                    self.status = detail.title;
                    return ServiceOutcome {
                        refresh: true,
                        route: Some(ScreenRoute::WeReadToc),
                        touch_activity: false,
                    };
                }
                self.status = detail.title;
                ServiceOutcome {
                    refresh: true,
                    route: None,
                    touch_activity: false,
                }
            }
            JobOutput::Chapter {
                blocks,
                text,
                images,
                psvts,
                format: _,
            } => {
                if !psvts.is_empty() {
                    self.psvts = psvts;
                }
                let downloading = self.phase == Phase::Download && !self.download_cancel;
                if downloading {
                    // Drop the decoded chapter before the next request is queued.
                    self.pages = Vec::new();
                    self.images = Vec::new();
                    self.chapter_source = String::new();
                    self.paginated_layout = None;
                    drop(blocks);
                    drop(images);
                } else {
                    self.show_text(&text, layout);
                    self.images = images;
                }
                if mounted {
                    if let Some(chapter) = self.chapters.get(self.chapter_pos) {
                        if let Err(error) = offline::save_chapter(
                            &sd_root(),
                            &self.book_id,
                            &CachedChapter {
                                uid: chapter.uid.clone(),
                                index: chapter.index,
                                title: chapter.title.clone(),
                                text,
                            },
                        ) {
                            if downloading {
                                self.retry_or_skip_download(now_ms, mounted, &error);
                                return ServiceOutcome {
                                    refresh: true,
                                    route: None,
                                    touch_activity: false,
                                };
                            }
                            self.status = error;
                            return ServiceOutcome {
                                refresh: true,
                                route: None,
                                touch_activity: false,
                            };
                        }
                    }
                    if !downloading {
                        self.remember_local_progress(mounted);
                    }
                }
                if downloading {
                    self.download_attempts = 0;
                    self.download_done = self.count_cached(mounted);
                    self.queue_next_download(mounted, now_ms.saturating_add(MIN_REQUEST_GAP_MS));
                    if self.pending.is_some() {
                        self.status =
                            format!("Saved {} / {}", self.download_done, self.chapters.len());
                    }
                } else {
                    self.arm_progress(now_ms);
                    self.status = self
                        .chapters
                        .get(self.chapter_pos)
                        .map(|chapter| chapter.title.clone())
                        .unwrap_or_else(|| "Chapter".into());
                }
                ServiceOutcome {
                    refresh: true,
                    route: None,
                    touch_activity: false,
                }
            }
            JobOutput::ChapterStored { psvts } => {
                if !psvts.is_empty() {
                    self.psvts = psvts;
                }
                // The raw parts are already on the SD card. Paginate only when opened.
                self.pages = Vec::new();
                self.images = Vec::new();
                self.chapter_source = String::new();
                self.paginated_layout = None;
                if self.phase == Phase::Download && !self.download_cancel {
                    let index = self
                        .chapters
                        .get(self.chapter_pos)
                        .map(|chapter| chapter.index);
                    let on_card = index.is_some_and(|index| {
                        mounted && offline::chapter_cached(&sd_root(), &self.book_id, index)
                    });
                    if mounted && !on_card {
                        self.retry_or_skip_download(now_ms, mounted, "SD card write failed");
                        return ServiceOutcome {
                            refresh: true,
                            route: None,
                            touch_activity: false,
                        };
                    }
                    self.download_attempts = 0;
                    self.download_done = self.count_cached(mounted);
                    self.queue_next_download(mounted, now_ms.saturating_add(MIN_REQUEST_GAP_MS));
                    if self.pending.is_some() {
                        self.status =
                            format!("Saved {} / {}", self.download_done, self.chapters.len());
                    }
                }
                ServiceOutcome {
                    refresh: true,
                    route: None,
                    touch_activity: false,
                }
            }
            JobOutput::ProgressUploaded => {
                self.session_dirty = true;
                self.status = "Progress uploaded.".into();
                ServiceOutcome {
                    refresh: true,
                    route: None,
                    touch_activity: false,
                }
            }
            JobOutput::Notes { lines } => {
                self.notes = lines;
                self.note_cursor = 0;
                self.status = if self.notes.is_empty() {
                    "No highlights or notes for this book.".into()
                } else {
                    format!("{} highlights and notes.", self.notes.len())
                };
                ServiceOutcome {
                    refresh: true,
                    route: None,
                    touch_activity: false,
                }
            }
            JobOutput::Cover { book_id, bitmap } => {
                if let Some(index) = self.books.iter().position(|book| book.book_id == book_id) {
                    if self.covers.len() != self.books.len() {
                        self.covers = vec![None; self.books.len()];
                    }
                    self.covers[index] = Some(bitmap);
                }
                ServiceOutcome {
                    refresh: true,
                    route: None,
                    touch_activity: false,
                }
            }
        }
    }

    fn apply_error(&mut self, error: JobError, now_ms: u64, mounted: bool) -> ServiceOutcome {
        match error {
            JobError::Expired => {
                self.session.expire_web();
                self.session_dirty = true;
                self.progress_arm = false;
                self.download_cancel = true;
                self.read_after_contents = false;
                self.toc_after_contents = false;
                self.begin_login(now_ms);
                self.status = error.to_string();
                ServiceOutcome {
                    refresh: true,
                    route: Some(ScreenRoute::WeReadLogin),
                    touch_activity: false,
                }
            }
            JobError::Cancelled => {
                self.download_cancel = true;
                self.read_after_contents = false;
                self.toc_after_contents = false;
                if matches!(self.pending, Some(Job::Chapter { .. })) {
                    self.pending = None;
                }
                self.release_settled_download();
                self.status = error.to_string();
                ServiceOutcome {
                    refresh: true,
                    route: None,
                    touch_activity: false,
                }
            }
            JobError::OtpRequired | JobError::Clock | JobError::Message(_) => {
                let message = error.to_string();
                self.read_after_contents = false;
                self.toc_after_contents = false;
                if self.phase == Phase::Download && !self.download_cancel {
                    self.retry_or_skip_download(now_ms, mounted, &message);
                } else {
                    self.status = message;
                }
                ServiceOutcome {
                    refresh: true,
                    route: None,
                    touch_activity: false,
                }
            }
        }
    }
}

/// Start or poll one WeRead job without waiting for it to finish.
///
/// `start` may return a handle immediately. Later calls pass the same `inflight`
/// slot and `poll` until it yields a report. SELECT sets `cancel_requested`;
/// the returned report is ignored after the generation moves on.
pub fn drive_with<H>(
    ui: &mut WereadUi,
    inflight: &mut Option<H>,
    unix: Option<u64>,
    now_ms: u64,
    layout: ReaderLayout,
    mounted: bool,
    mut start: impl FnMut(Work, Option<u64>, Arc<AtomicBool>) -> Result<H, Report>,
    mut poll: impl FnMut(&mut H) -> Option<Report>,
    mut cancel: impl FnMut(&H),
) -> ServiceOutcome {
    if let Some(job) = inflight.as_mut() {
        let mut refresh = false;
        if ui.cancel_requested {
            cancel(job);
            ui.generation = ui.generation.saturating_add(1);
            ui.cancel_requested = false;
            ui.download_cancel = true;
            if ui.status != "Cancelling..." && !ui.status.starts_with("Download stopped") {
                ui.status = "Cancelling...".into();
                refresh = true;
            }
        }
        if let Some(report) = poll(job) {
            inflight.take();
            ui.busy = false;
            if report.generation != ui.generation {
                ui.release_settled_download();
                if !ui.status.starts_with("Download stopped") {
                    ui.status = "Cancelled.".into();
                }
                return ServiceOutcome {
                    refresh: true,
                    route: None,
                    touch_activity: true,
                };
            }
            let mut outcome = ui.apply_report(report, layout, mounted, now_ms);
            if ui.session_dirty {
                persist(ui, mounted);
            }
            outcome.touch_activity = true;
            return outcome;
        }
        let became_busy = !ui.busy;
        ui.busy = true;
        return ServiceOutcome {
            refresh: refresh || became_busy,
            route: None,
            touch_activity: true,
        };
    }

    ui.busy = false;
    if ui.cancel_requested {
        ui.cancel_requested = false;
    }
    let Some(work) = ui.take_work(now_ms, mounted) else {
        if ui.session_dirty {
            persist(ui, mounted);
        }
        return ServiceOutcome {
            refresh: false,
            route: None,
            touch_activity: false,
        };
    };
    let cancel_flag = Arc::new(AtomicBool::new(false));
    match start(work, unix, Arc::clone(&cancel_flag)) {
        Ok(handle) => {
            *inflight = Some(handle);
            ui.busy = true;
            ServiceOutcome {
                refresh: true,
                route: None,
                touch_activity: true,
            }
        }
        Err(report) => {
            let mut outcome = ui.apply_report(report, layout, mounted, now_ms);
            if ui.session_dirty {
                persist(ui, mounted);
            }
            outcome.touch_activity = true;
            outcome
        }
    }
}

fn persist(ui: &mut WereadUi, mounted: bool) {
    if mounted {
        let _ = session::store(&sd_root(), &ui.session);
    }
    nvs::save(&ui.session);
    ui.session_dirty = false;
}

fn phase_from_route(route: ScreenRoute) -> Phase {
    match route {
        ScreenRoute::WeReadLogin => Phase::Login,
        ScreenRoute::WeReadBook => Phase::Book,
        ScreenRoute::WeReadToc => Phase::Toc,
        ScreenRoute::WeReadRead => Phase::Read,
        ScreenRoute::WeReadNotes => Phase::Notes,
        ScreenRoute::WeReadDownload => Phase::Download,
        _ => Phase::Shelf,
    }
}

fn sd_root() -> PathBuf {
    #[cfg(test)]
    if let Ok(path) = std::env::var("WEREAD_SD_ROOT") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    PathBuf::from(SD_ROOT)
}

#[cfg(test)]
mod tests {
    use super::WereadUi;
    use crate::{
        app::router::ScreenRoute,
        buttons::ButtonEvent,
        reader::{BookFontSize, ReaderPreferences},
        weread::{
            client::{Job, JobError, JobOutput, Report},
            limits::DOWNLOAD_ATTEMPTS,
            offline::{self, CachedChapter, ChapterDownload, DownloadEvent},
            parse::{BookDetail, ChapterMeta, ReadingProgress},
            session::Session,
        },
    };
    use std::fs;

    fn sd_root_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn cached_chapter_paginates_and_expiry_returns_to_the_qr() {
        let _sd = sd_root_lock();
        let dir = std::env::temp_dir().join(format!("weread-ui-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("WEREAD_SD_ROOT", &dir);
        let book_id = "43208843";
        let chapters = vec![ChapterMeta {
            uid: "2".into(),
            index: 2,
            title: "第二章".into(),
            word_count: 4,
            level: 1,
        }];
        offline::save_catalog(&dir, book_id, "持续交付", "乔梁", "epub", "ps", &chapters).unwrap();
        offline::save_chapter(
            &dir,
            book_id,
            &CachedChapter {
                uid: "2".into(),
                index: 2,
                title: "第二章".into(),
                text: "你好，微信读书。".into(),
            },
        )
        .unwrap();

        let mut ui = WereadUi::default();
        ui.session.vid = "1".into();
        ui.session.skey = "s".into();
        ui.book_id = book_id.into();
        ui.chapters = chapters;
        ui.chapter_pos = 0;
        let layout = ReaderPreferences::default().layout();
        assert!(ui.begin_read(layout, true));
        assert!(ui.pending.is_none());
        assert!(!ui.pages.is_empty());
        assert!(ui.pages[0].iter().any(|line| line.text.contains("微信")));

        ui.generation = 7;
        let mut expired = Session::default();
        expired.vid = "1".into();
        let outcome = ui.apply_report(
            Report {
                generation: 7,
                job: Job::Shelf,
                result: Err(JobError::Expired),
                session: expired,
            },
            layout,
            true,
            1_000,
        );
        assert_eq!(outcome.route, Some(ScreenRoute::WeReadLogin));
        assert!(!ui.session.web_signed_in());
        assert!(matches!(ui.pending, Some(Job::LoginUid)));
        std::env::remove_var("WEREAD_SD_ROOT");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_turn_arms_progress_without_starting_it_immediately() {
        let mut ui = WereadUi::default();
        ui.session.vid = "1".into();
        ui.session.skey = "s".into();
        ui.phase = super::Phase::Read;
        ui.book_id = "b".into();
        ui.chapters = vec![ChapterMeta {
            uid: "2".into(),
            index: 2,
            title: "Chapter".into(),
            word_count: 1,
            level: 1,
        }];
        ui.pages = vec![
            vec![crate::reader::ReaderPageLine {
                text: "one".into(),
                paragraph_end: true,
            }],
            vec![crate::reader::ReaderPageLine {
                text: "two".into(),
                paragraph_end: true,
            }],
        ];
        let layout = ReaderPreferences::default().layout();
        assert_eq!(
            ui.on_button(ScreenRoute::WeReadRead, ButtonEvent::Down, layout, false),
            None
        );
        assert_eq!(ui.page_index, 1);
        assert!(ui.take_work(0, false).is_none());
        let work = ui.take_work(super::PROGRESS_DELAY_MS, false).unwrap();
        assert!(matches!(work.job, Job::UploadProgress { .. }));
    }

    #[test]
    fn shelf_sign_in_row_opens_login_when_signed_out() {
        let mut ui = WereadUi::default();
        let layout = ReaderPreferences::default().layout();
        let route = ui.on_button(ScreenRoute::WeRead, ButtonEvent::Select, layout, false);
        assert_eq!(route, Some(ScreenRoute::WeReadLogin));
        assert!(matches!(ui.pending, Some(Job::LoginUid)));
    }

    #[test]
    fn signed_in_output_clears_the_qr_cookie() {
        let mut ui = WereadUi::default();
        ui.generation = 3;
        ui.qr_cookie = "wr_vid=1".into();
        let mut session = Session::default();
        session.vid = "1".into();
        session.skey = "secret".into();
        let outcome = ui.apply_report(
            Report {
                generation: 3,
                job: Job::PollLogin {
                    uid: "u".into(),
                    cookie: "wr_vid=1".into(),
                },
                result: Ok(JobOutput::SignedIn { name: "Ada".into() }),
                session,
            },
            ReaderPreferences::default().layout(),
            false,
            10,
        );
        assert_eq!(outcome.route, Some(ScreenRoute::WeRead));
        assert!(ui.qr_cookie.is_empty());
        assert!(ui.status.contains("Ada"));
    }

    #[test]
    fn inflight_job_stays_busy_and_select_drops_the_late_report() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        let mut ui = WereadUi::default();
        ui.queue(Job::Shelf, 0);
        let mut inflight = None;
        let open = Arc::new(AtomicBool::new(false));
        let layout = ReaderPreferences::default().layout();
        let step = |ui: &mut WereadUi, inflight: &mut Option<Gate>, open: &Arc<AtomicBool>| {
            let open = Arc::clone(open);
            super::drive_with(
                ui,
                inflight,
                None,
                0,
                layout,
                false,
                move |work, _, cancel| {
                    Ok(Gate {
                        generation: work.generation,
                        job: work.job,
                        session: work.session,
                        open: Arc::clone(&open),
                        cancel,
                    })
                },
                |gate| {
                    if !gate.open.load(Ordering::Relaxed) {
                        return None;
                    }
                    Some(Report {
                        generation: gate.generation,
                        job: gate.job.clone(),
                        session: gate.session.clone(),
                        result: Ok(JobOutput::Shelf { books: Vec::new() }),
                    })
                },
                |gate| gate.cancel.store(true, Ordering::Relaxed),
            )
        };

        let outcome = step(&mut ui, &mut inflight, &open);
        assert!(outcome.touch_activity);
        assert!(ui.busy);
        assert!(ui.holds_panel());
        assert!(inflight.is_some());
        let outcome = step(&mut ui, &mut inflight, &open);
        assert!(outcome.touch_activity);
        assert!(!outcome.refresh);
        assert!(ui
            .on_button(ScreenRoute::WeRead, ButtonEvent::Select, layout, false)
            .is_none());
        assert_eq!(ui.status, "Cancelling...");
        let outcome = step(&mut ui, &mut inflight, &open);
        assert!(outcome.touch_activity);
        assert!(ui.busy);
        assert!(inflight.as_ref().unwrap().cancel.load(Ordering::Relaxed));
        open.store(true, Ordering::Relaxed);
        let outcome = step(&mut ui, &mut inflight, &open);
        assert!(outcome.touch_activity);
        assert!(!ui.busy);
        assert!(inflight.is_none());
        assert_eq!(ui.status, "Cancelled.");
        assert!(ui.books.is_empty());
        let _ = outcome;
    }

    struct Gate {
        generation: u64,
        job: Job,
        session: crate::weread::session::Session,
        open: std::sync::Arc<std::sync::atomic::AtomicBool>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    fn signed_in() -> WereadUi {
        let mut ui = WereadUi::default();
        ui.session.vid = "1".into();
        ui.session.skey = "s".into();
        ui.book_id = "b".into();
        ui
    }

    fn chapter(uid: &str, index: u32, title: &str) -> ChapterMeta {
        ChapterMeta {
            uid: uid.into(),
            index,
            title: title.into(),
            word_count: 1,
            level: 1,
        }
    }

    fn book_output(chapters: Vec<ChapterMeta>, progress: Option<ReadingProgress>) -> JobOutput {
        JobOutput::Book {
            detail: BookDetail {
                book_id: "b".into(),
                title: "想通了".into(),
                author: "徐英瑾".into(),
                intro: String::new(),
                cover: String::new(),
                format: "epub".into(),
            },
            chapters,
            progress,
            psvts: "ps".into(),
        }
    }

    #[test]
    fn read_fetches_contents_then_opens_saved_progress() {
        let mut ui = signed_in();
        let layout = ReaderPreferences::default().layout();
        assert_eq!(
            ui.on_button(ScreenRoute::WeReadBook, ButtonEvent::Select, layout, false),
            None
        );
        assert_eq!(ui.status, "Loading contents...");
        assert!(ui.read_after_contents);
        let work = ui.take_work(0, false).unwrap();
        let outcome = ui.apply_report(
            Report {
                generation: work.generation,
                job: work.job.clone(),
                session: ui.session.clone(),
                result: Ok(book_output(
                    vec![chapter("1", 1, "One"), chapter("9", 9, "Nine")],
                    Some(ReadingProgress {
                        chapter_uid: "9".into(),
                        chapter_offset: 12,
                        progress: 40,
                    }),
                )),
            },
            layout,
            false,
            20,
        );
        assert_eq!(outcome.route, Some(ScreenRoute::WeReadRead));
        assert_eq!(ui.chapter_pos, 1);
        match ui.pending {
            Some(Job::Chapter { chapter_uid, .. }) => assert_eq!(chapter_uid, "9"),
            other => panic!("expected chapter job, got {other:?}"),
        }
    }

    #[test]
    fn read_without_saved_chapter_opens_the_first_chapter() {
        let mut ui = signed_in();
        let layout = ReaderPreferences::default().layout();
        assert_eq!(
            ui.on_button(ScreenRoute::WeReadBook, ButtonEvent::Select, layout, false),
            None
        );
        let work = ui.take_work(0, false).unwrap();
        let outcome = ui.apply_report(
            Report {
                generation: work.generation,
                job: work.job,
                session: ui.session.clone(),
                result: Ok(book_output(
                    vec![chapter("1", 1, "One"), chapter("2", 2, "Two")],
                    None,
                )),
            },
            layout,
            false,
            20,
        );
        assert_eq!(outcome.route, Some(ScreenRoute::WeReadRead));
        assert_eq!(ui.chapter_pos, 0);
        match ui.pending {
            Some(Job::Chapter { chapter_uid, .. }) => assert_eq!(chapter_uid, "1"),
            other => panic!("expected chapter job, got {other:?}"),
        }
    }

    #[test]
    fn contents_fetch_failure_shows_the_worker_error() {
        let mut ui = signed_in();
        let layout = ReaderPreferences::default().layout();
        assert_eq!(
            ui.on_button(ScreenRoute::WeReadBook, ButtonEvent::Select, layout, false),
            None
        );
        let work = ui.take_work(0, false).unwrap();
        let outcome = ui.apply_report(
            Report {
                generation: work.generation,
                job: work.job,
                session: ui.session.clone(),
                result: Err(JobError::Message(
                    "WeRead worker failed to start: Not enough memory".into(),
                )),
            },
            layout,
            false,
            20,
        );
        assert_eq!(outcome.route, None);
        assert!(!ui.read_after_contents);
        assert!(ui.status.contains("Not enough memory"));
        assert!(!ui.status.contains("no chapters"));
    }

    #[test]
    fn read_during_contents_fetch_does_not_cancel_it() {
        let mut ui = signed_in();
        ui.busy = true;
        ui.phase = super::Phase::Book;
        let layout = ReaderPreferences::default().layout();
        assert!(ui
            .on_button(ScreenRoute::WeReadBook, ButtonEvent::Select, layout, false)
            .is_none());
        assert!(ui.read_after_contents);
        assert!(!ui.cancel_requested);
        assert_eq!(ui.status, "Loading contents...");
    }

    #[test]
    fn download_retries_a_chapter_then_continues() {
        let mut ui = signed_in();
        ui.phase = super::Phase::Download;
        ui.chapters = vec![chapter("1", 1, "One"), chapter("2", 2, "Two")];
        ui.chapter_pos = 0;
        ui.generation = 4;
        let layout = ReaderPreferences::default().layout();
        for attempt in 1..DOWNLOAD_ATTEMPTS {
            let outcome = ui.apply_report(
                Report {
                    generation: 4,
                    job: Job::Chapter {
                        book_id: "b".into(),
                        chapter_uid: "1".into(),
                        chapter_idx: 1,
                        psvts: String::new(),
                        fetch_images: false,
                    },
                    session: ui.session.clone(),
                    result: Err(JobError::Message("Not enough memory".into())),
                },
                layout,
                true,
                1_000,
            );
            assert_eq!(outcome.route, None);
            assert!(!ui.download_cancel);
            assert!(ui.status.contains("Retrying"));
            assert!(ui.status.contains("Not enough memory"));
            assert_eq!(ui.download_attempts, attempt);
            match &ui.pending {
                Some(Job::Chapter { chapter_idx, .. }) => assert_eq!(*chapter_idx, 1),
                other => panic!("expected a retry of chapter 1, got {other:?}"),
            }
        }
        ui.apply_report(
            Report {
                generation: 4,
                job: Job::Chapter {
                    book_id: "b".into(),
                    chapter_uid: "1".into(),
                    chapter_idx: 1,
                    psvts: String::new(),
                    fetch_images: false,
                },
                session: ui.session.clone(),
                result: Err(JobError::Message("Not enough memory".into())),
            },
            layout,
            true,
            2_000,
        );
        assert!(!ui.download_cancel);
        assert!(ui.download_skip.contains(&1));
        match ui.pending {
            Some(Job::Chapter { chapter_idx, .. }) => assert_eq!(chapter_idx, 2),
            other => panic!("expected the next chapter, got {other:?}"),
        }
    }

    #[test]
    fn download_drops_chapter_text_before_the_next_job() {
        let mut ui = signed_in();
        ui.phase = super::Phase::Download;
        ui.chapters = vec![chapter("1", 1, "One")];
        ui.generation = 2;
        ui.pages = vec![vec![crate::reader::ReaderPageLine {
            text: "old".into(),
            paragraph_end: true,
        }]];
        ui.chapter_source = "old".into();
        ui.apply_report(
            Report {
                generation: 2,
                job: Job::Chapter {
                    book_id: "b".into(),
                    chapter_uid: "1".into(),
                    chapter_idx: 1,
                    psvts: String::new(),
                    fetch_images: false,
                },
                session: ui.session.clone(),
                result: Ok(JobOutput::Chapter {
                    blocks: vec![crate::weread::text::Block::Text(
                        crate::weread::text::TextBlock {
                            text: "hello chapter".into(),
                        },
                    )],
                    text: "hello chapter".into(),
                    images: Vec::new(),
                    psvts: String::new(),
                    format: "txt".into(),
                }),
            },
            ReaderPreferences::default().layout(),
            false,
            10,
        );
        assert!(ui.pages.is_empty());
        assert!(ui.chapter_source.is_empty());
        assert!(ui.images.is_empty());
        assert!(ui.pending.is_none());
    }

    #[test]
    fn download_stores_raw_chunks_and_paginates_only_when_opened() {
        let _sd = sd_root_lock();
        let dir = std::env::temp_dir().join(format!("weread-ui-stream-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("WEREAD_SD_ROOT", &dir);
        let book_id = "43208843";
        let shard = crate::weread::decode::seal_plain("<p>Hello 微信</p>");
        let mut download = ChapterDownload::begin(&dir, book_id, "1", 1).unwrap();
        download.apply(DownloadEvent::BeginPart("e0")).unwrap();
        for chunk in shard.as_bytes().chunks(100) {
            download
                .apply(DownloadEvent::Chunk(chunk.to_vec()))
                .unwrap();
        }
        download.apply(DownloadEvent::EndPart).unwrap();
        download.commit().unwrap();

        let mut ui = signed_in();
        ui.book_id = book_id.into();
        ui.phase = super::Phase::Download;
        ui.chapters = vec![chapter("1", 1, "One")];
        ui.generation = 3;
        ui.chapter_source = "old".into();
        ui.pages = vec![vec![crate::reader::ReaderPageLine {
            text: "old".into(),
            paragraph_end: true,
        }]];
        ui.apply_report(
            Report {
                generation: 3,
                job: Job::Chapter {
                    book_id: book_id.into(),
                    chapter_uid: "1".into(),
                    chapter_idx: 1,
                    psvts: "ps".into(),
                    fetch_images: false,
                },
                session: ui.session.clone(),
                result: Ok(JobOutput::ChapterStored { psvts: "ps".into() }),
            },
            ReaderPreferences::default().layout(),
            true,
            10,
        );
        assert!(ui.pages.is_empty());
        assert!(ui.chapter_source.is_empty());
        assert!(ui.pending.is_none());
        assert!(ui.status.contains("Saved"));
        assert_eq!(ui.psvts, "ps");
        assert!(ui.begin_read(ReaderPreferences::default().layout(), true));
        assert!(ui.chapter_source.contains("Hello"));
        assert!(ui.chapter_source.contains("微信"));
        assert!(!ui.pages.is_empty());
        std::env::remove_var("WEREAD_SD_ROOT");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn repaginate_uses_the_reader_font_size() {
        let mut ui = signed_in();
        ui.phase = super::Phase::Read;
        ui.chapters = vec![chapter("1", 1, "One")];
        let text = "abcd ".repeat(80);
        let mut small = ReaderPreferences::default();
        small.font_size = BookFontSize::Px16;
        ui.show_text(&text, small.layout());
        let small_pages = ui.pages.len();
        let mut large = small;
        large.font_size = BookFontSize::Px72;
        ui.repaginate(large.layout());
        assert!(ui.pages.len() > small_pages);
        assert_eq!(ui.chapter_source, text);
    }

    #[test]
    fn select_on_the_chapter_opens_reading_preferences() {
        let mut ui = signed_in();
        let layout = ReaderPreferences::default().layout();
        assert_eq!(
            ui.on_button(ScreenRoute::WeReadRead, ButtonEvent::Select, layout, false),
            Some(ScreenRoute::ReaderPreferences)
        );
    }

    #[test]
    fn sync_layout_rebuilds_only_when_the_font_metrics_change() {
        let mut ui = signed_in();
        let text = "abcd ".repeat(40);
        let small = ReaderPreferences::default().layout();
        ui.show_text(&text, small);
        let pages = ui.pages.len();
        assert!(!ui.sync_layout(small));
        assert_eq!(ui.pages.len(), pages);
        let mut large = ReaderPreferences::default();
        large.font_size = BookFontSize::Px72;
        assert!(ui.sync_layout(large.layout()));
        assert!(ui.pages.len() > pages);
    }

    #[test]
    fn download_stays_on_the_radio_between_chapters() {
        let mut ui = signed_in();
        ui.phase = super::Phase::Download;
        ui.busy = false;
        ui.pending = Some(Job::Chapter {
            book_id: "b".into(),
            chapter_uid: "2".into(),
            chapter_idx: 2,
            psvts: String::new(),
            fetch_images: false,
        });
        assert!(ui.needs_radio());
        ui.phase = super::Phase::Book;
        ui.pending = None;
        ui.busy = false;
        ui.progress_arm = false;
        assert!(!ui.needs_radio());
    }

    fn chapter_report(ui: &WereadUi, generation: u64, error: JobError) -> Report {
        Report {
            generation,
            job: Job::Chapter {
                book_id: ui.book_id.clone(),
                chapter_uid: "1".into(),
                chapter_idx: 1,
                psvts: String::new(),
                fetch_images: false,
            },
            session: ui.session.clone(),
            result: Err(error),
        }
    }

    #[test]
    fn sd_write_failure_retries_then_skips_and_is_remembered() {
        let _sd = sd_root_lock();
        let dir = std::env::temp_dir().join(format!("weread-sd-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-directory");
        fs::write(&blocker, b"x").unwrap();
        std::env::set_var("WEREAD_SD_ROOT", &blocker);
        let mut ui = signed_in();
        ui.book_id = "43208843".into();
        ui.phase = super::Phase::Download;
        ui.chapters = vec![chapter("1", 1, "One"), chapter("2", 2, "Two")];
        ui.generation = 8;
        let layout = ReaderPreferences::default().layout();
        ui.apply_report(
            Report {
                generation: 8,
                job: Job::Chapter {
                    book_id: ui.book_id.clone(),
                    chapter_uid: "1".into(),
                    chapter_idx: 1,
                    psvts: String::new(),
                    fetch_images: false,
                },
                session: ui.session.clone(),
                result: Ok(JobOutput::Chapter {
                    blocks: Vec::new(),
                    text: "hello".into(),
                    images: Vec::new(),
                    psvts: String::new(),
                    format: "txt".into(),
                }),
            },
            layout,
            true,
            1_000,
        );
        assert_eq!(ui.download_attempts, 1);
        assert!(ui.status.contains("SD card"));
        assert!(!ui.status.contains("Saved"));
        match &ui.pending {
            Some(Job::Chapter { chapter_idx, .. }) => assert_eq!(*chapter_idx, 1),
            other => panic!("expected a retry of the same chapter, got {other:?}"),
        }

        std::env::set_var("WEREAD_SD_ROOT", &dir);
        ui.apply_report(
            chapter_report(&ui, 8, JobError::Message("SD card full".into())),
            layout,
            true,
            2_000,
        );
        assert_eq!(ui.download_attempts, 2);
        assert!(ui.status.contains("SD card full"));
        ui.apply_report(
            chapter_report(&ui, 8, JobError::Message("SD card full".into())),
            layout,
            true,
            3_000,
        );
        assert!(ui.download_skip.contains(&1));
        match &ui.pending {
            Some(Job::Chapter { chapter_idx, .. }) => assert_eq!(*chapter_idx, 2),
            other => panic!("expected the next chapter, got {other:?}"),
        }
        let skip =
            fs::read_to_string(offline::book_dir(&dir, "43208843").join("SKIP.TXT")).unwrap();
        assert!(skip.contains("WRSKIP1"));
        assert!(skip.lines().any(|line| line.trim() == "1"));

        let mut again = signed_in();
        again.book_id = "43208843".into();
        again.chapters = ui.chapters.clone();
        again.book_cursor = 3;
        assert_eq!(
            again.on_button(ScreenRoute::WeReadBook, ButtonEvent::Select, layout, true),
            Some(ScreenRoute::WeReadDownload)
        );
        assert!(again.download_skip.contains(&1));
        match &again.pending {
            Some(Job::Chapter { chapter_idx, .. }) => assert_eq!(*chapter_idx, 2),
            other => panic!("reboot must skip the saved failure, got {other:?}"),
        }
        assert!(again.needs_radio());
        std::env::remove_var("WEREAD_SD_ROOT");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stored_chapter_without_a_file_is_not_reported_as_saved() {
        let _sd = sd_root_lock();
        let dir = std::env::temp_dir().join(format!("weread-sd-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("WEREAD_SD_ROOT", &dir);
        let mut ui = signed_in();
        ui.book_id = "43208843".into();
        ui.phase = super::Phase::Download;
        ui.chapters = vec![chapter("1", 1, "One")];
        ui.generation = 4;
        ui.apply_report(
            Report {
                generation: 4,
                job: Job::Chapter {
                    book_id: ui.book_id.clone(),
                    chapter_uid: "1".into(),
                    chapter_idx: 1,
                    psvts: String::new(),
                    fetch_images: false,
                },
                session: ui.session.clone(),
                result: Ok(JobOutput::ChapterStored {
                    psvts: String::new(),
                }),
            },
            ReaderPreferences::default().layout(),
            true,
            10,
        );
        assert_eq!(ui.download_attempts, 1);
        assert!(ui.status.contains("SD card write failed"));
        assert!(!ui.status.contains("Saved"));
        std::env::remove_var("WEREAD_SD_ROOT");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaving_a_download_releases_the_radio() {
        let mut ui = signed_in();
        ui.phase = super::Phase::Download;
        ui.pending = Some(Job::Chapter {
            book_id: "b".into(),
            chapter_uid: "1".into(),
            chapter_idx: 1,
            psvts: String::new(),
            fetch_images: false,
        });
        assert!(ui.needs_radio());
        ui.note_route(ScreenRoute::WeReadDownload, ScreenRoute::Home);
        assert!(ui.pending.is_none());
        assert_ne!(ui.phase, super::Phase::Download);
        assert!(!ui.needs_radio());

        let mut busy = signed_in();
        busy.phase = super::Phase::Download;
        busy.busy = true;
        busy.generation = 2;
        busy.note_route(ScreenRoute::WeReadDownload, ScreenRoute::Home);
        assert!(busy.cancel_requested);
        assert!(busy.needs_radio());
        busy.busy = false;
        busy.apply_report(
            chapter_report(&busy, 2, JobError::Cancelled),
            ReaderPreferences::default().layout(),
            false,
            0,
        );
        assert_ne!(busy.phase, super::Phase::Download);
        assert!(!busy.needs_radio());
    }

    #[test]
    fn cancel_and_finish_release_the_radio_on_the_download_screen() {
        let layout = ReaderPreferences::default().layout();
        let mut ui = signed_in();
        ui.phase = super::Phase::Download;
        ui.chapters = vec![chapter("1", 1, "One")];
        assert_eq!(
            ui.on_button(
                ScreenRoute::WeReadDownload,
                ButtonEvent::Select,
                layout,
                false
            ),
            Some(ScreenRoute::WeReadBook)
        );
        assert!(!ui.needs_radio());

        let _sd = sd_root_lock();
        let dir = std::env::temp_dir().join(format!("weread-ui-done-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("WEREAD_SD_ROOT", &dir);
        let mut done = signed_in();
        done.book_id = "43208843".into();
        done.phase = super::Phase::Download;
        done.chapters = vec![chapter("1", 1, "One")];
        offline::save_chapter(
            &dir,
            &done.book_id,
            &CachedChapter {
                uid: "1".into(),
                index: 1,
                title: "One".into(),
                text: "saved".into(),
            },
        )
        .unwrap();
        done.generation = 1;
        done.apply_report(
            Report {
                generation: 1,
                job: Job::Chapter {
                    book_id: done.book_id.clone(),
                    chapter_uid: "1".into(),
                    chapter_idx: 1,
                    psvts: String::new(),
                    fetch_images: false,
                },
                session: done.session.clone(),
                result: Ok(JobOutput::ChapterStored {
                    psvts: String::new(),
                }),
            },
            layout,
            true,
            0,
        );
        assert!(done.status.contains("Saved"), "{}", done.status);
        assert_ne!(done.phase, super::Phase::Download);
        assert!(!done.needs_radio());
        assert!(done
            .on_button(ScreenRoute::WeReadDownload, ButtonEvent::Up, layout, true)
            .is_none());
        assert!(!done.needs_radio());
        std::env::remove_var("WEREAD_SD_ROOT");
        let _ = fs::remove_dir_all(&dir);
    }
}
