//! One bounded WeRead HTTPS job on a short-lived worker.
//!
//! Response bodies are allocated only after `Content-Length` is known to fit
//! the job cap. Allocations above the internal heap threshold land in PSRAM.

use std::{thread, time::Duration};

use embedded_svc::{
    http::{client::Client as HttpClient, Headers, Method},
    io::Write,
};
use esp_idf_svc::{
    http::client::{Configuration as HttpConfiguration, EspHttpConnection},
    sys,
};

use crate::{
    runtime_worker::{run_named_worker, NamedWorkerError},
    weread::{
        body::BoundedBody,
        client::{self, JobError, Report, Request, Response, Transport, Work},
        limits::{HTTP_TIMEOUT_SECS, MIN_REQUEST_GAP_MS},
    },
};

pub const WEREAD_HTTP_WORKER_STACK_BYTES: usize = 96 * 1024;

pub fn run_work(work: Work, unix: Option<u64>) -> Report {
    let generation = work.generation;
    let job = work.job.clone();
    let session = work.session.clone();
    let seed = random_seed();
    match run_named_worker("weread-http", WEREAD_HTTP_WORKER_STACK_BYTES, move || {
        let mut transport = EspTransport { gap: true };
        Ok::<Report, String>(client::perform_with_seed(&mut transport, work, unix, seed))
    }) {
        Ok(report) => report,
        Err(NamedWorkerError::Operation(message)) => Report {
            generation,
            job,
            session,
            result: Err(JobError::Message(message)),
        },
        Err(error) => Report {
            generation,
            job,
            session,
            result: Err(JobError::Message(format!("WeRead worker failed: {error}"))),
        },
    }
}

fn random_seed() -> u64 {
    let high = unsafe { sys::esp_random() } as u64;
    let low = unsafe { sys::esp_random() } as u64;
    (high << 32) | low
}

struct EspTransport {
    gap: bool,
}

impl Transport for EspTransport {
    fn idle(&mut self) {
        if self.gap {
            thread::sleep(Duration::from_millis(MIN_REQUEST_GAP_MS));
        }
    }

    fn call(&mut self, request: &Request) -> Result<Response, String> {
        self.gap = true;
        let method = match request.method {
            "POST" => Method::Post,
            _ => Method::Get,
        };
        let http_config = HttpConfiguration {
            crt_bundle_attach: Some(sys::esp_crt_bundle_attach),
            timeout: Some(Duration::from_secs(HTTP_TIMEOUT_SECS)),
            buffer_size: Some(2048),
            buffer_size_tx: Some(2048),
            ..Default::default()
        };
        let connection = EspHttpConnection::new(&http_config)
            .map_err(|error| format!("HTTP connection init failed: {error}"))?;
        let mut client = HttpClient::wrap(connection);
        let mut headers: Vec<(&str, &str)> = request
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        // A declared length keeps ESP-IDF off chunked request bodies.
        let content_length = request.body.as_ref().map(|body| body.len().to_string());
        if let Some(length) = content_length.as_deref() {
            headers.push(("Content-Length", length));
        }
        let mut http_request = client
            .request(method, &request.url, &headers)
            .map_err(|error| format!("HTTP request setup failed: {error}"))?;
        if let Some(body) = request.body.as_deref() {
            http_request
                .write_all(body.as_bytes())
                .map_err(|error| format!("HTTP request write failed: {error}"))?;
        }
        let mut response = http_request
            .submit()
            .map_err(|error| format!("HTTP request failed: {error}"))?;
        let status = response.status();
        let declared_len = response.content_len();
        if declared_len.is_some_and(|len| len > request.max_bytes as u64) {
            return Err("response exceeds size limit".into());
        }
        let declared = declared_len.map(|len| len as usize);
        let mut set_cookie = String::new();
        for name in ["Set-Cookie", "set-cookie"] {
            if let Some(value) = response.header(name) {
                if !set_cookie.is_empty() {
                    set_cookie.push_str("; ");
                }
                set_cookie.push_str(value);
            }
        }
        let mut body = BoundedBody::new(declared, request.max_bytes)?;
        let mut chunk = [0_u8; 2048];
        loop {
            let read = response
                .read(&mut chunk)
                .map_err(|error| format!("HTTP response read failed: {error}"))?;
            if read == 0 {
                break;
            }
            body.push(&chunk[..read])?;
        }
        log::info!(
            "rustmix-wave=weread-http method={} status={} bytes={}",
            request.method,
            status,
            body.len()
        );
        Ok(Response {
            status,
            body: body.into_vec(),
            set_cookie,
            content_length: declared,
        })
    }
}
