//! One bounded WeRead HTTPS job on a single long-lived worker.
//!
//! The main loop polls the worker. A channel carries each job so a chapter
//! download does not call `pthread_create` again. The worker stack is allocated
//! from PSRAM. A PSRAM stack must not call flash, NVS, or FATFS, because those
//! disable the flash cache. Offline download therefore reads each response in
//! small chunks on this worker and the main task (internal stack) writes them
//! to a temp file. Interactive reads still buffer one chapter. The HTTP client
//! is closed and cleaned up on this worker before the next job. Cancel only
//! sets a flag on the main task; this thread closes the client after the
//! current body read returns. Body reads time out after
//! `HTTP_READ_TIMEOUT_MS` so that flag is noticed without destroying the
//! TLS session from the other task. Every `Set-Cookie` is kept;
//! the ESP-IDF Rust client stores headers in a map and would drop all but the last.

use std::{
    ffi::{CStr, CString},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use esp_idf_svc::sys::{
    self, esp_http_client_cleanup, esp_http_client_close, esp_http_client_config_t,
    esp_http_client_event_id_t_HTTP_EVENT_ON_HEADER, esp_http_client_event_t,
    esp_http_client_fetch_headers, esp_http_client_get_content_length,
    esp_http_client_get_status_code, esp_http_client_handle_t, esp_http_client_init,
    esp_http_client_is_chunked_response, esp_http_client_is_complete_data_received,
    esp_http_client_method_t_HTTP_METHOD_GET, esp_http_client_method_t_HTTP_METHOD_POST,
    esp_http_client_open, esp_http_client_read, esp_http_client_set_header,
    esp_http_client_set_method, esp_http_client_set_timeout_ms, esp_http_client_write, ESP_OK,
};

use crate::{
    reader::ReaderLayout,
    runtime_memory::{log_runtime_memory, log_worker_memory},
    runtime_worker::LongLivedWorker,
    weread::{
        body::{self, BoundedBody},
        client::{self, Job, JobError, Report, Request, Response, Transport, Work},
        limits::{
            DOWNLOAD_CHUNK_BYTES, DOWNLOAD_CLASSIFY_BYTES, HTTP_IO_BUFFER_BYTES,
            HTTP_READ_TIMEOUT_MS, HTTP_TIMEOUT_SECS, MIN_REQUEST_GAP_MS,
        },
        offline::{self, DownloadEvent},
        session,
        ui::{self, ServiceOutcome, WereadUi},
    },
};

pub use crate::weread::limits::WEREAD_HTTP_WORKER_STACK_BYTES;

/// PSRAM stack for the one `weread-http` thread. Large enough for one mbedTLS
/// handshake, and not taken from the ~334 KiB internal heap on every chapter.
/// A ~2 MiB SD face parsed by fontdue already occupies more than 2 MiB of
/// PSRAM. Offline download no longer keeps the chapter body beside that face:
/// each read is [`DOWNLOAD_CHUNK_BYTES`] and the main task writes it. The 2 KiB
/// TLS I/O buffers stay in internal RAM.
const MAX_SET_COOKIE_BYTES: usize = 4 * 1024;

pub struct HttpJobs {
    inflight: Option<Inflight>,
}

struct Inflight {
    reply: mpsc::Receiver<Report>,
    chunks: Option<mpsc::Receiver<DownloadEvent>>,
    download: Option<offline::ChapterDownload>,
    write_error: Option<String>,
    chunks_closed: bool,
    cancel: Arc<AtomicBool>,
    generation: u64,
    job: Job,
    session: session::Session,
}

struct QueuedJob {
    work: Work,
    unix: Option<u64>,
    cancel: Arc<AtomicBool>,
    seed: u64,
    download_tx: Option<mpsc::SyncSender<DownloadEvent>>,
}

struct WorkerSlot {
    worker: LongLivedWorker<QueuedJob, Report>,
}

impl Default for HttpJobs {
    fn default() -> Self {
        Self { inflight: None }
    }
}

impl HttpJobs {
    #[must_use]
    pub fn busy(&self) -> bool {
        self.inflight.is_some()
    }

    pub fn poll(
        &mut self,
        ui: &mut WereadUi,
        unix: Option<u64>,
        now_ms: u64,
        layout: ReaderLayout,
        mounted: bool,
    ) -> ServiceOutcome {
        ui::drive_with(
            ui,
            &mut self.inflight,
            unix,
            now_ms,
            layout,
            mounted,
            spawn_work,
            poll_report,
            |job| client::request_cancel(&job.cancel),
        )
    }
}

fn poll_report(job: &mut Inflight) -> Option<Report> {
    drain_download(job);
    if !job.chunks_closed {
        return None;
    }
    match job.reply.try_recv() {
        Ok(mut report) => {
            if let Some(download) = job.download.take() {
                let succeeded = report.result.is_ok();
                if let Err(error) =
                    offline::complete_download(download, succeeded, job.write_error.take())
                {
                    if report.result.is_ok() {
                        report.result = Err(JobError::Message(error));
                    }
                }
            }
            Some(report)
        }
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            if let Some(download) = job.download.take() {
                download.abort();
            }
            Some(error_report(
                job.generation,
                job.job.clone(),
                job.session.clone(),
                "WeRead worker stopped".into(),
            ))
        }
    }
}

fn drain_download(job: &mut Inflight) {
    let Some(chunks) = job.chunks.take() else {
        job.chunks_closed = true;
        return;
    };
    loop {
        match chunks.try_recv() {
            Ok(event) => {
                if job.write_error.is_some() {
                    continue;
                }
                if let Some(download) = job.download.as_mut() {
                    if let Err(error) = download.apply(event) {
                        job.write_error = Some(error);
                    }
                }
            }
            Err(TryRecvError::Empty) => {
                job.chunks = Some(chunks);
                break;
            }
            Err(TryRecvError::Disconnected) => {
                job.chunks_closed = true;
                break;
            }
        }
    }
}

fn spawn_work(work: Work, unix: Option<u64>, cancel: Arc<AtomicBool>) -> Result<Inflight, Report> {
    let generation = work.generation;
    let job = work.job.clone();
    let session = work.session.clone();
    let (download, download_tx, chunks) = match &job {
        Job::Chapter {
            fetch_images: false,
            book_id,
            chapter_uid,
            chapter_idx,
            ..
        } => {
            match offline::ChapterDownload::begin(
                Path::new("/sdcard/RUSTMIX"),
                book_id,
                chapter_uid,
                *chapter_idx,
            ) {
                Ok(file) => {
                    let (tx, rx) = mpsc::sync_channel(2);
                    (Some(file), Some(tx), Some(rx))
                }
                Err(error) => {
                    return Err(error_report(generation, job, session, error));
                }
            }
        }
        _ => (None, None, None),
    };
    let chunks_closed = chunks.is_none();
    let queued = QueuedJob {
        work,
        unix,
        cancel: Arc::clone(&cancel),
        seed: random_seed(),
        download_tx,
    };
    match submit_job(queued) {
        Ok(reply) => Ok(Inflight {
            reply,
            chunks,
            download,
            write_error: None,
            chunks_closed,
            cancel,
            generation,
            job,
            session,
        }),
        Err(error) => {
            if let Some(file) = download {
                file.abort();
            }
            Err(error_report(
                generation,
                job,
                session,
                format!("WeRead worker failed to start: {error}"),
            ))
        }
    }
}

fn error_report(generation: u64, job: Job, session: session::Session, message: String) -> Report {
    log_runtime_memory("weread-http-spawn-failed");
    Report {
        generation,
        job,
        session,
        result: Err(JobError::Message(message)),
    }
}

fn submit_job(mut job: QueuedJob) -> Result<mpsc::Receiver<Report>, String> {
    for attempt in 0..2 {
        let worker = {
            let mut slot = worker_slot()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if slot.is_none() {
                match start_worker() {
                    Ok(worker) => *slot = Some(WorkerSlot { worker }),
                    Err(error) => return Err(error.to_string()),
                }
            }
            slot.as_ref().unwrap().worker.clone()
        };
        match worker.submit(job) {
            Ok(reply) => return Ok(reply),
            Err(returned) => {
                job = returned;
                let mut slot = worker_slot()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                *slot = None;
                if attempt == 1 {
                    return Err("Not enough memory".into());
                }
            }
        }
    }
    Err("Not enough memory".into())
}

fn worker_slot() -> &'static Mutex<Option<WorkerSlot>> {
    static SLOT: Mutex<Option<WorkerSlot>> = Mutex::new(None);
    &SLOT
}

fn start_worker() -> Result<LongLivedWorker<QueuedJob, Report>, std::io::Error> {
    log_runtime_memory("weread-http-spawn");
    let _psram_stack = PsramStackGuard::enter(WEREAD_HTTP_WORKER_STACK_BYTES);
    LongLivedWorker::spawn(
        "weread-http",
        WEREAD_HTTP_WORKER_STACK_BYTES,
        |job: QueuedJob| {
            log_worker_memory(
                "weread-http-before-job",
                "weread-http",
                WEREAD_HTTP_WORKER_STACK_BYTES,
            );
            let report = {
                let mut transport = EspTransport {
                    gap: true,
                    cancel: job.cancel,
                    download: job.download_tx,
                };
                let report =
                    client::perform_with_seed(&mut transport, job.work, job.unix, job.seed);
                drop(transport);
                report
            };
            log::info!("rustmix-wave=weread-http status=client-released");
            log_worker_memory(
                "weread-http-after-job",
                "weread-http",
                WEREAD_HTTP_WORKER_STACK_BYTES,
            );
            report
        },
    )
}

/// Sets pthread stack caps for the duration of one `pthread_create`.
///
/// Restoring the previous config keeps weather, EPUB, and Lua workers on
/// internal stacks. Those tasks touch the filesystem and NVS.
struct PsramStackGuard {
    restore: esp_idf_svc::sys::esp_pthread_cfg_t,
}

impl PsramStackGuard {
    fn enter(stack_bytes: usize) -> Self {
        unsafe {
            let fallback = sys::esp_pthread_get_default_config();
            let mut restore = fallback;
            if sys::esp_pthread_get_cfg(&mut restore) != ESP_OK {
                restore = fallback;
            }
            let mut cfg = restore;
            cfg.stack_size = stack_bytes;
            cfg.stack_alloc_caps = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
            cfg.inherit_cfg = false;
            cfg.thread_name = c"weread-http".as_ptr();
            if sys::esp_pthread_set_cfg(&cfg) == ESP_OK {
                log::info!(
                    "rustmix-wave=weread-http status=psram-stack stack-bytes={stack_bytes} caps=spiram"
                );
            } else {
                log::warn!(
                    "rustmix-wave=weread-http status=psram-stack-cfg-failed stack-bytes={stack_bytes} fallback=internal"
                );
            }
            Self { restore }
        }
    }
}

impl Drop for PsramStackGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = sys::esp_pthread_set_cfg(&self.restore);
        }
    }
}

fn random_seed() -> u64 {
    let high = unsafe { sys::esp_random() } as u64;
    let low = unsafe { sys::esp_random() } as u64;
    (high << 32) | low
}

struct EspTransport {
    gap: bool,
    cancel: Arc<AtomicBool>,
    download: Option<mpsc::SyncSender<DownloadEvent>>,
}

impl Transport for EspTransport {
    fn idle(&mut self) {
        if !self.gap {
            return;
        }
        let mut left = MIN_REQUEST_GAP_MS;
        while left > 0 {
            if self.cancelled() {
                return;
            }
            let slice = left.min(50);
            thread::sleep(Duration::from_millis(slice));
            left -= slice;
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn call(&mut self, request: &Request) -> Result<Response, String> {
        self.gap = true;
        if self.cancelled() {
            return Err("cancelled".into());
        }
        http_call(request, &self.cancel)
    }

    fn streaming_download(&self) -> bool {
        self.download.is_some()
    }

    fn call_stream(
        &mut self,
        request: &Request,
        part: &'static str,
    ) -> Result<client::StreamedPart, String> {
        self.gap = true;
        if self.cancelled() {
            return Err("cancelled".into());
        }
        let tx = self
            .download
            .as_ref()
            .ok_or("streaming download is not available")?;
        http_call_stream(request, &self.cancel, tx, part)
    }
}

struct CookieList {
    text: String,
}

unsafe extern "C" fn on_http_event(event: *mut esp_http_client_event_t) -> sys::esp_err_t {
    if event.is_null() {
        return ESP_OK;
    }
    let event = unsafe { &*event };
    if event.event_id == esp_http_client_event_id_t_HTTP_EVENT_ON_HEADER
        && !event.user_data.is_null()
    {
        let key = c_string(event.header_key);
        if key.eq_ignore_ascii_case("set-cookie") {
            let list = unsafe { &mut *(event.user_data as *mut CookieList) };
            session::append_set_cookie(
                &mut list.text,
                &c_string(event.header_value),
                MAX_SET_COOKIE_BYTES,
            );
        }
    }
    ESP_OK
}

struct HttpClient {
    raw: esp_http_client_handle_t,
    closed: bool,
}

impl HttpClient {
    fn from_raw(raw: esp_http_client_handle_t) -> Self {
        Self { raw, closed: false }
    }

    /// `esp_http_client_close` on this thread. Drop still calls `cleanup`.
    /// The main task never holds this handle.
    fn close_on_worker(&mut self) {
        if self.raw.is_null() || self.closed {
            return;
        }
        unsafe {
            let _ = esp_http_client_close(self.raw);
        }
        self.closed = true;
    }
}

impl Drop for HttpClient {
    fn drop(&mut self) {
        if self.raw.is_null() {
            return;
        }
        if !self.closed {
            unsafe {
                let _ = esp_http_client_close(self.raw);
            }
        }
        unsafe {
            let _ = esp_http_client_cleanup(self.raw);
        }
        self.raw = core::ptr::null_mut();
    }
}

fn arm_body_timeout(raw: esp_http_client_handle_t) {
    unsafe {
        let _ = esp_http_client_set_timeout_ms(raw, HTTP_READ_TIMEOUT_MS);
    }
}

fn read_body_chunk(
    client: &mut HttpClient,
    cancel: &AtomicBool,
    buf: &mut [u8],
) -> Result<(client::BodyRead, i32), String> {
    client::stop_if_cancelled(cancel.load(Ordering::Relaxed), &mut || {
        client.close_on_worker();
    })?;
    let read = unsafe {
        esp_http_client_read(
            client.raw,
            buf.as_mut_ptr() as *mut core::ffi::c_char,
            buf.len() as i32,
        )
    };
    let step = client::worker_read(
        cancel.load(Ordering::Relaxed),
        read,
        sys::ESP_ERR_HTTP_EAGAIN,
        &mut || client.close_on_worker(),
    )?;
    Ok((step, read))
}

fn http_call(request: &Request, cancel: &AtomicBool) -> Result<Response, String> {
    let url = CString::new(request.url.as_str()).map_err(|_| "URL is not a C string")?;
    let mut cookies = CookieList {
        text: String::new(),
    };
    let mut config = esp_http_client_config_t::default();
    config.url = url.as_ptr();
    config.timeout_ms = (HTTP_TIMEOUT_SECS * 1000) as i32;
    config.buffer_size = HTTP_IO_BUFFER_BYTES as _;
    config.buffer_size_tx = HTTP_IO_BUFFER_BYTES as _;
    config.event_handler = Some(on_http_event);
    config.user_data = &mut cookies as *mut CookieList as *mut core::ffi::c_void;
    config.crt_bundle_attach = Some(sys::esp_crt_bundle_attach);
    let raw = unsafe { esp_http_client_init(&config) };
    if raw.is_null() {
        return Err("HTTP connection init failed".into());
    }
    let mut client = HttpClient::from_raw(raw);
    let method = if request.method == "POST" {
        esp_http_client_method_t_HTTP_METHOD_POST
    } else {
        esp_http_client_method_t_HTTP_METHOD_GET
    };
    if unsafe { esp_http_client_set_method(client.raw, method) } != ESP_OK {
        return Err("HTTP method setup failed".into());
    }
    let mut owned_headers = Vec::new();
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let c_name = CString::new(name.as_str()).map_err(|_| "header name is not a C string")?;
        let c_value = CString::new(value.as_str()).map_err(|_| "header value is not a C string")?;
        if unsafe { esp_http_client_set_header(client.raw, c_name.as_ptr(), c_value.as_ptr()) }
            != ESP_OK
        {
            return Err("HTTP header setup failed".into());
        }
        owned_headers.push((c_name, c_value));
    }
    let write_len = request.body.as_ref().map(String::len).unwrap_or(0) as i32;
    if unsafe { esp_http_client_open(client.raw, write_len) } != ESP_OK {
        return Err("HTTP request failed".into());
    }
    if let Some(body) = request.body.as_deref() {
        client::stop_if_cancelled(cancel.load(Ordering::Relaxed), &mut || {
            client.close_on_worker();
        })?;
        let wrote = unsafe {
            esp_http_client_write(
                client.raw,
                body.as_ptr() as *const core::ffi::c_char,
                write_len,
            )
        };
        if wrote != write_len {
            return Err("HTTP request write failed".into());
        }
    }
    let fetched = unsafe { esp_http_client_fetch_headers(client.raw) };
    if fetched < 0 {
        return Err("HTTP response headers failed".into());
    }
    let status = unsafe { esp_http_client_get_status_code(client.raw) } as u16;
    let content_length = unsafe { esp_http_client_get_content_length(client.raw) };
    let declared = if content_length >= 0 {
        Some(content_length as usize)
    } else {
        None
    };
    if declared.is_some_and(|len| len > request.max_bytes) {
        return Err("response exceeds size limit".into());
    }
    let mut body = BoundedBody::new(declared, request.max_bytes)?;
    let mut chunk = [0_u8; 2048];
    arm_body_timeout(client.raw);
    loop {
        let (step, read) = read_body_chunk(&mut client, cancel, &mut chunk)?;
        match step {
            client::BodyRead::Timeout => continue,
            client::BodyRead::Ended => {
                finish_read(&client, cancel, content_length, body.len())?;
                break;
            }
            client::BodyRead::Bytes => body.push(&chunk[..read as usize])?,
        }
    }
    log::info!(
        "rustmix-wave=weread-http method={} status={} bytes={} set-cookie-bytes={}",
        request.method,
        status,
        body.len(),
        cookies.text.len()
    );
    Ok(Response {
        status,
        body: body.into_vec(),
        set_cookie: cookies.text,
        content_length: declared,
    })
}

fn http_call_stream(
    request: &Request,
    cancel: &AtomicBool,
    tx: &mpsc::SyncSender<DownloadEvent>,
    part: &'static str,
) -> Result<client::StreamedPart, String> {
    let url = CString::new(request.url.as_str()).map_err(|_| "URL is not a C string")?;
    let mut cookies = CookieList {
        text: String::new(),
    };
    let mut config = esp_http_client_config_t::default();
    config.url = url.as_ptr();
    config.timeout_ms = (HTTP_TIMEOUT_SECS * 1000) as i32;
    config.buffer_size = HTTP_IO_BUFFER_BYTES as _;
    config.buffer_size_tx = HTTP_IO_BUFFER_BYTES as _;
    config.event_handler = Some(on_http_event);
    config.user_data = &mut cookies as *mut CookieList as *mut core::ffi::c_void;
    config.crt_bundle_attach = Some(sys::esp_crt_bundle_attach);
    let raw = unsafe { esp_http_client_init(&config) };
    if raw.is_null() {
        return Err("HTTP connection init failed".into());
    }
    let mut client = HttpClient::from_raw(raw);
    let method = if request.method == "POST" {
        esp_http_client_method_t_HTTP_METHOD_POST
    } else {
        esp_http_client_method_t_HTTP_METHOD_GET
    };
    if unsafe { esp_http_client_set_method(client.raw, method) } != ESP_OK {
        return Err("HTTP method setup failed".into());
    }
    let mut owned_headers = Vec::new();
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let c_name = CString::new(name.as_str()).map_err(|_| "header name is not a C string")?;
        let c_value = CString::new(value.as_str()).map_err(|_| "header value is not a C string")?;
        if unsafe { esp_http_client_set_header(client.raw, c_name.as_ptr(), c_value.as_ptr()) }
            != ESP_OK
        {
            return Err("HTTP header setup failed".into());
        }
        owned_headers.push((c_name, c_value));
    }
    let write_len = request.body.as_ref().map(String::len).unwrap_or(0) as i32;
    if unsafe { esp_http_client_open(client.raw, write_len) } != ESP_OK {
        return Err("HTTP request failed".into());
    }
    if let Some(body) = request.body.as_deref() {
        client::stop_if_cancelled(cancel.load(Ordering::Relaxed), &mut || {
            client.close_on_worker();
        })?;
        let wrote = unsafe {
            esp_http_client_write(
                client.raw,
                body.as_ptr() as *const core::ffi::c_char,
                write_len,
            )
        };
        if wrote != write_len {
            return Err("HTTP request write failed".into());
        }
    }
    let fetched = unsafe { esp_http_client_fetch_headers(client.raw) };
    if fetched < 0 {
        return Err("HTTP response headers failed".into());
    }
    let status = unsafe { esp_http_client_get_status_code(client.raw) } as u16;
    let content_length = unsafe { esp_http_client_get_content_length(client.raw) };
    if content_length >= 0 && content_length as usize > request.max_bytes {
        return Err("response exceeds size limit".into());
    }
    // One reusable read buffer. Chunks forwarded to the main task are the only
    // body copies; the shard is not assembled on this thread.
    let mut chunk = vec![0u8; DOWNLOAD_CHUNK_BYTES];
    let mut prefix = Vec::new();
    let mut total = 0usize;
    emit_download(tx, DownloadEvent::BeginPart(part))?;
    arm_body_timeout(client.raw);
    loop {
        let (step, read) = read_body_chunk(&mut client, cancel, &mut chunk)?;
        match step {
            client::BodyRead::Timeout => continue,
            client::BodyRead::Ended => {
                finish_read(&client, cancel, content_length, total)?;
                break;
            }
            client::BodyRead::Bytes => {
                let slice = &chunk[..read as usize];
                if prefix.len() < DOWNLOAD_CLASSIFY_BYTES {
                    let room = DOWNLOAD_CLASSIFY_BYTES - prefix.len();
                    prefix.extend_from_slice(&slice[..slice.len().min(room)]);
                }
                total = total.saturating_add(slice.len());
                if total > request.max_bytes {
                    return Err("response exceeds size limit".into());
                }
                emit_download(tx, DownloadEvent::Chunk(slice.to_vec()))?;
            }
        }
    }
    let chunked = unsafe { esp_http_client_is_chunked_response(client.raw) };
    let terminal_chunk = unsafe { esp_http_client_is_complete_data_received(client.raw) };
    emit_download(tx, DownloadEvent::EndPart)?;
    log::info!(
        "rustmix-wave=weread-http method={} status={} bytes={} set-cookie-bytes={} stream=chunk",
        request.method,
        status,
        total,
        cookies.text.len()
    );
    Ok(client::StreamedPart {
        status,
        set_cookie: cookies.text,
        prefix,
        total,
        content_length,
        chunked,
        terminal_chunk,
    })
}

fn finish_read(
    client: &HttpClient,
    cancel: &AtomicBool,
    content_length: i64,
    bytes_read: usize,
) -> Result<(), String> {
    let chunked = unsafe { esp_http_client_is_chunked_response(client.raw) };
    let terminal_chunk = unsafe { esp_http_client_is_complete_data_received(client.raw) };
    if let Some(error) = body::stopped_transfer_error(
        cancel.load(Ordering::Relaxed),
        content_length,
        bytes_read,
        chunked,
        terminal_chunk,
    ) {
        return Err(error.into());
    }
    Ok(())
}

fn emit_download(tx: &mpsc::SyncSender<DownloadEvent>, event: DownloadEvent) -> Result<(), String> {
    tx.send(event).map_err(|_| "download cancelled".to_string())
}

fn c_string(ptr: *mut core::ffi::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}
