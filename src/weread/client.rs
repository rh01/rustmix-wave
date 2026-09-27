//! One polite WeRead job: login, shelf, chapter, progress, or notes.
//!
//! Network I/O stays behind [`Transport`]. The device runs it on a short-lived
//! worker; host tests use a scripted transport.

use crate::weread::{
    bitmap::{self, MonoBitmap},
    body::body_transfer_error,
    crypto, decode,
    limits::{
        DOWNLOAD_CLASSIFY_BYTES, MAX_CHAPTER_IMAGES, MAX_HTML_BYTES, MAX_IMAGE_BYTES,
        MAX_JSON_BYTES, MAX_SHARD_BYTES,
    },
    parse::{
        self, BookDetail, ChapterMeta, LoginPoll, NoteLine, ReadingProgress, ResponseClass,
        ShelfBook,
    },
    protocol::{self, WEB_ORIGIN},
    session::Session,
    text::{self, Block},
};

#[derive(Clone, Debug)]
pub struct Request {
    pub method: &'static str,
    pub url: String,
    pub body: Option<String>,
    pub headers: Vec<(String, String)>,
    pub max_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    pub set_cookie: String,
    pub content_length: Option<usize>,
}

/// Prefix and length of a chapter part whose body was streamed to storage.
#[derive(Clone, Debug)]
pub struct StreamedPart {
    pub status: u16,
    pub set_cookie: String,
    /// Leading bytes used to tell a zip, a txt envelope, and a shard apart.
    pub prefix: Vec<u8>,
    pub total: usize,
    /// Negative when the response has no Content-Length.
    pub content_length: i64,
    pub chunked: bool,
    /// True after the terminal chunk, or when a Content-Length body is complete.
    pub terminal_chunk: bool,
}

/// Mark a job cancelled and close the live HTTP client.
///
/// The flag alone is checked between reads. Closing the client is what makes
/// a blocked `esp_http_client_read` return instead of waiting out the timeout.
pub fn signal_cancel(flag: &std::sync::atomic::AtomicBool, close_client: &mut dyn FnMut()) {
    flag.store(true, std::sync::atomic::Ordering::Relaxed);
    close_client();
}

pub trait Transport {
    fn idle(&mut self);
    fn call(&mut self, request: &Request) -> Result<Response, String>;

    fn cancelled(&self) -> bool {
        false
    }

    /// Offline download is streaming each response body to the SD card.
    fn streaming_download(&self) -> bool {
        false
    }

    /// Read one chapter part without retaining its body.
    ///
    /// The device override writes each read onward. Interactive reads keep using [`call`].
    fn call_stream(
        &mut self,
        _request: &Request,
        _part: &'static str,
    ) -> Result<StreamedPart, String> {
        Err("streaming download is not available".into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    LoginUid,
    PollLogin {
        uid: String,
        cookie: String,
    },
    Shelf,
    OpenBook {
        book_id: String,
    },
    Chapter {
        book_id: String,
        chapter_uid: String,
        chapter_idx: u32,
        psvts: String,
        fetch_images: bool,
    },
    UploadProgress {
        book_id: String,
        chapter_uid: String,
        chapter_idx: u32,
        chapter_offset: u32,
        summary: String,
        progress: u8,
        psvts: String,
    },
    Notes {
        book_id: String,
    },
    Cover {
        book_id: String,
        url: String,
    },
}

#[derive(Clone, Debug)]
pub enum JobOutput {
    Qr {
        uid: String,
        url: String,
        cookie: String,
    },
    LoginPending,
    SignedIn {
        name: String,
    },
    Shelf {
        books: Vec<ShelfBook>,
    },
    Book {
        detail: BookDetail,
        chapters: Vec<ChapterMeta>,
        progress: Option<ReadingProgress>,
        psvts: String,
    },
    Chapter {
        blocks: Vec<Block>,
        text: String,
        images: Vec<ChapterImage>,
        psvts: String,
        format: String,
    },
    /// Raw chapter parts are already on the SD card. Decode when the chapter is opened.
    ChapterStored {
        psvts: String,
    },
    ProgressUploaded,
    Notes {
        lines: Vec<NoteLine>,
    },
    Cover {
        book_id: String,
        bitmap: MonoBitmap,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChapterImage {
    pub alt: String,
    pub bitmap: Option<MonoBitmap>,
}

#[derive(Clone, Debug)]
pub enum JobError {
    Expired,
    OtpRequired,
    Clock,
    Cancelled,
    Message(String),
}

impl std::fmt::Display for JobError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => formatter.write_str("WeRead session expired. Scan the QR code again."),
            Self::OtpRequired => formatter.write_str(
                "This account needs a phone code. Finish login in the WeRead app, then scan again.",
            ),
            Self::Clock => {
                formatter.write_str("Sync the clock over Wi-Fi before reading WeRead chapters.")
            }
            Self::Cancelled => formatter.write_str("Cancelled."),
            Self::Message(message) => formatter.write_str(message),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Work {
    pub generation: u64,
    pub job: Job,
    pub session: Session,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub generation: u64,
    pub job: Job,
    pub result: Result<JobOutput, JobError>,
    pub session: Session,
}

pub struct CallCtx {
    pub unix: Option<u64>,
    pub random_state: u64,
    first: bool,
    renewed: bool,
}

impl CallCtx {
    #[must_use]
    pub fn new(unix: Option<u64>, random_state: u64) -> Self {
        Self {
            unix,
            random_state,
            first: true,
            renewed: false,
        }
    }

    fn next_u32(&mut self) -> u32 {
        self.random_state = self
            .random_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1);
        (self.random_state >> 32) as u32
    }
}

pub fn perform(transport: &mut dyn Transport, work: Work, unix: Option<u64>) -> Report {
    perform_with_seed(transport, work, unix, 0x1234_5678_9abc_def0)
}

pub fn perform_with_seed(
    transport: &mut dyn Transport,
    mut work: Work,
    unix: Option<u64>,
    seed: u64,
) -> Report {
    let mut ctx = CallCtx::new(unix, seed);
    let result = dispatch(transport, &mut work.session, &work.job, &mut ctx);
    Report {
        generation: work.generation,
        job: work.job,
        result,
        session: work.session,
    }
}

fn dispatch(
    transport: &mut dyn Transport,
    session: &mut Session,
    job: &Job,
    ctx: &mut CallCtx,
) -> Result<JobOutput, JobError> {
    match job {
        Job::LoginUid => login_uid(transport, session, ctx),
        Job::PollLogin { uid, cookie } => poll_login(transport, session, ctx, uid, cookie),
        Job::Shelf => shelf(transport, session, ctx),
        Job::OpenBook { book_id } => open_book(transport, session, ctx, book_id),
        Job::Chapter {
            book_id,
            chapter_uid,
            chapter_idx,
            psvts,
            fetch_images,
        } => chapter(
            transport,
            session,
            ctx,
            book_id,
            chapter_uid,
            *chapter_idx,
            psvts,
            *fetch_images,
        ),
        Job::UploadProgress {
            book_id,
            chapter_uid,
            chapter_idx,
            chapter_offset,
            summary,
            progress,
            psvts,
        } => upload_progress(
            transport,
            session,
            ctx,
            book_id,
            chapter_uid,
            *chapter_idx,
            *chapter_offset,
            summary,
            *progress,
            psvts,
        ),
        Job::Notes { book_id } => notes(transport, session, ctx, book_id),
        Job::Cover { book_id, url } => cover(transport, ctx, book_id, url),
    }
}

fn login_uid(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
) -> Result<JobOutput, JobError> {
    let page = call(
        transport,
        ctx,
        Request {
            method: "GET",
            url: protocol::skills_page_url(),
            body: None,
            headers: protocol::browser_headers("", &format!("{WEB_ORIGIN}/"), false),
            max_bytes: 32 * 1024,
        },
        false,
    );
    if let Ok(response) = page {
        session.absorb_set_cookie(&response.set_cookie);
        session.handshake = session.cookie_header();
    }
    let response = call(
        transport,
        ctx,
        Request {
            method: "GET",
            url: protocol::login_uid_url(),
            body: None,
            headers: protocol::browser_headers(
                &session.cookie_header(),
                &protocol::skills_page_url(),
                true,
            ),
            max_bytes: MAX_JSON_BYTES,
        },
        true,
    )?;
    session.absorb_set_cookie(&response.set_cookie);
    let text = body_text(&response)?;
    let uid = parse::parse_login_uid(&text).ok_or(JobError::Message(
        "WeRead did not return a login code.".into(),
    ))?;
    let url = protocol::confirm_url(&uid);
    if url.len() > crate::weread::limits::MAX_QR_CHARS {
        return Err(JobError::Message("Login QR payload is too long.".into()));
    }
    Ok(JobOutput::Qr {
        uid,
        url,
        cookie: session.cookie_header(),
    })
}

fn poll_login(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
    uid: &str,
    cookie: &str,
) -> Result<JobOutput, JobError> {
    session.handshake = cookie.to_string();
    let response = call(
        transport,
        ctx,
        Request {
            method: "GET",
            url: protocol::login_info_url(uid),
            body: None,
            headers: protocol::browser_headers(cookie, &protocol::skills_page_url(), true),
            max_bytes: MAX_JSON_BYTES,
        },
        true,
    )?;
    session.absorb_set_cookie(&response.set_cookie);
    let text = body_text(&response)?;
    match parse::parse_login_poll(&text) {
        LoginPoll::Pending => Ok(JobOutput::LoginPending),
        LoginPoll::Otp => Err(JobError::OtpRequired),
        LoginPoll::Expired => Err(JobError::Message(
            "The QR code expired. Press SELECT to refresh it.".into(),
        )),
        LoginPoll::Failed(message) => Err(JobError::Message(message)),
        LoginPoll::Success {
            vid,
            access_token,
            refresh_token,
        } => {
            if session.vid.is_empty() {
                session.vid = vid.clone();
            }
            if session.skey.is_empty() {
                session.skey = access_token.clone();
            }
            if session.rt.is_empty() && !refresh_token.is_empty() {
                session.rt = crypto::url_encode(&refresh_token);
            }
            session.ql = "0".into();
            session.vid = if session.vid.is_empty() {
                vid.clone()
            } else {
                session.vid.clone()
            };
            if session.skey.is_empty() {
                session.skey = access_token.clone();
            }
            // Server Set-Cookie values already overwrote fallbacks via absorb.
            if session.vid.is_empty() {
                session.vid = vid;
            }
            if session.skey.is_empty() {
                session.skey = access_token.clone();
            }
            session.skey_unix = ctx.unix.unwrap_or(0);
            let user_url = format!(
                "{WEB_ORIGIN}/api/userInfo?userVid={}",
                crypto::url_encode(&session.vid)
            );
            if let Ok(user) = authed_get(transport, session, ctx, &user_url) {
                if let Ok(text) = body_text(&user) {
                    if let Some(name) = crate::weread::jsonutil::object_string(&text, "name", 32) {
                        session.name = name;
                    }
                }
            }
            if !session.has_api_key() {
                let key_url = format!("{WEB_ORIGIN}/api/skills/apikeyGet?only_show=1");
                if let Ok(key_response) = authed_get(transport, session, ctx, &key_url) {
                    if let Ok(text) = body_text(&key_response) {
                        if let Some(key) =
                            crate::weread::jsonutil::object_string(&text, "apikey", 80)
                        {
                            if crate::weread::session::valid_api_key(&key) {
                                session.api_key = key;
                            }
                        }
                    }
                }
            }
            session.handshake.clear();
            Ok(JobOutput::SignedIn {
                name: session.name.clone(),
            })
        }
    }
}

fn shelf(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
) -> Result<JobOutput, JobError> {
    if session.web_signed_in() {
        maybe_renew(transport, session, ctx)?;
        let response = call(
            transport,
            ctx,
            Request {
                method: "GET",
                url: protocol::shelf_url(),
                body: None,
                headers: protocol::browser_headers(
                    &session.cookie_header(),
                    &format!("{WEB_ORIGIN}/"),
                    true,
                ),
                max_bytes: MAX_JSON_BYTES,
            },
            true,
        )?;
        let text = body_text(&response)?;
        return Ok(JobOutput::Shelf {
            books: parse::parse_shelf(&text),
        });
    }
    if session.has_api_key() {
        let text = gateway(transport, session, ctx, "/shelf/sync", &[])?;
        return Ok(JobOutput::Shelf {
            books: parse::parse_shelf(&text),
        });
    }
    Err(JobError::Message(
        "Sign in with a QR code, or save an API key in the Wi-Fi portal.".into(),
    ))
}

fn open_book(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
    book_id: &str,
) -> Result<JobOutput, JobError> {
    if !parse::valid_id(book_id) {
        return Err(JobError::Message("Book id was rejected.".into()));
    }
    let mut detail;
    let chapters;
    let mut progress = None;
    let mut psvts = String::new();
    if session.web_signed_in() {
        maybe_renew(transport, session, ctx)?;
        let info = call(
            transport,
            ctx,
            Request {
                method: "GET",
                url: protocol::book_info_url(book_id),
                body: None,
                headers: protocol::browser_headers(
                    &session.cookie_header(),
                    &protocol::reader_url(book_id, None),
                    true,
                ),
                max_bytes: MAX_JSON_BYTES,
            },
            true,
        )?;
        let info_text = body_text(&info)?;
        let info_truncated = observe_payload("/web/book/info", info.status, &info_text, 0);
        detail = parse::parse_book_detail(&info_text, book_id);
        if !info_truncated {
            progress = parse::parse_progress(&info_text);
        }
        let catalog = match call(
            transport,
            ctx,
            Request {
                method: "POST",
                url: format!("{WEB_ORIGIN}/web/book/chapterInfos"),
                body: Some(protocol::chapter_infos_body(book_id)),
                headers: protocol::browser_headers(
                    &session.cookie_header(),
                    &protocol::reader_url(book_id, None),
                    true,
                ),
                max_bytes: MAX_JSON_BYTES,
            },
            true,
        ) {
            Ok(response) => response,
            Err(error) => return Err(catalog_limit(error, "/web/book/chapterInfos")),
        };
        let text = body_text(&catalog)?;
        let truncated = parse::catalog_truncated(&text);
        chapters = if truncated {
            Vec::new()
        } else {
            parse::parse_chapters(&text)
        };
        observe_payload(
            "/web/book/chapterInfos",
            catalog.status,
            &text,
            chapters.len(),
        );
        if truncated {
            return Err(JobError::Message(
                "WeRead catalog response was truncated.".into(),
            ));
        }
        if let Some(format) = crate::weread::jsonutil::object_string(&text, "format", 16) {
            if let Some(detail) = detail.as_mut() {
                if detail.format.is_empty() {
                    detail.format = format;
                }
            }
        }
    } else if session.has_api_key() {
        let (info_status, info) = gateway_response(
            transport,
            session,
            ctx,
            "/book/info",
            &[("bookId", book_id)],
        )?;
        let info_truncated = observe_payload("/book/info", info_status, &info, 0);
        detail = parse::parse_book_detail(&info, book_id);
        if !info_truncated {
            progress = parse::parse_progress(&info);
        }
        let (catalog_status, catalog) = match gateway_response(
            transport,
            session,
            ctx,
            "/book/chapterinfo",
            &[("bookId", book_id)],
        ) {
            Ok(response) => response,
            Err(error) => return Err(catalog_limit(error, "/book/chapterinfo")),
        };
        let truncated = parse::catalog_truncated(&catalog);
        chapters = if truncated {
            Vec::new()
        } else {
            parse::parse_chapters(&catalog)
        };
        observe_payload(
            "/book/chapterinfo",
            catalog_status,
            &catalog,
            chapters.len(),
        );
        if truncated {
            return Err(JobError::Message(
                "WeRead catalog response was truncated.".into(),
            ));
        }
    } else {
        return Err(JobError::Message("Sign in to open this book.".into()));
    }
    let detail = detail.ok_or(JobError::Message(
        "WeRead did not return book details.".into(),
    ))?;
    if session.has_api_key() {
        if let Ok((status, text)) = gateway_response(
            transport,
            session,
            ctx,
            "/book/getprogress",
            &[("bookId", book_id)],
        ) {
            let truncated = observe_payload("/book/getprogress", status, &text, 0);
            if !truncated {
                if let Some(parsed) = parse::parse_progress(&text) {
                    progress = Some(parsed);
                }
            }
        }
    }
    if session.web_signed_in() {
        if let Ok(html) = fetch_reader_html(transport, session, ctx, book_id, None) {
            psvts = protocol::extract_psvts(&html).unwrap_or_default();
        }
    }
    Ok(JobOutput::Book {
        detail,
        chapters,
        progress,
        psvts,
    })
}

struct PreparedChapter {
    psvts: String,
    unix: u64,
    referer: String,
}

fn prepare_chapter(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
    book_id: &str,
    chapter_uid: &str,
    psvts: &str,
) -> Result<PreparedChapter, JobError> {
    if !session.web_signed_in() {
        return Err(JobError::Message("Chapter text needs a WeRead QR login. An API key only covers shelf, progress, and notes.".into()));
    }
    let unix = ctx.unix.ok_or(JobError::Clock)?;
    maybe_renew(transport, session, ctx)?;
    let mut psvts = psvts.to_string();
    if psvts.is_empty() {
        let html = fetch_reader_html(transport, session, ctx, book_id, Some(chapter_uid))?;
        psvts = protocol::extract_psvts(&html).ok_or(JobError::Message(
            "WeRead reader page had no session token. Sign in again.".into(),
        ))?;
    }
    Ok(PreparedChapter {
        psvts,
        unix,
        referer: protocol::reader_url(book_id, Some(chapter_uid)),
    })
}

fn chapter(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
    book_id: &str,
    chapter_uid: &str,
    _chapter_idx: u32,
    psvts: &str,
    fetch_images: bool,
) -> Result<JobOutput, JobError> {
    let prepared = prepare_chapter(transport, session, ctx, book_id, chapter_uid, psvts)?;
    if transport.streaming_download() {
        return stream_chapter(transport, session, ctx, book_id, chapter_uid, prepared);
    }
    let PreparedChapter {
        psvts,
        unix,
        referer,
    } = prepared;
    let e0 = post_shard(
        transport,
        session,
        ctx,
        "/web/book/chapter/e_0",
        book_id,
        chapter_uid,
        &psvts,
        unix,
        &referer,
    )?;
    if e0.body.starts_with(b"PK\x03\x04") {
        let text =
            text::zip_html_text(&e0.body).map_err(|error| JobError::Message(error.into()))?;
        let blocks = text::blocks_from_markup(&text);
        return finish_chapter(transport, ctx, blocks, psvts, "epub", fetch_images);
    }
    let e0_text = String::from_utf8_lossy(&e0.body);
    if e0_text.trim() == "{}" {
        return Err(JobError::Message(
            "WeRead returned an empty chapter. The session may lack access.".into(),
        ));
    }
    if e0_text.starts_with('{') && e0_text.contains("\"bookId\"") {
        let t0 = post_shard(
            transport,
            session,
            ctx,
            "/web/book/chapter/t_0",
            book_id,
            chapter_uid,
            &psvts,
            unix,
            &referer,
        )?;
        let t1 = post_shard(
            transport,
            session,
            ctx,
            "/web/book/chapter/t_1",
            book_id,
            chapter_uid,
            &psvts,
            unix,
            &referer,
        )?;
        let plain = decode_pair(&t0.body, &t1.body)?;
        let blocks = text::blocks_from_markup(&plain);
        return finish_chapter(transport, ctx, blocks, psvts, "txt", fetch_images);
    }
    let e1 = post_shard(
        transport,
        session,
        ctx,
        "/web/book/chapter/e_1",
        book_id,
        chapter_uid,
        &psvts,
        unix,
        &referer,
    )?;
    let e3 = post_shard(
        transport,
        session,
        ctx,
        "/web/book/chapter/e_3",
        book_id,
        chapter_uid,
        &psvts,
        unix,
        &referer,
    )?;
    let shards = [
        String::from_utf8_lossy(&e0.body).into_owned(),
        String::from_utf8_lossy(&e1.body).into_owned(),
        String::from_utf8_lossy(&e3.body).into_owned(),
    ];
    let refs: Vec<&str> = shards.iter().map(String::as_str).collect();
    let bytes = decode::decode_shards(&refs).map_err(|error| JobError::Message(error.into()))?;
    if bytes.starts_with(b"PK\x03\x04") {
        let text = text::zip_html_text(&bytes).map_err(|error| JobError::Message(error.into()))?;
        return finish_chapter(
            transport,
            ctx,
            text::blocks_from_markup(&text),
            psvts,
            "epub",
            fetch_images,
        );
    }
    let markup = String::from_utf8_lossy(&bytes).into_owned();
    finish_chapter(
        transport,
        ctx,
        text::blocks_from_markup(&markup),
        psvts,
        "epub",
        fetch_images,
    )
}

fn finish_chapter(
    transport: &mut dyn Transport,
    ctx: &mut CallCtx,
    blocks: Vec<Block>,
    psvts: String,
    format: &str,
    fetch_images: bool,
) -> Result<JobOutput, JobError> {
    let mut images = Vec::new();
    if fetch_images {
        for block in &blocks {
            if images.len() >= MAX_CHAPTER_IMAGES {
                break;
            }
            let Block::Image(image) = block else {
                continue;
            };
            if !bitmap::allowed_asset_url(&image.url) {
                images.push(ChapterImage {
                    alt: image.alt.clone(),
                    bitmap: None,
                });
                continue;
            }
            let fetched = call(
                transport,
                ctx,
                Request {
                    method: "GET",
                    url: image.url.clone(),
                    body: None,
                    headers: protocol::browser_headers("", WEB_ORIGIN, false),
                    max_bytes: MAX_IMAGE_BYTES,
                },
                false,
            );
            let bitmap = fetched
                .ok()
                .and_then(|response| bitmap::decode_mono(&response.body, 400, 480).ok());
            images.push(ChapterImage {
                alt: image.alt.clone(),
                bitmap,
            });
        }
    }
    let text = text::plain_from_blocks(&blocks);
    Ok(JobOutput::Chapter {
        blocks,
        text,
        images,
        psvts,
        format: format.into(),
    })
}

fn upload_progress(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
    book_id: &str,
    chapter_uid: &str,
    chapter_idx: u32,
    chapter_offset: u32,
    summary: &str,
    progress: u8,
    psvts: &str,
) -> Result<JobOutput, JobError> {
    if !session.web_signed_in() {
        return Err(JobError::Message(
            "Progress upload needs a WeRead login.".into(),
        ));
    }
    let unix = ctx.unix.ok_or(JobError::Clock)?;
    maybe_renew(transport, session, ctx)?;
    let mut psvts = psvts.to_string();
    if psvts.is_empty() {
        let html = fetch_reader_html(transport, session, ctx, book_id, Some(chapter_uid))?;
        psvts = protocol::extract_psvts(&html).unwrap_or_default();
    }
    if psvts.is_empty() {
        return Err(JobError::Message(
            "Missing reader token for progress upload.".into(),
        ));
    }
    let rn = ctx.next_u32() % 1000;
    let ts = unix
        .saturating_mul(1000)
        .saturating_add(u64::from(ctx.next_u32() % 1000));
    let body = protocol::progress_json(
        book_id,
        chapter_uid,
        chapter_idx,
        chapter_offset,
        summary,
        progress,
        &psvts,
        unix,
        ts,
        rn,
    );
    let response = call(
        transport,
        ctx,
        Request {
            method: "POST",
            url: format!("{WEB_ORIGIN}/web/book/read"),
            body: Some(body),
            headers: protocol::browser_headers(
                &session.cookie_header(),
                &protocol::reader_url(book_id, Some(chapter_uid)),
                true,
            ),
            max_bytes: 8 * 1024,
        },
        true,
    )?;
    let text = body_text(&response)?;
    if parse::renewal_succeeded(&text) || crate::weread::jsonutil::is_truthy(&text, "succ") {
        Ok(JobOutput::ProgressUploaded)
    } else {
        Err(JobError::Message(
            "WeRead rejected the progress update.".into(),
        ))
    }
}

fn notes(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
    book_id: &str,
) -> Result<JobOutput, JobError> {
    if !session.has_api_key() {
        return Err(JobError::Message(
            "Highlights and notes use the official API. Save a wrk- key in the Wi-Fi portal."
                .into(),
        ));
    }
    let marks = gateway(
        transport,
        session,
        ctx,
        "/book/bookmarklist",
        &[("bookId", book_id)],
    )?;
    let reviews = gateway(
        transport,
        session,
        ctx,
        "/review/list/mine",
        &[("bookid", book_id), ("count", "20")],
    )?;
    let mut lines = parse::parse_notes(&marks);
    lines.extend(parse::parse_notes(&reviews));
    lines.truncate(crate::weread::limits::MAX_NOTES);
    Ok(JobOutput::Notes { lines })
}

fn cover(
    transport: &mut dyn Transport,
    ctx: &mut CallCtx,
    book_id: &str,
    url: &str,
) -> Result<JobOutput, JobError> {
    if !bitmap::allowed_asset_url(url) {
        return Err(JobError::Message("Cover host is not allowed.".into()));
    }
    let response = call(
        transport,
        ctx,
        Request {
            method: "GET",
            url: url.to_string(),
            body: None,
            headers: protocol::browser_headers("", WEB_ORIGIN, false),
            max_bytes: MAX_IMAGE_BYTES,
        },
        false,
    )?;
    let bitmap = bitmap::decode_mono(&response.body, 48, 64)
        .map_err(|error| JobError::Message(error.into()))?;
    Ok(JobOutput::Cover {
        book_id: book_id.to_string(),
        bitmap,
    })
}

enum ShardPlan {
    Zip,
    Empty,
    Txt,
    Shards,
}

fn shard_plan(prefix: &[u8]) -> ShardPlan {
    let prefix = &prefix[..prefix.len().min(DOWNLOAD_CLASSIFY_BYTES)];
    if prefix.starts_with(b"PK\x03\x04") {
        return ShardPlan::Zip;
    }
    let text = String::from_utf8_lossy(prefix);
    let compact = text.trim();
    if !compact.starts_with('{') {
        return ShardPlan::Shards;
    }
    if compact == "{}" || compact.starts_with("{}") {
        return ShardPlan::Empty;
    }
    if compact.contains("\"bookId\"") {
        return ShardPlan::Txt;
    }
    ShardPlan::Empty
}

fn stream_chapter(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
    book_id: &str,
    chapter_uid: &str,
    prepared: PreparedChapter,
) -> Result<JobOutput, JobError> {
    let e0 = stream_part(
        transport,
        session,
        ctx,
        "/web/book/chapter/e_0",
        "e0",
        book_id,
        chapter_uid,
        &prepared.psvts,
        prepared.unix,
        &prepared.referer,
    )?;
    match shard_plan(&e0.prefix) {
        ShardPlan::Zip => {}
        ShardPlan::Empty => {
            return Err(JobError::Message(
                "WeRead returned an empty chapter. The session may lack access.".into(),
            ));
        }
        ShardPlan::Txt => {
            stream_part(
                transport,
                session,
                ctx,
                "/web/book/chapter/t_0",
                "t0",
                book_id,
                chapter_uid,
                &prepared.psvts,
                prepared.unix,
                &prepared.referer,
            )?;
            stream_part(
                transport,
                session,
                ctx,
                "/web/book/chapter/t_1",
                "t1",
                book_id,
                chapter_uid,
                &prepared.psvts,
                prepared.unix,
                &prepared.referer,
            )?;
        }
        ShardPlan::Shards => {
            stream_part(
                transport,
                session,
                ctx,
                "/web/book/chapter/e_1",
                "e1",
                book_id,
                chapter_uid,
                &prepared.psvts,
                prepared.unix,
                &prepared.referer,
            )?;
            stream_part(
                transport,
                session,
                ctx,
                "/web/book/chapter/e_3",
                "e3",
                book_id,
                chapter_uid,
                &prepared.psvts,
                prepared.unix,
                &prepared.referer,
            )?;
        }
    }
    Ok(JobOutput::ChapterStored {
        psvts: prepared.psvts,
    })
}

fn stream_part(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
    path: &str,
    part: &'static str,
    book_id: &str,
    chapter_uid: &str,
    psvts: &str,
    unix: u64,
    referer: &str,
) -> Result<StreamedPart, JobError> {
    if transport.cancelled() {
        return Err(JobError::Cancelled);
    }
    if ctx.first {
        ctx.first = false;
    } else {
        transport.idle();
    }
    if transport.cancelled() {
        return Err(JobError::Cancelled);
    }
    let square = u64::from(ctx.next_u32() % 10_000).saturating_pow(2);
    let body = protocol::content_json(book_id, chapter_uid, psvts, unix, square, false);
    if body.len() > MAX_JSON_BYTES {
        return Err(JobError::Message(
            "request body exceeds the size limit".into(),
        ));
    }
    let streamed = transport
        .call_stream(
            &Request {
                method: "POST",
                url: format!("{WEB_ORIGIN}{path}"),
                body: Some(body),
                headers: protocol::browser_headers(&session.cookie_header(), referer, true),
                max_bytes: MAX_SHARD_BYTES,
            },
            part,
        )
        .map_err(transport_job_error)?;
    if streamed.total > MAX_SHARD_BYTES {
        return Err(JobError::Message("response exceeds size limit".into()));
    }
    if let Some(error) = body_transfer_error(
        streamed.content_length,
        streamed.total,
        streamed.chunked,
        streamed.terminal_chunk,
    ) {
        return Err(JobError::Message(error.into()));
    }
    session.absorb_set_cookie(&streamed.set_cookie);
    let prefix = String::from_utf8_lossy(
        &streamed.prefix[..streamed.prefix.len().min(DOWNLOAD_CLASSIFY_BYTES)],
    );
    match parse::classify(streamed.status, &prefix) {
        ResponseClass::Expired => Err(JobError::Expired),
        ResponseClass::Upgrade => Err(JobError::Message(
            crate::weread::jsonutil::upgrade_message(&prefix)
                .unwrap_or_else(|| "WeRead asked for a client update.".into()),
        )),
        ResponseClass::Rejected => Err(JobError::Message(format!(
            "WeRead rejected the request (HTTP {}).",
            streamed.status
        ))),
        ResponseClass::Ok => Ok(streamed),
    }
}

fn post_shard(
    transport: &mut dyn Transport,
    session: &Session,
    ctx: &mut CallCtx,
    path: &str,
    book_id: &str,
    chapter_uid: &str,
    psvts: &str,
    unix: u64,
    referer: &str,
) -> Result<Response, JobError> {
    let square = u64::from(ctx.next_u32() % 10_000).saturating_pow(2);
    let body = protocol::content_json(book_id, chapter_uid, psvts, unix, square, false);
    call(
        transport,
        ctx,
        Request {
            method: "POST",
            url: format!("{WEB_ORIGIN}{path}"),
            body: Some(body),
            headers: protocol::browser_headers(&session.cookie_header(), referer, true),
            max_bytes: MAX_SHARD_BYTES,
        },
        true,
    )
}

fn fetch_reader_html(
    transport: &mut dyn Transport,
    session: &Session,
    ctx: &mut CallCtx,
    book_id: &str,
    chapter_uid: Option<&str>,
) -> Result<String, JobError> {
    let response = call(
        transport,
        ctx,
        Request {
            method: "GET",
            url: protocol::reader_url(book_id, chapter_uid),
            body: None,
            headers: protocol::browser_headers(
                &session.cookie_header(),
                &format!("{WEB_ORIGIN}/"),
                false,
            ),
            max_bytes: MAX_HTML_BYTES,
        },
        true,
    )?;
    body_text(&response)
}

fn gateway(
    transport: &mut dyn Transport,
    session: &Session,
    ctx: &mut CallCtx,
    api_name: &str,
    fields: &[(&str, &str)],
) -> Result<String, JobError> {
    gateway_response(transport, session, ctx, api_name, fields).map(|(_, text)| text)
}

fn gateway_response(
    transport: &mut dyn Transport,
    session: &Session,
    ctx: &mut CallCtx,
    api_name: &str,
    fields: &[(&str, &str)],
) -> Result<(u16, String), JobError> {
    let response = call(
        transport,
        ctx,
        Request {
            method: "POST",
            url: protocol::OFFICIAL_GATEWAY.into(),
            body: Some(protocol::gateway_body(api_name, fields)),
            headers: protocol::official_headers(&session.api_key),
            max_bytes: MAX_JSON_BYTES,
        },
        true,
    )?;
    let status = response.status;
    body_text(&response).map(|text| (status, text))
}

/// One catalog or progress diagnostic. The endpoint is a path only: no query
/// string, no cookies, no request body.
fn observe_payload(endpoint: &str, status: u16, body: &str, chapters: usize) -> bool {
    let truncated = parse::catalog_truncated(body);
    log::info!(
        "{}",
        payload_log_line(
            endpoint,
            status,
            body.len(),
            crate::weread::jsonutil::errcode(body),
            chapters,
            truncated
        )
    );
    truncated
}

fn catalog_limit(error: JobError, endpoint: &str) -> JobError {
    match error {
        JobError::Message(message) if message.contains("exceeds size limit") => {
            log::info!("{}", payload_log_line(endpoint, 0, 0, None, 0, true));
            JobError::Message("WeRead catalog response was truncated.".into())
        }
        other => other,
    }
}

fn payload_log_line(
    endpoint: &str,
    status: u16,
    bytes: usize,
    errcode: Option<i64>,
    chapters: usize,
    truncated: bool,
) -> String {
    let code = match errcode {
        Some(code) => code.to_string(),
        None => "none".into(),
    };
    format!(
        "rustmix-wave=weread-payload endpoint={} status={status} bytes={bytes} errcode={code} chapters={chapters} truncated={truncated}",
        endpoint_path(endpoint)
    )
}

fn endpoint_path(endpoint: &str) -> &str {
    let without_query = endpoint.split(['?', '#']).next().unwrap_or(endpoint);
    let Some(scheme) = without_query.find("://") else {
        return without_query;
    };
    let rest = &without_query[scheme + 3..];
    rest.find('/').map(|slash| &rest[slash..]).unwrap_or("/")
}

fn authed_get(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
    url: &str,
) -> Result<Response, JobError> {
    let mut headers =
        protocol::browser_headers(&session.cookie_header(), &protocol::skills_page_url(), true);
    headers.push(("X-Vid".into(), session.vid.clone()));
    headers.push(("X-Skey".into(), session.skey.clone()));
    let response = call(
        transport,
        ctx,
        Request {
            method: "GET",
            url: url.into(),
            body: None,
            headers,
            max_bytes: MAX_JSON_BYTES,
        },
        true,
    )?;
    session.absorb_set_cookie(&response.set_cookie);
    Ok(response)
}

fn maybe_renew(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
) -> Result<(), JobError> {
    let Some(now) = ctx.unix else {
        return Ok(());
    };
    if !session.needs_renewal(now) {
        return Ok(());
    }
    renew(transport, session, ctx)
}

fn renew(
    transport: &mut dyn Transport,
    session: &mut Session,
    ctx: &mut CallCtx,
) -> Result<(), JobError> {
    if ctx.renewed || !session.web_signed_in() {
        return Ok(());
    }
    ctx.renewed = true;
    let response = call(
        transport,
        ctx,
        Request {
            method: "POST",
            url: format!("{WEB_ORIGIN}/web/login/renewal"),
            body: Some(protocol::RENEWAL_BODY.into()),
            headers: protocol::browser_headers(
                &session.cookie_header(),
                &format!("{WEB_ORIGIN}/"),
                true,
            ),
            max_bytes: 8 * 1024,
        },
        false,
    )?;
    let previous_skey = session.skey.clone();
    session.absorb_set_cookie(&response.set_cookie);
    let text = body_text(&response).unwrap_or_default();
    if !parse::renewal_succeeded(&text) {
        session.expire_web();
        return Err(JobError::Expired);
    }
    if session.skey == previous_skey || session.skey.is_empty() {
        return Err(JobError::Message(
            "WeRead session renewal did not return a new key.".into(),
        ));
    }
    if let Some(now) = ctx.unix {
        session.skey_unix = now;
    }
    Ok(())
}

fn call(
    transport: &mut dyn Transport,
    ctx: &mut CallCtx,
    request: Request,
    retry_expired: bool,
) -> Result<Response, JobError> {
    if transport.cancelled() {
        return Err(JobError::Cancelled);
    }
    if ctx.first {
        ctx.first = false;
    } else {
        transport.idle();
    }
    if transport.cancelled() {
        return Err(JobError::Cancelled);
    }
    if let Some(len) = request.body.as_ref().map(String::len) {
        if len > MAX_JSON_BYTES {
            return Err(JobError::Message(
                "request body exceeds the size limit".into(),
            ));
        }
    }
    let response = transport.call(&request).map_err(transport_job_error)?;
    if let Some(len) = response.content_length {
        if len > request.max_bytes {
            return Err(JobError::Message("response exceeds size limit".into()));
        }
    }
    if response.body.len() > request.max_bytes {
        return Err(JobError::Message("response exceeds size limit".into()));
    }
    let declared = response
        .content_length
        .map(|len| i64::try_from(len).unwrap_or(i64::MAX))
        .unwrap_or(-1);
    if let Some(error) = body_transfer_error(declared, response.body.len(), false, true) {
        return Err(JobError::Message(error.into()));
    }
    let text = String::from_utf8_lossy(&response.body);
    match parse::classify(response.status, &text) {
        ResponseClass::Expired if retry_expired && !ctx.renewed => {
            // Renewal needs the session cookie. The caller handles renewal before
            // content calls. A mid-flight expiry is reported so the UI can show a QR.
            Err(JobError::Expired)
        }
        ResponseClass::Expired => Err(JobError::Expired),
        ResponseClass::Upgrade => Err(JobError::Message(
            crate::weread::jsonutil::upgrade_message(&text)
                .unwrap_or_else(|| "WeRead asked for a client update.".into()),
        )),
        ResponseClass::Rejected => Err(JobError::Message(format!(
            "WeRead rejected the request (HTTP {}).",
            response.status
        ))),
        ResponseClass::Ok => Ok(response),
    }
}

fn body_text(response: &Response) -> Result<String, JobError> {
    if response.body.len() > MAX_SHARD_BYTES {
        return Err(JobError::Message("response exceeds size limit".into()));
    }
    Ok(String::from_utf8_lossy(&response.body).into_owned())
}

fn decode_pair(first: &[u8], second: &[u8]) -> Result<String, JobError> {
    let a = String::from_utf8_lossy(first).into_owned();
    let b = String::from_utf8_lossy(second).into_owned();
    let bytes =
        decode::decode_shards(&[&a, &b]).map_err(|error| JobError::Message(error.into()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn transport_job_error(error: String) -> JobError {
    if error == "cancelled" {
        JobError::Cancelled
    } else {
        JobError::Message(trim_message(&error))
    }
}

fn trim_message(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(80)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        perform, Job, JobError, JobOutput, Request, Response, StreamedPart, Transport, Work,
    };
    use crate::weread::{decode::seal_plain, session::Session};

    struct Script {
        steps: Vec<Response>,
        index: usize,
        pub waits: usize,
        pub urls: Vec<String>,
    }

    impl Transport for Script {
        fn idle(&mut self) {
            self.waits += 1;
        }
        fn call(&mut self, request: &Request) -> Result<Response, String> {
            self.urls.push(request.url.clone());
            if let Some(len) = request.body.as_ref().map(String::len) {
                if len > request.max_bytes {
                    return Err("request too large".into());
                }
            }
            let response = self
                .steps
                .get(self.index)
                .cloned()
                .ok_or_else(|| "script ended".to_string())?;
            self.index += 1;
            if response.body.len() > request.max_bytes {
                return Err("response exceeds size limit".into());
            }
            Ok(response)
        }
    }

    fn json_response(body: &str) -> Response {
        Response {
            status: 200,
            body: body.as_bytes().to_vec(),
            set_cookie: String::new(),
            content_length: Some(body.len()),
        }
    }

    #[test]
    fn login_poll_and_signed_chapter_round_trip() {
        let mut script = Script {
            steps: vec![
                json_response("<html>ok</html>"),
                json_response(r#"{"uid":"uid_1"}"#),
            ],
            index: 0,
            waits: 0,
            urls: Vec::new(),
        };
        let report = perform(
            &mut script,
            Work {
                generation: 1,
                job: Job::LoginUid,
                session: Session::default(),
            },
            Some(1_780_488_000),
        );
        let JobOutput::Qr { uid, url, .. } = report.result.unwrap() else {
            panic!("expected qr");
        };
        assert_eq!(uid, "uid_1");
        assert!(url.contains("web/confirm?uid=uid_1"));
        assert!(script.waits >= 1);

        let shard = seal_plain("<p>Hello 微信</p>");
        let mut reader = Script {
            steps: vec![
                Response {
                    status: 200,
                    body: br#"{"succ":1}"#.to_vec(),
                    set_cookie: "wr_skey=tok2; Path=/".into(),
                    content_length: Some(10),
                },
                Response {
                    status: 200,
                    body: format!(r#"{{"reader":{{"psvts":"ps-token"}}}}"#).into_bytes(),
                    set_cookie: String::new(),
                    content_length: None,
                },
                Response {
                    status: 200,
                    body: shard.into_bytes(),
                    set_cookie: String::new(),
                    content_length: None,
                },
                json_response(""),
                json_response(""),
            ],
            index: 0,
            waits: 0,
            urls: Vec::new(),
        };
        let mut session = Session::default();
        session.vid = "9".into();
        session.skey = "tok".into();
        session.skey_unix = 1;
        let report = perform(
            &mut reader,
            Work {
                generation: 2,
                job: Job::Chapter {
                    book_id: "43208843".into(),
                    chapter_uid: "2".into(),
                    chapter_idx: 2,
                    psvts: String::new(),
                    fetch_images: false,
                },
                session,
            },
            Some(1_780_488_000),
        );
        let JobOutput::Chapter { text, .. } = report.result.unwrap() else {
            panic!("expected chapter");
        };
        assert!(text.contains("Hello"));
        assert!(text.contains("微信"));
        assert!(reader
            .urls
            .iter()
            .any(|url| url.contains("/web/login/renewal")));
        assert!(reader
            .urls
            .iter()
            .any(|url| url.contains("/web/book/chapter/e_0")));
    }

    #[test]
    fn expired_session_is_reported_without_echoing_secrets() {
        let mut script = Script {
            steps: vec![json_response(r#"{"errcode":-2012}"#)],
            index: 0,
            waits: 0,
            urls: Vec::new(),
        };
        let mut session = Session::default();
        session.vid = "9".into();
        session.skey = "super-secret".into();
        session.skey_unix = 1_780_488_000;
        let report = perform(
            &mut script,
            Work {
                generation: 3,
                job: Job::Shelf,
                session,
            },
            Some(1_780_488_000),
        );
        let Err(JobError::Expired) = report.result else {
            panic!("expected expiry");
        };
        let rendered = JobError::Expired.to_string();
        assert!(!rendered.contains("super-secret"));
    }

    #[test]
    fn official_notes_need_an_api_key_and_parse() {
        let mut missing = Script {
            steps: Vec::new(),
            index: 0,
            waits: 0,
            urls: Vec::new(),
        };
        let report = perform(
            &mut missing,
            Work {
                generation: 4,
                job: Job::Notes {
                    book_id: "43208843".into(),
                },
                session: Session::default(),
            },
            Some(1_780_488_000),
        );
        assert!(matches!(report.result, Err(JobError::Message(_))));

        let mut script = Script {
            steps: vec![
                json_response(r#"{"updated":[{"markText":"划线","chapterUid":2}]}"#),
                json_response(
                    r#"{"reviews":[{"review":{"content":"想法","chapterName":"第二章"}}]}"#,
                ),
            ],
            index: 0,
            waits: 0,
            urls: Vec::new(),
        };
        let mut session = Session::default();
        session.api_key = "wrk-abcDEF1234".into();
        let report = perform(
            &mut script,
            Work {
                generation: 5,
                job: Job::Notes {
                    book_id: "43208843".into(),
                },
                session,
            },
            Some(1_780_488_000),
        );
        let JobOutput::Notes { lines } = report.result.unwrap() else {
            panic!("notes");
        };
        assert_eq!(lines.len(), 2);
        assert!(script
            .urls
            .iter()
            .all(|url| url.contains("/api/agent/gateway")));
    }

    #[test]
    fn renewal_without_a_new_skey_is_an_error() {
        let unix = 1_780_488_000;
        let mut unchanged = Script {
            steps: vec![
                Response {
                    status: 200,
                    body: br#"{"succ":1}"#.to_vec(),
                    set_cookie: "wr_vid=9; Path=/, wr_rt=same-rt".into(),
                    content_length: Some(10),
                },
                json_response(r#"{"books":[]}"#),
            ],
            index: 0,
            waits: 0,
            urls: Vec::new(),
        };
        let mut session = Session::default();
        session.vid = "9".into();
        session.skey = "old-key".into();
        session.skey_unix = 1;
        let report = perform(
            &mut unchanged,
            Work {
                generation: 1,
                job: Job::Shelf,
                session: session.clone(),
            },
            Some(unix),
        );
        let Err(JobError::Message(message)) = report.result else {
            panic!("renewal without wr_skey must fail");
        };
        assert!(message.contains("new key"));
        assert_eq!(report.session.skey, "old-key");
        assert_eq!(report.session.skey_unix, 1);
        assert!(unchanged
            .urls
            .iter()
            .all(|url| url.contains("/web/login/renewal")));
        assert!(!unchanged.urls.iter().any(|url| url.contains("shelf")));

        let mut same_key = Script {
            steps: vec![Response {
                status: 200,
                body: br#"{"succ":1}"#.to_vec(),
                set_cookie: "wr_skey=old-key; Path=/".into(),
                content_length: Some(10),
            }],
            index: 0,
            waits: 0,
            urls: Vec::new(),
        };
        let report = perform(
            &mut same_key,
            Work {
                generation: 3,
                job: Job::Shelf,
                session: session.clone(),
            },
            Some(unix),
        );
        assert!(matches!(report.result, Err(JobError::Message(_))));
        assert_eq!(report.session.skey_unix, 1);
    }

    #[test]
    fn renewal_updates_skey_unix_when_wr_skey_changes() {
        let unix = 1_780_488_000;
        let mut session = Session::default();
        session.vid = "9".into();
        session.skey = "old-key".into();
        session.skey_unix = 1;
        let mut rotated = Script {
            steps: vec![
                Response {
                    status: 200,
                    body: br#"{"succ":1}"#.to_vec(),
                    set_cookie: "wr_skey=new-key; Path=/".into(),
                    content_length: Some(10),
                },
                json_response(r#"{"books":[]}"#),
            ],
            index: 0,
            waits: 0,
            urls: Vec::new(),
        };
        let report = perform(
            &mut rotated,
            Work {
                generation: 2,
                job: Job::Shelf,
                session,
            },
            Some(unix),
        );
        assert_eq!(report.session.skey, "new-key");
        assert_eq!(report.session.skey_unix, unix);
    }

    #[test]
    fn cancel_sets_the_flag_and_closes_the_client() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        let mut closed = false;
        super::signal_cancel(&flag, &mut || closed = true);
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
        assert!(closed);
    }

    #[test]
    fn truncated_and_unchunked_downloads_are_not_stored() {
        let mut short = RecordingStream {
            steps: vec![Response {
                status: 200,
                body: b"AAAA".to_vec(),
                set_cookie: String::new(),
                content_length: Some(100),
            }],
            index: 0,
            urls: Vec::new(),
            chunk_lens: Vec::new(),
        };
        let report = perform(
            &mut short,
            Work {
                generation: 1,
                job: chapter_job(false),
                session: stream_session(),
            },
            Some(1_780_488_000),
        );
        let Err(JobError::Message(message)) = report.result else {
            panic!("short Content-Length must not be stored");
        };
        assert!(message.contains("Content-Length"));

        struct OpenChunk;
        impl Transport for OpenChunk {
            fn idle(&mut self) {}
            fn call(&mut self, _request: &Request) -> Result<Response, String> {
                Err("unused".into())
            }
            fn streaming_download(&self) -> bool {
                true
            }
            fn call_stream(
                &mut self,
                _request: &Request,
                _part: &'static str,
            ) -> Result<StreamedPart, String> {
                Ok(StreamedPart {
                    status: 200,
                    set_cookie: String::new(),
                    prefix: b"AAAA".to_vec(),
                    total: 4,
                    content_length: -1,
                    chunked: true,
                    terminal_chunk: false,
                })
            }
        }
        let report = perform(
            &mut OpenChunk,
            Work {
                generation: 2,
                job: chapter_job(false),
                session: stream_session(),
            },
            Some(1_780_488_000),
        );
        let Err(JobError::Message(message)) = report.result else {
            panic!("missing terminal chunk must not be stored");
        };
        assert!(message.contains("terminal chunk"));
    }

    #[test]
    fn payload_log_keeps_the_path_and_drops_query_secrets() {
        let line = super::payload_log_line(
            "https://weread.qq.com/web/book/chapterInfos?bookId=secret&wr_skey=cookie",
            200,
            4096,
            Some(0),
            34,
            false,
        );
        assert_eq!(
            line,
            "rustmix-wave=weread-payload endpoint=/web/book/chapterInfos status=200 bytes=4096 errcode=0 chapters=34 truncated=false"
        );
        assert!(!line.contains("secret"));
        assert!(!line.contains("cookie"));
        assert!(!line.contains('?'));
        let progress = super::payload_log_line("/book/getprogress", 200, 80, None, 0, true);
        assert!(progress.contains("endpoint=/book/getprogress"));
        assert!(progress.contains("errcode=none"));
        assert!(progress.contains("truncated=true"));
    }

    #[test]
    fn large_chapter_object_is_kept_and_truncated_catalog_errors() {
        let pad = "a".repeat(3 * 1024);
        let catalog = format!(
            r#"{{"data":[{{"bookId":"43208843","updated":[{{"chapterUid":7,"chapterIdx":1,"title":"锚点章","anchors":["{pad}"]}}]}}]}}"#
        );
        let mut script = Script {
            steps: vec![
                json_response(
                    r#"{"bookId":"43208843","title":"持续交付","author":"乔梁","progress":22,"chapterUid":7,"chapterOffset":2}"#,
                ),
                json_response(&catalog),
            ],
            index: 0,
            waits: 0,
            urls: Vec::new(),
        };
        let mut session = Session::default();
        session.vid = "9".into();
        session.skey = "tok".into();
        session.skey_unix = 1_780_488_000;
        let report = perform(
            &mut script,
            Work {
                generation: 6,
                job: Job::OpenBook {
                    book_id: "43208843".into(),
                },
                session: session.clone(),
            },
            Some(1_780_488_000),
        );
        let JobOutput::Book {
            chapters, progress, ..
        } = report.result.unwrap()
        else {
            panic!("expected book");
        };
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].uid, "7");
        assert_eq!(progress.unwrap().progress, 22);
        assert!(script
            .urls
            .iter()
            .any(|url| url.contains("/web/book/chapterInfos")));

        let mut truncated = Script {
            steps: vec![
                json_response(r#"{"bookId":"43208843","title":"持续交付"}"#),
                json_response(r#"{"updated":[{"chapterUid":1,"title":"第一章""#),
            ],
            index: 0,
            waits: 0,
            urls: Vec::new(),
        };
        let report = perform(
            &mut truncated,
            Work {
                generation: 7,
                job: Job::OpenBook {
                    book_id: "43208843".into(),
                },
                session,
            },
            Some(1_780_488_000),
        );
        let Err(JobError::Message(message)) = report.result else {
            panic!("truncated catalog must be an error");
        };
        assert!(message.contains("truncated"));
        assert!(!message.to_lowercase().contains("no chapters"));
    }

    struct RecordingStream {
        steps: Vec<Response>,
        index: usize,
        urls: Vec<String>,
        chunk_lens: Vec<usize>,
    }

    impl Transport for RecordingStream {
        fn idle(&mut self) {}
        fn call(&mut self, request: &Request) -> Result<Response, String> {
            self.urls.push(request.url.clone());
            let response = self
                .steps
                .get(self.index)
                .cloned()
                .ok_or_else(|| "script ended".to_string())?;
            self.index += 1;
            if response.body.len() > request.max_bytes {
                return Err("response exceeds size limit".into());
            }
            Ok(response)
        }
        fn streaming_download(&self) -> bool {
            true
        }
        fn call_stream(
            &mut self,
            request: &Request,
            _part: &'static str,
        ) -> Result<StreamedPart, String> {
            let response = self.call(request)?;
            let chunk_bytes = crate::weread::limits::DOWNLOAD_CHUNK_BYTES;
            for chunk in response.body.chunks(chunk_bytes) {
                assert!(chunk.len() <= chunk_bytes);
                self.chunk_lens.push(chunk.len());
            }
            let prefix_len = response
                .body
                .len()
                .min(crate::weread::limits::DOWNLOAD_CLASSIFY_BYTES);
            let content_length = response
                .content_length
                .map(|len| i64::try_from(len).unwrap_or(i64::MAX))
                .unwrap_or(-1);
            let terminal_chunk = response
                .content_length
                .is_none_or(|len| response.body.len() == len);
            Ok(StreamedPart {
                status: response.status,
                set_cookie: response.set_cookie,
                prefix: response.body[..prefix_len].to_vec(),
                total: response.body.len(),
                content_length,
                chunked: false,
                terminal_chunk,
            })
        }
    }

    fn stream_session() -> crate::weread::session::Session {
        let mut session = Session::default();
        session.vid = "9".into();
        session.skey = "tok".into();
        session.skey_unix = 1_780_488_000;
        session
    }

    fn chapter_job(fetch_images: bool) -> Job {
        Job::Chapter {
            book_id: "43208843".into(),
            chapter_uid: "2".into(),
            chapter_idx: 2,
            psvts: "ps".into(),
            fetch_images,
        }
    }

    #[test]
    fn streaming_download_keeps_each_response_chunk_bounded() {
        let chunk = crate::weread::limits::DOWNLOAD_CHUNK_BYTES;
        let shard = vec![b'A'; chunk + 100];
        let mut transport = RecordingStream {
            steps: vec![
                Response {
                    status: 200,
                    body: shard.clone(),
                    set_cookie: String::new(),
                    content_length: Some(shard.len()),
                },
                Response {
                    status: 200,
                    body: shard.clone(),
                    set_cookie: String::new(),
                    content_length: Some(shard.len()),
                },
                Response {
                    status: 200,
                    body: shard,
                    set_cookie: String::new(),
                    content_length: None,
                },
            ],
            index: 0,
            urls: Vec::new(),
            chunk_lens: Vec::new(),
        };
        let report = perform(
            &mut transport,
            Work {
                generation: 1,
                job: chapter_job(false),
                session: stream_session(),
            },
            Some(1_780_488_000),
        );
        let JobOutput::ChapterStored { .. } = report.result.unwrap() else {
            panic!("download stores the raw parts and does not return chapter text");
        };
        assert!(transport.chunk_lens.iter().any(|len| *len == chunk));
        assert!(transport.chunk_lens.iter().all(|len| *len <= chunk));
        assert!(transport
            .urls
            .iter()
            .any(|url| url.contains("/chapter/e_0")));
        assert!(transport
            .urls
            .iter()
            .any(|url| url.contains("/chapter/e_1")));
        assert!(transport
            .urls
            .iter()
            .any(|url| url.contains("/chapter/e_3")));
        assert!(!transport.urls.iter().any(|url| url.contains("/chapter/t_")));
    }

    #[test]
    fn streaming_download_classifies_zip_txt_and_empty_from_the_prefix() {
        let mut zip = RecordingStream {
            steps: vec![Response {
                status: 200,
                body: b"PK\x03\x04chapter-zip".to_vec(),
                set_cookie: String::new(),
                content_length: None,
            }],
            index: 0,
            urls: Vec::new(),
            chunk_lens: Vec::new(),
        };
        let report = perform(
            &mut zip,
            Work {
                generation: 1,
                job: chapter_job(false),
                session: stream_session(),
            },
            Some(1_780_488_000),
        );
        assert!(matches!(report.result, Ok(JobOutput::ChapterStored { .. })));
        assert_eq!(zip.urls.len(), 1);
        assert!(zip.urls[0].contains("/chapter/e_0"));

        let mut txt = RecordingStream {
            steps: vec![
                json_response(r#"{"bookId":"43208843","chapterUid":2}"#),
                Response {
                    status: 200,
                    body: b"t0-shard".to_vec(),
                    set_cookie: String::new(),
                    content_length: None,
                },
                Response {
                    status: 200,
                    body: b"t1-shard".to_vec(),
                    set_cookie: String::new(),
                    content_length: None,
                },
            ],
            index: 0,
            urls: Vec::new(),
            chunk_lens: Vec::new(),
        };
        let report = perform(
            &mut txt,
            Work {
                generation: 2,
                job: chapter_job(false),
                session: stream_session(),
            },
            Some(1_780_488_000),
        );
        assert!(matches!(report.result, Ok(JobOutput::ChapterStored { .. })));
        assert!(txt.urls.iter().any(|url| url.contains("/chapter/t_0")));
        assert!(txt.urls.iter().any(|url| url.contains("/chapter/t_1")));
        assert!(!txt.urls.iter().any(|url| url.contains("/chapter/e_1")));

        let mut empty = RecordingStream {
            steps: vec![json_response("{}")],
            index: 0,
            urls: Vec::new(),
            chunk_lens: Vec::new(),
        };
        let report = perform(
            &mut empty,
            Work {
                generation: 3,
                job: chapter_job(false),
                session: stream_session(),
            },
            Some(1_780_488_000),
        );
        let Err(JobError::Message(message)) = report.result else {
            panic!("empty chapter must fail");
        };
        assert!(message.contains("empty chapter"));
    }
}
