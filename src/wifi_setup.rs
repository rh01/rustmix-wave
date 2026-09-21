//! SoftAP captive Wi-Fi provisioning at `http://192.168.4.1`.
//!
//! Station credentials still load first from SD `/RUSTMIX/WIFI.TXT`. SoftAP is
//! the on-device path when that file is missing, STA association fails, or
//! Settings > Network > Configure Wi-Fi is selected. Saving writes NVS and,
//! when the card is present, writes `WIFI.TXT` back. The LAN transfer portal
//! stays a separate STA-only service.

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
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
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
        time::Duration,
    };

    use anyhow::{anyhow, Result};
    use embedded_svc::{
        http::Method,
        io::{Read as _, Write as _},
    };
    use esp_idf_svc::http::server::{Configuration, EspHttpServer};
    use log::info;

    use super::{
        parse_setup_form, scanned_networks_json, setup_html, setup_url, ScannedNetwork,
        WifiSetupSnapshot, WifiSetupState, SAVED_HTML, WIFI_SETUP_HTTP_PORT,
        WIFI_SETUP_SERVER_STACK_BYTES,
    };
    use crate::network_config::NetworkConfig;

    struct SharedStatus {
        snapshot: WifiSetupSnapshot,
        networks: Vec<ScannedNetwork>,
        pending: Option<NetworkConfig>,
        scan_requested: bool,
    }

    impl SharedStatus {
        fn ready() -> Self {
            Self {
                snapshot: WifiSetupSnapshot::ready(0),
                networks: Vec::new(),
                pending: None,
                scan_requested: true,
            }
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

            server.fn_handler("/", Method::Get, move |request| {
                request
                    .into_ok_response()?
                    .write_all(setup_html().as_bytes())?;
                Ok::<(), anyhow::Error>(())
            })?;
            server.fn_handler("/index.html", Method::Get, move |request| {
                request
                    .into_ok_response()?
                    .write_all(setup_html().as_bytes())?;
                Ok::<(), anyhow::Error>(())
            })?;
            server.fn_handler("/generate_204", Method::Get, move |request| {
                request
                    .into_ok_response()?
                    .write_all(setup_html().as_bytes())?;
                Ok::<(), anyhow::Error>(())
            })?;
            server.fn_handler("/hotspot-detect.html", Method::Get, move |request| {
                request
                    .into_ok_response()?
                    .write_all(setup_html().as_bytes())?;
                Ok::<(), anyhow::Error>(())
            })?;
            server.fn_handler("/ncsi.txt", Method::Get, move |request| {
                request
                    .into_ok_response()?
                    .write_all(setup_html().as_bytes())?;
                Ok::<(), anyhow::Error>(())
            })?;
            server.fn_handler("/connecttest.txt", Method::Get, move |request| {
                request
                    .into_ok_response()?
                    .write_all(setup_html().as_bytes())?;
                Ok::<(), anyhow::Error>(())
            })?;

            let scan_shared = Arc::clone(&shared);
            server.fn_handler("/scan", Method::Get, move |request| {
                let mut locked = lock(&scan_shared);
                if request.uri().contains("refresh=1") {
                    locked.scan_requested = true;
                    locked.snapshot.last_action = "Scan requested".into();
                }
                let body = scanned_networks_json(&locked.networks);
                request.into_ok_response()?.write_all(body.as_bytes())?;
                Ok::<(), anyhow::Error>(())
            })?;

            let save_shared = Arc::clone(&shared);
            server.fn_handler("/save", Method::Post, move |mut request| {
                let mut buffer = [0_u8; 256];
                let mut total = 0;
                loop {
                    if total == buffer.len() {
                        break;
                    }
                    let read = request.read(&mut buffer[total..])?;
                    if read == 0 {
                        break;
                    }
                    total += read;
                }
                let body = core::str::from_utf8(&buffer[..total])
                    .map_err(|_| anyhow!("form is not UTF-8"))?;
                let config = parse_setup_form(body)?;
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
}

#[cfg(test)]
mod tests {
    use super::{
        collapse_scan_results, parse_setup_form, scanned_networks_json, setup_html, setup_url,
        ScannedNetwork, WifiSetupSnapshot, WifiSetupState, WIFI_SETUP_AP_SSID,
    };

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
        let json = scanned_networks_json(&[ScannedNetwork {
            ssid: r#"Cafe "Main""#.into(),
            rssi_dbm: -40,
            open: false,
        }]);
        assert!(json.contains(r#"Cafe \"Main\""#));
        assert!(!json.to_ascii_lowercase().contains("pass"));
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
