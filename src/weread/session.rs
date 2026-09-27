//! Web session cookies, official API key, and SD/NVS persistence records.

use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use crate::weread::limits::{
    MAX_COOKIE_CHARS, MAX_FIELD_CHARS, SESSION_BAK, SESSION_FILE, SESSION_TMP,
    SKEY_RENEW_AFTER_SECS,
};

#[derive(Clone, PartialEq, Eq)]
pub struct Session {
    pub vid: String,
    pub skey: String,
    pub rt: String,
    pub ql: String,
    pub name: String,
    pub api_key: String,
    pub skey_unix: u64,
    pub covers: bool,
    pub handshake: String,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            vid: String::new(),
            skey: String::new(),
            rt: String::new(),
            ql: "0".into(),
            name: String::new(),
            api_key: String::new(),
            skey_unix: 0,
            covers: false,
            handshake: String::new(),
        }
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Session")
            .field("vid_len", &self.vid.len())
            .field("skey_len", &self.skey.len())
            .field("rt_len", &self.rt.len())
            .field("has_api_key", &self.has_api_key())
            .field("covers", &self.covers)
            .field("skey_unix", &self.skey_unix)
            .finish()
    }
}

impl Session {
    #[must_use]
    pub fn web_signed_in(&self) -> bool {
        !self.vid.is_empty() && !self.skey.is_empty()
    }

    #[must_use]
    pub fn has_api_key(&self) -> bool {
        valid_api_key(&self.api_key)
    }

    #[must_use]
    pub fn needs_renewal(&self, now_unix: u64) -> bool {
        self.web_signed_in()
            && (self.skey_unix == 0
                || now_unix.saturating_sub(self.skey_unix) >= SKEY_RENEW_AFTER_SECS)
    }

    pub fn expire_web(&mut self) {
        self.skey.clear();
        self.rt.clear();
        self.skey_unix = 0;
        self.handshake.clear();
    }

    #[must_use]
    pub fn cookie_header(&self) -> String {
        let mut parts = Vec::new();
        push_cookie(&mut parts, "wr_vid", &self.vid);
        push_cookie(&mut parts, "wr_skey", &self.skey);
        push_cookie(&mut parts, "wr_rt", &self.rt);
        push_cookie(&mut parts, "wr_ql", &self.ql);
        if !self.handshake.is_empty() {
            if parts.is_empty() {
                return truncate_cookie(&self.handshake);
            }
            parts.push(truncate_cookie(&self.handshake));
        }
        parts.join("; ")
    }

    pub fn absorb_set_cookie(&mut self, header: &str) {
        for name in ["wr_vid", "wr_skey", "wr_rt", "wr_ql", "wr_name"] {
            if let Some(value) = cookie_value(header, name) {
                self.set_cookie(name, &value);
            }
        }
    }

    pub fn set_cookie(&mut self, name: &str, value: &str) {
        let value = sanitize_cookie(value);
        match name {
            "wr_vid" => self.vid = value,
            "wr_skey" => self.skey = value,
            "wr_rt" => self.rt = value,
            "wr_ql" => self.ql = value,
            "wr_name" => self.name = value.chars().take(MAX_FIELD_CHARS).collect(),
            _ => {}
        }
    }

    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "WRSS1\nvid={}\nskey={}\nrt={}\nql={}\nname={}\nskey_unix={}\napi_key={}\ncovers={}\n",
            self.vid,
            self.skey,
            self.rt,
            self.ql,
            self.name.replace(['\n', '\r'], " "),
            self.skey_unix,
            self.api_key,
            if self.covers { "1" } else { "0" }
        )
    }

    pub fn decode(text: &str) -> Result<Self, &'static str> {
        if text.len() > 8 * 1024 {
            return Err("session record is too large");
        }
        let mut session = Self::default();
        let mut version_ok = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "WRSS1" {
                version_ok = true;
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "vid" => session.vid = sanitize_cookie(value),
                "skey" => session.skey = sanitize_cookie(value),
                "rt" => session.rt = sanitize_cookie(value),
                "ql" => session.ql = sanitize_cookie(value),
                "name" => session.name = value.chars().take(MAX_FIELD_CHARS).collect(),
                "skey_unix" => session.skey_unix = value.parse().unwrap_or(0),
                "api_key" => {
                    if value.is_empty() || valid_api_key(value) {
                        session.api_key = value.to_string();
                    }
                }
                "covers" => session.covers = value == "1",
                _ => {}
            }
        }
        if !version_ok {
            return Err("session record is not WRSS1");
        }
        Ok(session)
    }
}

#[must_use]
pub fn valid_api_key(value: &str) -> bool {
    let bytes = value.as_bytes();
    (12..=80).contains(&bytes.len())
        && value.starts_with("wrk-")
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub fn parse_portal_body(body: &str) -> Result<PortalUpdate, &'static str> {
    if body.len() > 256 {
        return Err("WeRead portal body is too large");
    }
    let mut key: Option<String> = None;
    let mut covers: Option<bool> = None;
    for part in body.split('&') {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        let value = percent_decode(value);
        match name {
            "api_key" => {
                if value.is_empty() {
                    key = Some(String::new());
                } else if valid_api_key(&value) {
                    key = Some(value);
                } else {
                    return Err("API key must look like wrk-...");
                }
            }
            "covers" => covers = Some(value == "1"),
            _ => {}
        }
    }
    if key.is_none() && covers.is_none() {
        return Err("WeRead portal body needs api_key or covers");
    }
    Ok(PortalUpdate {
        api_key: key,
        covers,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalUpdate {
    pub api_key: Option<String>,
    pub covers: Option<bool>,
}

pub fn load_session(root: &Path) -> Session {
    let mut session = read_session_file(&session_path(root)).unwrap_or_default();
    if let Some(update) = read_config(root) {
        if let Some(key) = update.api_key {
            session.api_key = key;
        }
        if let Some(covers) = update.covers {
            session.covers = covers;
        }
    }
    session
}

pub fn store(root: &Path, session: &Session) -> Result<(), String> {
    write_config(root, session)?;
    save_session(root, session)
}

pub fn save_session(root: &Path, session: &Session) -> Result<(), String> {
    let dir = root.join("WEREAD");
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let encoded = session.encode();
    if encoded.len() > 8 * 1024 {
        return Err("session record is too large".into());
    }
    atomic_write(
        &dir.join(SESSION_FILE),
        &dir.join(SESSION_TMP),
        &dir.join(SESSION_BAK),
        encoded.as_bytes(),
    )
}

pub fn apply_portal_update(root: &Path, update: &PortalUpdate) -> Result<Session, String> {
    let mut session = load_session(root);
    if let Some(key) = &update.api_key {
        session.api_key = key.clone();
    }
    if let Some(covers) = update.covers {
        session.covers = covers;
    }
    write_config(root, &session)?;
    save_session(root, &session)?;
    Ok(session)
}

fn read_config(root: &Path) -> Option<PortalUpdate> {
    let text = fs::read_to_string(root.join("WEREAD.TXT")).ok()?;
    parse_config_text(&text).ok()
}

fn write_config(root: &Path, session: &Session) -> Result<(), String> {
    let text = format!(
        "api_key={}\ncovers={}\n",
        session.api_key,
        if session.covers { "1" } else { "0" }
    );
    atomic_write(
        &root.join("WEREAD.TXT"),
        &root.join("WEREAD.TMP"),
        &root.join("WEREAD.BAK"),
        text.as_bytes(),
    )
}

fn parse_config_text(text: &str) -> Result<PortalUpdate, &'static str> {
    if text.len() > 512 {
        return Err("WeRead config is too large");
    }
    let mut body = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        if !body.is_empty() {
            body.push('&');
        }
        body.push_str(line);
    }
    parse_portal_body(&body)
}

fn read_session_file(path: &Path) -> Result<Session, &'static str> {
    let meta = fs::metadata(path).map_err(|_| "missing session")?;
    if meta.len() > 8 * 1024 {
        return Err("session record is too large");
    }
    let text = fs::read_to_string(path).map_err(|_| "session read failed")?;
    Session::decode(&text)
}

fn session_path(root: &Path) -> PathBuf {
    root.join("WEREAD").join(SESSION_FILE)
}

pub fn atomic_write(
    path: &Path,
    temporary: &Path,
    backup: &Path,
    bytes: &[u8],
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    {
        let mut file = File::create(temporary).map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().ok();
    }
    if path.exists() {
        let _ = fs::rename(path, backup);
    }
    fs::rename(temporary, path).map_err(|error| error.to_string())?;
    Ok(())
}

fn push_cookie(parts: &mut Vec<String>, name: &str, value: &str) {
    if !value.is_empty() {
        parts.push(format!("{name}={value}"));
    }
}

fn sanitize_cookie(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '\r' && *ch != '\n' && *ch != ';')
        .take(MAX_COOKIE_CHARS)
        .collect()
}

fn truncate_cookie(value: &str) -> String {
    value.chars().take(MAX_COOKIE_CHARS).collect()
}

/// Join every `Set-Cookie` value. ESP-IDF's header map keeps only the last
/// one, so the HTTP client calls this from the on-header event instead.
pub fn append_set_cookie(combined: &mut String, value: &str, max_bytes: usize) {
    let value = value.trim();
    if value.is_empty() || combined.len() >= max_bytes {
        return;
    }
    let separator = if combined.is_empty() { "" } else { ", " };
    let mut addition = String::new();
    addition.push_str(separator);
    addition.push_str(value);
    let room = max_bytes.saturating_sub(combined.len());
    if addition.len() > room {
        let mut end = room;
        while end > 0 && !addition.is_char_boundary(end) {
            end -= 1;
        }
        addition.truncate(end);
    }
    if addition.is_empty() || addition == ", " {
        return;
    }
    combined.push_str(&addition);
}

fn cookie_value(header: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let start = header.find(&needle)?;
    if start > 0 {
        let previous = header.as_bytes()[start - 1];
        if previous.is_ascii_alphanumeric() || previous == b'_' {
            return None;
        }
    }
    let rest = &header[start + needle.len()..];
    let end = rest.find([';', ',', '\n', '\r']).unwrap_or(rest.len());
    let value = rest[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(sanitize_cookie(value))
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(decoded) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            ) {
                out.push(decoded);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{load_session, parse_portal_body, save_session, Session};
    use std::fs;

    #[test]
    fn session_round_trip_and_cookie_overlay() {
        let mut session = Session::default();
        session.vid = "42".into();
        session.skey = "secret".into();
        session.rt = "refresh".into();
        session.api_key = "wrk-abcDEF1234".into();
        session.skey_unix = 1_700_000_000;
        session.covers = true;
        let decoded = Session::decode(&session.encode()).unwrap();
        assert_eq!(decoded.vid, "42");
        assert!(decoded.has_api_key());
        assert!(decoded.needs_renewal(1_700_000_000 + 46 * 60));
        assert!(!decoded.needs_renewal(1_700_000_000 + 60));
        session.absorb_set_cookie("wr_skey=new-key; Max-Age=3600, wr_rt=rt2");
        assert_eq!(session.skey, "new-key");
        assert_eq!(session.rt, "rt2");
        assert!(session.cookie_header().contains("wr_skey=new-key"));
        let mut jar = String::new();
        super::append_set_cookie(&mut jar, "wr_vid=9; Path=/", 256);
        super::append_set_cookie(&mut jar, "wr_skey=rotated; HttpOnly", 256);
        super::append_set_cookie(&mut jar, "wr_rt=rt-new; Secure", 256);
        session.absorb_set_cookie(&jar);
        assert_eq!(session.vid, "9");
        assert_eq!(session.skey, "rotated");
        assert_eq!(session.rt, "rt-new");
        let mut capped = String::new();
        super::append_set_cookie(&mut capped, "wr_vid=1; Path=/", 12);
        super::append_set_cookie(&mut capped, "wr_skey=too-long", 12);
        assert!(capped.len() <= 12);
    }

    #[test]
    fn portal_body_rejects_hostile_keys_and_persists_a_valid_one() {
        assert!(parse_portal_body("api_key=not-a-key").is_err());
        assert!(parse_portal_body(&format!("api_key={}", "A".repeat(300))).is_err());
        let update = parse_portal_body("api_key=wrk-abcDEF1234&covers=1").unwrap();
        let dir = std::env::temp_dir().join(format!("weread-session-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut session = Session::default();
        session.vid = "7".into();
        session.skey = "s".into();
        session.api_key = update.api_key.clone().unwrap();
        session.covers = true;
        save_session(&dir, &session).unwrap();
        let loaded = load_session(&dir);
        assert_eq!(loaded.vid, "7");
        assert!(loaded.covers);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_session_file_is_rejected() {
        let huge = format!("WRSS1\nvid={}\n", "x".repeat(9000));
        assert!(Session::decode(&huge).is_err());
    }
}
