//! One bounded WeRead HTTPS job on a short-lived worker.
//!
//! The main loop polls the worker. Response bodies are allocated only after
//! `Content-Length` is known to fit the job cap. Allocations above the internal
//! heap threshold land in PSRAM. Every `Set-Cookie` is kept; the ESP-IDF Rust
//! client stores headers in a map and would drop all but the last.

use std::{
    ffi::{CStr, CString},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use esp_idf_svc::sys::{
    self, esp_http_client_cleanup, esp_http_client_close, esp_http_client_config_t,
    esp_http_client_event_id_t_HTTP_EVENT_ON_HEADER, esp_http_client_event_t,
    esp_http_client_fetch_headers, esp_http_client_get_content_length,
    esp_http_client_get_status_code, esp_http_client_handle_t, esp_http_client_init,
    esp_http_client_method_t_HTTP_METHOD_GET, esp_http_client_method_t_HTTP_METHOD_POST,
    esp_http_client_open, esp_http_client_read, esp_http_client_set_header,
    esp_http_client_set_method, esp_http_client_write, ESP_OK,
};

use crate::{
    reader::ReaderLayout,
    runtime_worker::NamedWorkerHandle,
    weread::{
        body::BoundedBody,
        client::{self, Job, JobError, Report, Request, Response, Transport, Work},
        limits::{HTTP_TIMEOUT_SECS, MIN_REQUEST_GAP_MS},
        session,
        ui::{self, ServiceOutcome, WereadUi},
    },
};

pub const WEREAD_HTTP_WORKER_STACK_BYTES: usize = 96 * 1024;
const MAX_SET_COOKIE_BYTES: usize = 4 * 1024;

pub struct HttpJobs {
    inflight: Option<Inflight>,
}

struct Inflight {
    handle: NamedWorkerHandle<Report, String>,
    cancel: Arc<AtomicBool>,
    generation: u64,
    job: Job,
    session: session::Session,
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
            |job| job.cancel.store(true, Ordering::Relaxed),
        )
    }
}

fn poll_report(job: &mut Inflight) -> Option<Report> {
    let joined = job.handle.try_join()?;
    Some(match joined {
        Ok(report) => report,
        Err(error) => Report {
            generation: job.generation,
            job: job.job.clone(),
            session: job.session.clone(),
            result: Err(JobError::Message(format!("WeRead worker failed: {error}"))),
        },
    })
}

fn spawn_work(work: Work, unix: Option<u64>, cancel: Arc<AtomicBool>) -> Result<Inflight, Report> {
    let generation = work.generation;
    let job = work.job.clone();
    let session = work.session.clone();
    let flag = Arc::clone(&cancel);
    let seed = random_seed();
    match NamedWorkerHandle::spawn("weread-http", WEREAD_HTTP_WORKER_STACK_BYTES, move || {
        let mut transport = EspTransport {
            gap: true,
            cancel: flag,
        };
        Ok::<Report, String>(client::perform_with_seed(&mut transport, work, unix, seed))
    }) {
        Ok(handle) => Ok(Inflight {
            handle,
            cancel,
            generation,
            job,
            session,
        }),
        Err(error) => Err(Report {
            generation,
            job,
            session,
            result: Err(JobError::Message(format!(
                "WeRead worker failed to start: {error}"
            ))),
        }),
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

struct HttpClient(esp_http_client_handle_t);

impl Drop for HttpClient {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        unsafe {
            let _ = esp_http_client_close(self.0);
            let _ = esp_http_client_cleanup(self.0);
        }
        self.0 = core::ptr::null_mut();
    }
}

fn http_call(request: &Request, cancel: &AtomicBool) -> Result<Response, String> {
    let url = CString::new(request.url.as_str()).map_err(|_| "URL is not a C string")?;
    let mut cookies = CookieList {
        text: String::new(),
    };
    let mut config = esp_http_client_config_t::default();
    config.url = url.as_ptr();
    config.timeout_ms = (HTTP_TIMEOUT_SECS * 1000) as i32;
    config.buffer_size = 2048;
    config.buffer_size_tx = 2048;
    config.event_handler = Some(on_http_event);
    config.user_data = &mut cookies as *mut CookieList as *mut core::ffi::c_void;
    config.crt_bundle_attach = Some(sys::esp_crt_bundle_attach);
    let raw = unsafe { esp_http_client_init(&config) };
    if raw.is_null() {
        return Err("HTTP connection init failed".into());
    }
    let client = HttpClient(raw);
    let method = if request.method == "POST" {
        esp_http_client_method_t_HTTP_METHOD_POST
    } else {
        esp_http_client_method_t_HTTP_METHOD_GET
    };
    if unsafe { esp_http_client_set_method(client.0, method) } != ESP_OK {
        return Err("HTTP method setup failed".into());
    }
    let mut owned_headers = Vec::new();
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let c_name = CString::new(name.as_str()).map_err(|_| "header name is not a C string")?;
        let c_value = CString::new(value.as_str()).map_err(|_| "header value is not a C string")?;
        if unsafe { esp_http_client_set_header(client.0, c_name.as_ptr(), c_value.as_ptr()) }
            != ESP_OK
        {
            return Err("HTTP header setup failed".into());
        }
        owned_headers.push((c_name, c_value));
    }
    let write_len = request.body.as_ref().map(String::len).unwrap_or(0) as i32;
    if unsafe { esp_http_client_open(client.0, write_len) } != ESP_OK {
        return Err("HTTP request failed".into());
    }
    if let Some(body) = request.body.as_deref() {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let wrote = unsafe {
            esp_http_client_write(
                client.0,
                body.as_ptr() as *const core::ffi::c_char,
                write_len,
            )
        };
        if wrote != write_len {
            return Err("HTTP request write failed".into());
        }
    }
    let fetched = unsafe { esp_http_client_fetch_headers(client.0) };
    if fetched < 0 {
        return Err("HTTP response headers failed".into());
    }
    let status = unsafe { esp_http_client_get_status_code(client.0) } as u16;
    let content_length = unsafe { esp_http_client_get_content_length(client.0) };
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
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let read = unsafe {
            esp_http_client_read(
                client.0,
                chunk.as_mut_ptr() as *mut core::ffi::c_char,
                chunk.len() as i32,
            )
        };
        if read < 0 {
            return Err("HTTP response read failed".into());
        }
        if read == 0 {
            break;
        }
        body.push(&chunk[..read as usize])?;
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

fn c_string(ptr: *mut core::ffi::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}
