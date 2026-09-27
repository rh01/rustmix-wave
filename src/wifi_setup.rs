//! SoftAP captive Wi-Fi provisioning at `http://192.168.4.1`.
//!
//! Station credentials still load first from SD `/RUSTMIX/WIFI.TXT`. SoftAP is
//! the on-device path when that file is missing, STA association fails, or
//! Settings > Network > Configure Wi-Fi is selected. Saving writes NVS and,
//! when the card is present, writes `WIFI.TXT` back. The LAN transfer portal
//! stays a separate STA-only service.
//!
//! The open AP has the same 10-minute budget as the transfer portal, both as an
//! idle timeout and as a total timeout. Every exit stops the HTTP server and
//! the SoftAP radio together.

use crate::network_config::{NetworkConfig, DEFAULT_NTP_SERVER, DEFAULT_TIMEZONE};

/// Open setup network shown on the e-paper while provisioning.
pub const WIFI_SETUP_AP_SSID: &str = "Rustmix-Setup";
/// ESP-IDF SoftAP default IPv4. Phones open this URL after joining the AP.
pub const WIFI_SETUP_AP_IP: &str = "192.168.4.1";
/// Captive HTTP port. Mutually exclusive with the STA transfer portal.
pub const WIFI_SETUP_HTTP_PORT: u16 = 80;
/// Dedicated HTTP task stack for the short-lived setup portal.
pub const WIFI_SETUP_SERVER_STACK_BYTES: usize = 20 * 1024;
/// Bound scan rows returned to the phone UI.
pub const WIFI_SETUP_MAX_SCAN: usize = 24;
/// POST `/save` body cap. A full buffer with more bytes still unread is rejected.
pub const WIFI_SETUP_MAX_BODY_BYTES: usize = 256;
/// HTTP status when that cap is exceeded. The truncated body is not saved.
pub const WIFI_SETUP_BODY_TOO_LARGE_STATUS: u16 = 413;
/// `/scan` response type. SSIDs are JSON-escaped, including `<`.
pub const WIFI_SETUP_SCAN_CONTENT_TYPE: &str = "application/json";
/// Idle timeout with no HTTP traffic. Matches the transfer portal.
pub const WIFI_SETUP_IDLE_TIMEOUT_SECONDS: u64 = 10 * 60;
/// Hard cap from the moment the setup AP starts, even if the phone keeps polling.
pub const WIFI_SETUP_TOTAL_TIMEOUT_SECONDS: u64 = 10 * 60;
/// Shown on e-paper after the setup AP stops on its own.
pub const WIFI_SETUP_RESTART_HINT: &str = "Settings > Network > Configure Wi-Fi";

/// Why a running setup session must stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WifiSetupTimeoutKind {
    Idle,
    Total,
}

impl WifiSetupTimeoutKind {
    #[must_use]
    pub const fn log_reason(self) -> &'static str {
        match self {
            Self::Idle => "idle-timeout",
            Self::Total => "total-timeout",
        }
    }
}

/// Host-testable idle and total timeout clock. Times are milliseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WifiSetupSessionClock {
    started_ms: u64,
    last_activity_ms: u64,
}

impl WifiSetupSessionClock {
    #[must_use]
    pub const fn start(now_ms: u64) -> Self {
        Self {
            started_ms: now_ms,
            last_activity_ms: now_ms,
        }
    }

    pub fn note_activity(&mut self, now_ms: u64) {
        if now_ms > self.last_activity_ms {
            self.last_activity_ms = now_ms;
        }
    }

    #[must_use]
    pub fn poll(self, now_ms: u64) -> Option<WifiSetupTimeoutKind> {
        Self::from_elapsed(
            now_ms.saturating_sub(self.last_activity_ms),
            now_ms.saturating_sub(self.started_ms),
        )
    }

    /// `idle_ms` is time since the last HTTP request. `total_ms` is time since start.
    #[must_use]
    pub fn from_elapsed(idle_ms: u64, total_ms: u64) -> Option<WifiSetupTimeoutKind> {
        let idle_due = idle_ms >= timeout_millis(WIFI_SETUP_IDLE_TIMEOUT_SECONDS);
        let total_due = total_ms >= timeout_millis(WIFI_SETUP_TOTAL_TIMEOUT_SECONDS);
        match (idle_due, total_due) {
            (false, false) => None,
            (false, true) => Some(WifiSetupTimeoutKind::Total),
            (true, false) => Some(WifiSetupTimeoutKind::Idle),
            (true, true) if idle_ms < total_ms => Some(WifiSetupTimeoutKind::Total),
            (true, true) => Some(WifiSetupTimeoutKind::Idle),
        }
    }
}

const fn timeout_millis(seconds: u64) -> u64 {
    seconds.saturating_mul(1_000)
}

/// Every way the setup portal leaves the running state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WifiSetupExit {
    SettingsStop,
    HttpServerFailed,
    SoftApFailed,
    SavedCredentials,
    IdleTimeout,
    TotalTimeout,
    SleepEntry,
    Restart,
    TransferPortal,
}

impl WifiSetupExit {
    pub const ALL: [Self; 9] = [
        Self::SettingsStop,
        Self::HttpServerFailed,
        Self::SoftApFailed,
        Self::SavedCredentials,
        Self::IdleTimeout,
        Self::TotalTimeout,
        Self::SleepEntry,
        Self::Restart,
        Self::TransferPortal,
    ];

    /// One teardown plan: HTTP and the AP radio both stop, on every path.
    #[must_use]
    pub const fn teardown(self) -> WifiSetupTeardown {
        let _ = self;
        WifiSetupTeardown {
            stop_http: true,
            stop_ap_radio: true,
        }
    }

    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::SettingsStop => "settings-stop",
            Self::HttpServerFailed => "http-start-failed",
            Self::SoftApFailed => "softap-start-failed",
            Self::SavedCredentials => "saved-credentials",
            Self::IdleTimeout => "idle-timeout",
            Self::TotalTimeout => "total-timeout",
            Self::SleepEntry => "sleep-entry",
            Self::Restart => "restart",
            Self::TransferPortal => "transfer-portal",
        }
    }

    #[must_use]
    pub const fn shows_restart_hint(self) -> bool {
        matches!(self, Self::IdleTimeout | Self::TotalTimeout)
    }
}

/// Resources a setup exit must release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WifiSetupTeardown {
    pub stop_http: bool,
    pub stop_ap_radio: bool,
}

/// HTTP server and SoftAP radio ownership, used to prove teardown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetupResources {
    pub http: bool,
    pub ap_radio: bool,
}

impl SetupResources {
    #[must_use]
    pub const fn apply_exit(self, exit: WifiSetupExit) -> Self {
        let plan = exit.teardown();
        Self {
            http: self.http && !plan.stop_http,
            ap_radio: self.ap_radio && !plan.stop_ap_radio,
        }
    }
}

/// What to do with a POST `/save` body after the bounded read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupPostDecision {
    Save(NetworkConfig),
    TooLarge,
    Invalid(String),
}

impl SetupPostDecision {
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        match self {
            Self::Save(_) => 200,
            Self::TooLarge => WIFI_SETUP_BODY_TOO_LARGE_STATUS,
            Self::Invalid(_) => 400,
        }
    }
}

/// Stack buffer for POST `/save`. Overflow is remembered so a truncated form is never parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupBodyAccumulator {
    buf: [u8; WIFI_SETUP_MAX_BODY_BYTES],
    filled: usize,
    overflow: bool,
}

impl SetupBodyAccumulator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; WIFI_SETUP_MAX_BODY_BYTES],
            filled: 0,
            overflow: false,
        }
    }

    /// Push the next socket read. Returns true once the body exceeds the cap.
    pub fn push(&mut self, chunk: &[u8]) -> bool {
        if chunk.is_empty() {
            return self.overflow;
        }
        if self.overflow {
            return true;
        }
        let room = WIFI_SETUP_MAX_BODY_BYTES.saturating_sub(self.filled);
        if chunk.len() > room {
            if room > 0 {
                self.buf[self.filled..self.filled + room].copy_from_slice(&chunk[..room]);
                self.filled += room;
            }
            self.overflow = true;
            return true;
        }
        self.buf[self.filled..self.filled + chunk.len()].copy_from_slice(chunk);
        self.filled += chunk.len();
        false
    }

    #[must_use]
    pub fn decide(&self) -> SetupPostDecision {
        if self.overflow {
            return SetupPostDecision::TooLarge;
        }
        let bytes = &self.buf[..self.filled];
        let text = match core::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return SetupPostDecision::Invalid("form is not UTF-8".into()),
        };
        match parse_setup_form(text) {
            Ok(config) => SetupPostDecision::Save(config),
            Err(error) => SetupPostDecision::Invalid(format!("{error:#}")),
        }
    }
}

impl Default for SetupBodyAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// User request handed from Settings into `main.rs`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WifiSetupUiRequest {
    Start,
    Stop,
}

/// Compact UI-facing SoftAP lifecycle. Never contains a Wi-Fi password.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WifiSetupState {
    #[default]
    Off,
    Starting,
    Ready,
    Saving,
    Failed,
    TimedOut,
}

impl WifiSetupState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Starting => "STARTING",
            Self::Ready => "READY",
            Self::Saving => "SAVING",
            Self::Failed => "FAILED",
            Self::TimedOut => "TIMEOUT",
        }
    }
}

/// One scanned BSS shown in the phone UI (and counted on e-paper).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannedNetwork {
    pub ssid: String,
    pub rssi_dbm: i32,
    pub open: bool,
}

/// Password-free snapshot rendered on the e-paper setup screen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WifiSetupSnapshot {
    pub state: WifiSetupState,
    pub ap_ssid: String,
    pub url: String,
    pub network_count: usize,
    pub last_action: String,
    pub error: Option<String>,
}

impl Default for WifiSetupSnapshot {
    fn default() -> Self {
        Self {
            state: WifiSetupState::Off,
            ap_ssid: WIFI_SETUP_AP_SSID.into(),
            url: setup_url().into(),
            network_count: 0,
            last_action: "SoftAP setup is off".into(),
            error: None,
        }
    }
}

impl WifiSetupSnapshot {
    #[must_use]
    pub fn starting() -> Self {
        Self {
            state: WifiSetupState::Starting,
            last_action: "Starting setup network".into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn ready(network_count: usize) -> Self {
        Self {
            state: WifiSetupState::Ready,
            network_count,
            last_action: "Join Rustmix-Setup, then open 192.168.4.1".into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            state: WifiSetupState::Failed,
            last_action: "SoftAP setup failed".into(),
            error: Some(error.into()),
            ..Self::default()
        }
    }

    /// Setup AP stopped on its own. E-paper tells the user how to start it again.
    #[must_use]
    pub fn timed_out(kind: WifiSetupTimeoutKind) -> Self {
        let last_action = match kind {
            WifiSetupTimeoutKind::Idle => "Stopped after 10 idle minutes",
            WifiSetupTimeoutKind::Total => "Stopped after 10 minutes",
        };
        Self {
            state: WifiSetupState::TimedOut,
            last_action: last_action.into(),
            error: Some(format!("Restart: {WIFI_SETUP_RESTART_HINT}")),
            network_count: 0,
            ..Self::default()
        }
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self.state,
            WifiSetupState::Starting | WifiSetupState::Ready | WifiSetupState::Saving
        )
    }

    #[must_use]
    pub fn url_label(&self) -> &str {
        self.url.as_str()
    }
}

/// Canonical captive-portal URL shown on e-paper and in the HTML.
#[must_use]
pub const fn setup_url() -> &'static str {
    "http://192.168.4.1/"
}

/// Deduplicate scan rows by SSID, keeping the strongest RSSI, strongest first.
#[must_use]
pub fn collapse_scan_results(mut networks: Vec<ScannedNetwork>) -> Vec<ScannedNetwork> {
    networks.retain(|network| !network.ssid.is_empty() && network.ssid != WIFI_SETUP_AP_SSID);
    networks.sort_by(|left, right| {
        right
            .rssi_dbm
            .cmp(&left.rssi_dbm)
            .then_with(|| left.ssid.cmp(&right.ssid))
    });
    let mut unique = Vec::new();
    for network in networks {
        if unique
            .iter()
            .any(|existing: &ScannedNetwork| existing.ssid == network.ssid)
        {
            continue;
        }
        unique.push(network);
        if unique.len() == WIFI_SETUP_MAX_SCAN {
            break;
        }
    }
    unique
}

/// Phone-facing JSON array. SSIDs are escaped; passwords are never included.
#[must_use]
pub fn scanned_networks_json(networks: &[ScannedNetwork]) -> String {
    let mut json = String::from("[");
    for (index, network) in networks.iter().enumerate() {
        if index > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            r#"{{"ssid":"{}","rssi":{},"open":{}}}"#,
            json_escape(&network.ssid),
            network.rssi_dbm,
            if network.open { "true" } else { "false" }
        ));
    }
    json.push(']');
    json
}

/// `/scan` payload. The handler sends `content_type` with the JSON body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanHttpBody {
    pub content_type: &'static str,
    pub json: String,
}

#[must_use]
pub fn scan_http_body(networks: &[ScannedNetwork]) -> ScanHttpBody {
    ScanHttpBody {
        content_type: WIFI_SETUP_SCAN_CONTENT_TYPE,
        json: scanned_networks_json(networks),
    }
}

/// Parse `application/x-www-form-urlencoded` from POST `/save`.
pub fn parse_setup_form(body: &str) -> anyhow::Result<NetworkConfig> {
    let mut ssid = None;
    let mut password = String::new();
    let mut timezone = None;
    let mut ntp_server = None;
    for part in body.split('&') {
        if part.is_empty() {
            continue;
        }
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        let key = percent_decode(key).map_err(anyhow::Error::msg)?;
        let value = percent_decode(value).map_err(anyhow::Error::msg)?;
        match key.as_str() {
            "ssid" => ssid = Some(value),
            "password" => password = value,
            "timezone" => timezone = Some(value),
            "ntp_server" => ntp_server = Some(value),
            _ => {}
        }
    }
    let ssid = ssid.ok_or_else(|| anyhow::anyhow!("ssid is required"))?;
    if ssid.chars().any(|ch| ch.is_control()) || password.chars().any(|ch| ch.is_control()) {
        anyhow::bail!("ssid and password must not contain control characters");
    }
    let rendered = format!(
        "ssid={ssid}\npassword={password}\ntimezone={}\nntp_server={}\n",
        timezone.as_deref().unwrap_or(DEFAULT_TIMEZONE),
        ntp_server.as_deref().unwrap_or(DEFAULT_NTP_SERVER)
    );
    NetworkConfig::parse(&rendered)
}

/// Compact mobile HTML served at `/` and captive-portal probe URLs.
#[must_use]
pub const fn setup_html() -> &'static str {
    include_str!("wifi_setup_portal.html")
}

#[allow(dead_code)]
const SAVED_HTML: &str = include_str!("wifi_setup_saved.html");

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '<' => escaped.push_str("\\u003c"),
            '>' => escaped.push_str("\\u003e"),
            '&' => escaped.push_str("\\u0026"),
            other if other.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", u32::from(other)));
            }
            other => escaped.push(other),
        }
    }
    escaped
}

fn percent_decode(value: &str) -> Result<String, &'static str> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = hex(bytes[index + 1]).ok_or("invalid percent escape")?;
                let low = hex(bytes[index + 2]).ok_or("invalid percent escape")?;
                output.push((high << 4) | low);
                index += 3;
            }
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).map_err(|_| "form is not UTF-8")
}

const fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(target_os = "espidf")]
pub mod espidf {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use anyhow::Result;
    use embedded_svc::{http::Method, io::Write as _};
    use esp_idf_svc::http::server::{Configuration, EspHttpServer};
    use log::{info, warn};

    use super::{
        scan_http_body, setup_html, setup_url, ScannedNetwork, SetupBodyAccumulator,
        SetupPostDecision, WifiSetupSessionClock, WifiSetupSnapshot, WifiSetupState,
        WifiSetupTimeoutKind, SAVED_HTML, WIFI_SETUP_HTTP_PORT, WIFI_SETUP_SCAN_CONTENT_TYPE,
        WIFI_SETUP_SERVER_STACK_BYTES,
    };
    use crate::network_config::NetworkConfig;

    struct SharedStatus {
        snapshot: WifiSetupSnapshot,
        networks: Vec<ScannedNetwork>,
        pending: Option<NetworkConfig>,
        scan_requested: bool,
        started_at: Instant,
        last_activity_at: Instant,
    }

    impl SharedStatus {
        fn ready() -> Self {
            let now = Instant::now();
            Self {
                snapshot: WifiSetupSnapshot::ready(0),
                networks: Vec::new(),
                pending: None,
                scan_requested: true,
                started_at: now,
                last_activity_at: now,
            }
        }

        fn touch(&mut self) {
            self.last_activity_at = Instant::now();
        }
    }

    /// RAII wrapper. Dropping stops the ESP-IDF HTTP task.
    pub struct WifiSetupServer {
        _server: EspHttpServer<'static>,
        shared: Arc<Mutex<SharedStatus>>,
    }

    impl WifiSetupServer {
        pub fn start() -> Result<Self> {
            let shared = Arc::new(Mutex::new(SharedStatus::ready()));
            let mut server = EspHttpServer::new(&Configuration {
                http_port: WIFI_SETUP_HTTP_PORT,
                stack_size: WIFI_SETUP_SERVER_STACK_BYTES,
                max_open_sockets: 4,
                max_sessions: 4,
                max_uri_handlers: 12,
                session_timeout: Duration::from_secs(30),
                ..Default::default()
            })?;

            for path in [
                "/",
                "/index.html",
                "/generate_204",
                "/hotspot-detect.html",
                "/ncsi.txt",
                "/connecttest.txt",
            ] {
                let page_shared = Arc::clone(&shared);
                server.fn_handler(path, Method::Get, move |request| {
                    lock(&page_shared).touch();
                    request
                        .into_ok_response()?
                        .write_all(setup_html().as_bytes())?;
                    Ok::<(), anyhow::Error>(())
                })?;
            }

            let scan_shared = Arc::clone(&shared);
            server.fn_handler("/scan", Method::Get, move |request| {
                let mut locked = lock(&scan_shared);
                locked.touch();
                if request.uri().contains("refresh=1") {
                    locked.scan_requested = true;
                    locked.snapshot.last_action = "Scan requested".into();
                }
                let body = scan_http_body(&locked.networks);
                debug_assert_eq!(body.content_type, WIFI_SETUP_SCAN_CONTENT_TYPE);
                let payload = body.json;
                drop(locked);
                request
                    .into_response(
                        200,
                        Some("OK"),
                        &[("Content-Type", WIFI_SETUP_SCAN_CONTENT_TYPE)],
                    )?
                    .write_all(payload.as_bytes())?;
                Ok::<(), anyhow::Error>(())
            })?;

            let save_shared = Arc::clone(&shared);
            server.fn_handler("/save", Method::Post, move |mut request| {
                lock(&save_shared).touch();
                let mut body = SetupBodyAccumulator::new();
                let mut chunk = [0_u8; 64];
                loop {
                    let read = request.read(&mut chunk)?;
                    if read == 0 {
                        break;
                    }
                    if body.push(&chunk[..read]) {
                        break;
                    }
                }
                match body.decide() {
                    SetupPostDecision::Save(config) => {
                        {
                            let mut locked = lock(&save_shared);
                            locked.snapshot.state = WifiSetupState::Saving;
                            locked.snapshot.last_action = format!("Saving {}", config.ssid);
                            locked.snapshot.error = None;
                            locked.pending = Some(config);
                        }
                        request
                            .into_ok_response()?
                            .write_all(SAVED_HTML.as_bytes())?;
                    }
                    SetupPostDecision::TooLarge => {
                        warn!(
                            "rustmix-wave=wifi-setup status=rejected http=413 reason=body-too-large"
                        );
                        request
                            .into_response(
                                SetupPostDecision::TooLarge.status_code(),
                                Some("Payload Too Large"),
                                &[("Content-Type", "text/plain; charset=utf-8")],
                            )?
                            .write_all(b"request body too large")?;
                    }
                    SetupPostDecision::Invalid(message) => {
                        warn!(
                            "rustmix-wave=wifi-setup status=rejected http=400 reason=invalid-form"
                        );
                        request
                            .into_response(
                                400,
                                Some("Bad Request"),
                                &[("Content-Type", "text/plain; charset=utf-8")],
                            )?
                            .write_all(message.as_bytes())?;
                    }
                }
                Ok::<(), anyhow::Error>(())
            })?;

            info!(
                "rustmix-wave=wifi-setup-server status=ready url={} stack-bytes={WIFI_SETUP_SERVER_STACK_BYTES}",
                setup_url()
            );
            Ok(Self {
                _server: server,
                shared,
            })
        }

        #[must_use]
        pub fn snapshot(&self) -> WifiSetupSnapshot {
            lock(&self.shared).snapshot.clone()
        }

        pub fn set_networks(&self, networks: Vec<ScannedNetwork>) {
            let mut locked = lock(&self.shared);
            let count = networks.len();
            locked.networks = networks;
            locked.snapshot.network_count = count;
            if locked.snapshot.state == WifiSetupState::Starting {
                locked.snapshot.state = WifiSetupState::Ready;
            }
            locked.snapshot.last_action = format!("{count} networks scanned");
        }

        #[must_use]
        pub fn take_scan_requested(&self) -> bool {
            let mut locked = lock(&self.shared);
            let requested = locked.scan_requested;
            locked.scan_requested = false;
            requested
        }

        #[must_use]
        pub fn take_pending(&self) -> Option<NetworkConfig> {
            lock(&self.shared).pending.take()
        }

        pub fn record_error(&self, error: impl Into<String>) {
            let mut locked = lock(&self.shared);
            locked.snapshot.state = WifiSetupState::Failed;
            locked.snapshot.error = Some(error.into());
            locked.snapshot.last_action = "Setup error".into();
        }

        /// Keep a non-fatal notice, such as an invalid `WIFI.TXT`, on the setup screen.
        pub fn set_notice(&self, notice: impl Into<String>) {
            lock(&self.shared).snapshot.error = Some(notice.into());
        }

        #[must_use]
        pub fn timeout_kind(&self) -> Option<WifiSetupTimeoutKind> {
            let locked = lock(&self.shared);
            let now = Instant::now();
            WifiSetupSessionClock::from_elapsed(
                elapsed_millis(locked.last_activity_at, now),
                elapsed_millis(locked.started_at, now),
            )
        }
    }

    impl Drop for WifiSetupServer {
        fn drop(&mut self) {
            info!("rustmix-wave=wifi-setup-server status=stopped");
        }
    }

    fn lock(shared: &Arc<Mutex<SharedStatus>>) -> std::sync::MutexGuard<'_, SharedStatus> {
        shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn elapsed_millis(start: Instant, now: Instant) -> u64 {
        now.saturating_duration_since(start).as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        collapse_scan_results, parse_setup_form, scan_http_body, scanned_networks_json, setup_html,
        setup_url, ScannedNetwork, SetupBodyAccumulator, SetupPostDecision, SetupResources,
        WifiSetupExit, WifiSetupSessionClock, WifiSetupSnapshot, WifiSetupState,
        WifiSetupTimeoutKind, WIFI_SETUP_AP_SSID, WIFI_SETUP_BODY_TOO_LARGE_STATUS,
        WIFI_SETUP_IDLE_TIMEOUT_SECONDS, WIFI_SETUP_MAX_BODY_BYTES, WIFI_SETUP_RESTART_HINT,
        WIFI_SETUP_SCAN_CONTENT_TYPE, WIFI_SETUP_TOTAL_TIMEOUT_SECONDS,
    };
    use crate::wifi_transfer::WIFI_TRANSFER_INACTIVITY_SECONDS;

    #[test]
    fn setup_portal_is_off_until_started() {
        let snapshot = WifiSetupSnapshot::default();
        assert_eq!(snapshot.state, WifiSetupState::Off);
        assert!(!snapshot.is_active());
        assert_eq!(setup_url(), "http://192.168.4.1/");
        assert_eq!(WIFI_SETUP_AP_SSID, "Rustmix-Setup");
    }

    #[test]
    fn html_mentions_ap_name_and_captive_url() {
        let html = setup_html();
        assert!(html.contains("Rustmix-Setup"));
        assert!(html.contains("192.168.4.1"));
        assert!(html.contains("/scan"));
        assert!(html.contains("refresh=1"));
        assert!(html.contains("/save"));
        assert!(html.contains("name=\"ssid\""));
        assert!(html.contains("name=\"password\""));
        assert!(html.contains("maxlength=\"64\""));
    }

    #[test]
    fn form_parser_accepts_urlencoded_credentials() {
        let config =
            parse_setup_form("ssid=Lab%20WiFi&password=correct-horse&timezone=UTC").unwrap();
        assert_eq!(config.ssid, "Lab WiFi");
        assert_eq!(config.password, "correct-horse");
        assert_eq!(config.timezone, "UTC");
    }

    #[test]
    fn form_parser_rejects_short_password_and_missing_ssid() {
        assert!(parse_setup_form("password=correct-horse").is_err());
        assert!(parse_setup_form("ssid=Lab&password=short").is_err());
    }

    #[test]
    fn scan_json_escapes_ssid_and_omits_secrets() {
        let response = scan_http_body(&[ScannedNetwork {
            ssid: "<Cafe & \"Main\">".into(),
            rssi_dbm: -40,
            open: false,
        }]);
        assert_eq!(response.content_type, WIFI_SETUP_SCAN_CONTENT_TYPE);
        assert_eq!(response.content_type, "application/json");
        assert!(response
            .json
            .contains(r#"\u003cCafe \u0026 \"Main\"\u003e"#));
        assert!(!response.json.contains('<'));
        assert!(!response.json.contains('>'));
        assert!(!response.json.contains('&'));
        assert!(!response.json.to_ascii_lowercase().contains("pass"));
        let quoted = scanned_networks_json(&[ScannedNetwork {
            ssid: "Cafe \"Main\"".into(),
            rssi_dbm: -40,
            open: false,
        }]);
        assert!(quoted.contains(r#"Cafe \"Main\""#));
    }

    #[test]
    fn timeouts_match_transfer_portal_and_distinguish_idle_from_total() {
        assert_eq!(
            WIFI_SETUP_IDLE_TIMEOUT_SECONDS,
            WIFI_TRANSFER_INACTIVITY_SECONDS
        );
        assert_eq!(
            WIFI_SETUP_TOTAL_TIMEOUT_SECONDS,
            WIFI_TRANSFER_INACTIVITY_SECONDS
        );
        assert_eq!(WIFI_SETUP_IDLE_TIMEOUT_SECONDS, 10 * 60);
        let limit = Duration::from_secs(WIFI_SETUP_TOTAL_TIMEOUT_SECONDS).as_millis() as u64;
        let clock = WifiSetupSessionClock::start(0);
        assert_eq!(clock.poll(limit - 1), None);
        assert_eq!(clock.poll(limit), Some(WifiSetupTimeoutKind::Idle));

        let mut active = WifiSetupSessionClock::start(0);
        active.note_activity(9 * 60 * 1_000);
        assert_eq!(
            active.poll(limit),
            Some(WifiSetupTimeoutKind::Total),
            "traffic resets idle, but the total cap still stops the AP"
        );
        assert_eq!(active.poll(limit - 1), None);
    }

    #[test]
    fn timed_out_snapshot_shows_how_to_restart_setup() {
        for kind in [WifiSetupTimeoutKind::Idle, WifiSetupTimeoutKind::Total] {
            let snapshot = WifiSetupSnapshot::timed_out(kind);
            assert_eq!(snapshot.state, WifiSetupState::TimedOut);
            assert!(!snapshot.is_active());
            let error = snapshot.error.expect("restart hint");
            assert!(error.contains(WIFI_SETUP_RESTART_HINT));
            assert!(error.contains("Settings > Network > Configure Wi-Fi"));
        }
    }

    #[test]
    fn teardown_stops_http_and_ap_radio_on_every_exit() {
        let running = SetupResources {
            http: true,
            ap_radio: true,
        };
        let http_failed_after_softap = SetupResources {
            http: false,
            ap_radio: true,
        };
        let stopped = SetupResources {
            http: false,
            ap_radio: false,
        };
        for exit in WifiSetupExit::ALL {
            let plan = exit.teardown();
            assert!(plan.stop_http, "{exit:?} must stop HTTP");
            assert!(plan.stop_ap_radio, "{exit:?} must stop the AP radio");
            assert_eq!(running.apply_exit(exit), stopped, "{exit:?}");
            assert_eq!(
                http_failed_after_softap.apply_exit(exit),
                stopped,
                "{exit:?} must drop the radio left up when HTTP failed to start"
            );
            assert!(!exit.reason().is_empty());
        }
        assert!(WifiSetupExit::SettingsStop.teardown().stop_ap_radio);
        assert!(WifiSetupExit::HttpServerFailed.teardown().stop_ap_radio);
        assert!(WifiSetupExit::IdleTimeout.shows_restart_hint());
        assert!(WifiSetupExit::TotalTimeout.shows_restart_hint());
        assert!(!WifiSetupExit::SettingsStop.shows_restart_hint());
    }

    #[test]
    fn oversize_body_is_rejected_with_413_and_not_saved() {
        let password = "p".repeat(49);
        let mut form = format!("ssid=Lab&password={password}&timezone=UTC&extra=");
        while form.len() < WIFI_SETUP_MAX_BODY_BYTES {
            form.push('x');
        }
        assert_eq!(form.len(), WIFI_SETUP_MAX_BODY_BYTES);
        let truncated = parse_setup_form(&form).expect("truncated prefix would have parsed");
        assert_eq!(truncated.password.len(), 49);
        form.push('y');

        let mut body = SetupBodyAccumulator::new();
        assert!(body.push(form.as_bytes()));
        let decision = body.decide();
        assert_eq!(decision, SetupPostDecision::TooLarge);
        assert_eq!(decision.status_code(), WIFI_SETUP_BODY_TOO_LARGE_STATUS);
        assert_eq!(decision.status_code(), 413);

        let mut exact = SetupBodyAccumulator::new();
        let mut fitting = format!("ssid=Lab&password=correct-horse&timezone=UTC&pad=");
        while fitting.len() < WIFI_SETUP_MAX_BODY_BYTES {
            fitting.push('a');
        }
        assert_eq!(fitting.len(), WIFI_SETUP_MAX_BODY_BYTES);
        assert!(!exact.push(fitting.as_bytes()));
        match exact.decide() {
            SetupPostDecision::Save(config) => {
                assert_eq!(config.ssid, "Lab");
                assert_eq!(config.password, "correct-horse");
            }
            other => panic!("complete body at the cap should save, got {other:?}"),
        }
    }

    #[test]
    fn scan_collapse_drops_setup_ssid_and_duplicates() {
        let collapsed = collapse_scan_results(vec![
            ScannedNetwork {
                ssid: "Home".into(),
                rssi_dbm: -70,
                open: false,
            },
            ScannedNetwork {
                ssid: "Home".into(),
                rssi_dbm: -40,
                open: false,
            },
            ScannedNetwork {
                ssid: WIFI_SETUP_AP_SSID.into(),
                rssi_dbm: -10,
                open: true,
            },
        ]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].rssi_dbm, -40);
    }
}
