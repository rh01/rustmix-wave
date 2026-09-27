//! Optional NVS mirror of the web session.
//!
//! The SD card remains authoritative. NVS restores `wr_vid`, `wr_skey`, and
//! `wr_rt` when the card is absent. The official API key stays in `WEREAD.TXT`.

use crate::weread::session::Session;

#[allow(dead_code)]
const NVS_NAMESPACE: &str = "rw_weread";

pub fn save(session: &Session) {
    #[cfg(target_os = "espidf")]
    save_espidf(session);
    #[cfg(not(target_os = "espidf"))]
    {
        let _ = session;
    }
}

pub fn overlay(session: &mut Session) -> bool {
    #[cfg(target_os = "espidf")]
    {
        return overlay_espidf(session);
    }
    #[cfg(not(target_os = "espidf"))]
    {
        let _ = session;
        false
    }
}

#[cfg(target_os = "espidf")]
fn save_espidf(session: &Session) {
    use esp_idf_svc::sys::{
        nvs_close, nvs_commit, nvs_handle_t, nvs_open, nvs_open_mode_t_NVS_READWRITE, ESP_OK,
    };
    use std::ffi::CString;

    unsafe {
        let mut handle: nvs_handle_t = 0;
        let Ok(ns) = CString::new(NVS_NAMESPACE) else {
            return;
        };
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            log::warn!("rustmix-wave=weread-nvs status=open-failed");
            return;
        }
        let _ = put_str(handle, "vid", &session.vid);
        let _ = put_str(handle, "skey", &session.skey);
        let _ = put_str(handle, "rt", &session.rt);
        let _ = put_str(handle, "ql", &session.ql);
        let _ = put_str(handle, "name", &session.name);
        let _ = put_str(handle, "skeyu", &session.skey_unix.to_string());
        let _ = nvs_commit(handle);
        nvs_close(handle);
        log::info!(
            "rustmix-wave=weread-nvs status=saved web={}",
            session.web_signed_in()
        );
    }
}

#[cfg(target_os = "espidf")]
fn overlay_espidf(session: &mut Session) -> bool {
    use esp_idf_svc::sys::{
        nvs_close, nvs_handle_t, nvs_open, nvs_open_mode_t_NVS_READONLY, ESP_OK,
    };
    use std::ffi::CString;

    unsafe {
        let mut handle: nvs_handle_t = 0;
        let Ok(ns) = CString::new(NVS_NAMESPACE) else {
            return false;
        };
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return false;
        }
        let mut changed = false;
        if session.vid.is_empty() {
            if let Some(value) = get_str(handle, "vid") {
                session.vid = value;
                changed = true;
            }
        }
        if session.skey.is_empty() {
            if let Some(value) = get_str(handle, "skey") {
                session.skey = value;
                changed = true;
            }
        }
        if session.rt.is_empty() {
            if let Some(value) = get_str(handle, "rt") {
                session.rt = value;
                changed = true;
            }
        }
        if session.ql.is_empty() {
            if let Some(value) = get_str(handle, "ql") {
                session.ql = value;
            }
        }
        if session.name.is_empty() {
            if let Some(value) = get_str(handle, "name") {
                session.name = value;
            }
        }
        if session.skey_unix == 0 {
            if let Some(value) = get_str(handle, "skeyu") {
                session.skey_unix = value.parse().unwrap_or(0);
            }
        }
        nvs_close(handle);
        changed
    }
}

#[cfg(target_os = "espidf")]
unsafe fn put_str(
    handle: esp_idf_svc::sys::nvs_handle_t,
    key: &str,
    value: &str,
) -> Result<(), ()> {
    use esp_idf_svc::sys::{nvs_set_str, ESP_OK};
    use std::ffi::CString;

    let key = CString::new(key).map_err(|_| ())?;
    let value = CString::new(value).map_err(|_| ())?;
    if nvs_set_str(handle, key.as_ptr(), value.as_ptr()) == ESP_OK {
        Ok(())
    } else {
        Err(())
    }
}

#[cfg(target_os = "espidf")]
unsafe fn get_str(handle: esp_idf_svc::sys::nvs_handle_t, key: &str) -> Option<String> {
    use esp_idf_svc::sys::{nvs_get_str, ESP_OK};
    use std::ffi::CString;

    let key = CString::new(key).ok()?;
    // Xtensa `c_char` is unsigned. A byte buffer casts cleanly on this target.
    let mut buf = vec![0u8; 520];
    let mut len = buf.len();
    if nvs_get_str(handle, key.as_ptr(), buf.as_mut_ptr().cast(), &mut len) != ESP_OK || len == 0 {
        return None;
    }
    let end = len.saturating_sub(1).min(buf.len());
    core::str::from_utf8(&buf[..end]).ok().map(str::to_string)
}
