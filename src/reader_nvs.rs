//! Optional NVS mirror of Reader font preferences.
//!
//! SD `/RUSTMIX/READER/PREFS.TXT` remains authoritative. NVS is a fallback when
//! the card is missing and a boot-time restore source after a prefs parse error.
//! The Wi-Fi stack already owns `EspDefaultNvsPartition::take()`, so this
//! module uses the C NVS API on a private namespace.

use crate::reader::ReaderPreferences;

#[cfg(target_os = "espidf")]
use crate::reader::{BookFont, BookFontSize};

#[allow(dead_code)]
const NVS_NAMESPACE: &str = "rw_read";
#[allow(dead_code)]
const KEY_FONT_SIZE: &str = "font_px";
#[allow(dead_code)]
const KEY_BOOK_FONT: &str = "font_face";

pub fn save_reader_preferences(prefs: &ReaderPreferences) {
    #[cfg(target_os = "espidf")]
    save_espidf(prefs);
    #[cfg(not(target_os = "espidf"))]
    {
        let _ = prefs;
    }
}

#[must_use]
pub fn load_reader_preferences_overlay(prefs: &mut ReaderPreferences) -> bool {
    #[cfg(target_os = "espidf")]
    {
        return load_espidf(prefs);
    }
    #[cfg(not(target_os = "espidf"))]
    {
        let _ = prefs;
        false
    }
}

#[cfg(target_os = "espidf")]
fn save_espidf(prefs: &ReaderPreferences) {
    use core::ffi::CStr;
    use esp_idf_svc::sys::{
        nvs_close, nvs_commit, nvs_handle_t, nvs_open, nvs_open_mode_t_NVS_READWRITE, nvs_set_i32,
        nvs_set_str, ESP_OK,
    };
    use std::ffi::CString;

    unsafe {
        let mut handle: nvs_handle_t = 0;
        let ns = CString::new(NVS_NAMESPACE).expect("nvs namespace");
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            log::warn!("rustmix-wave=reader-nvs status=open-failed");
            return;
        }
        let size_key = CString::new(KEY_FONT_SIZE).expect("nvs size key");
        let face_key = CString::new(KEY_BOOK_FONT).expect("nvs face key");
        let face = CString::new(prefs.nvs_face_marker())
            .unwrap_or_else(|_| CString::new("serif").unwrap());
        let _ = nvs_set_i32(
            handle,
            size_key.as_ptr(),
            i32::from(prefs.font_size.pixels()),
        );
        let _ = nvs_set_str(handle, face_key.as_ptr(), face.as_ptr());
        let _ = nvs_commit(handle);
        nvs_close(handle);
        let _ = CStr::from_ptr(ns.as_ptr());
        log::info!(
            "rustmix-wave=reader-nvs status=saved font-size={} book-font={}",
            prefs.font_size.marker(),
            prefs.nvs_face_marker()
        );
    }
}

#[cfg(target_os = "espidf")]
fn load_espidf(prefs: &mut ReaderPreferences) -> bool {
    use esp_idf_svc::sys::{
        nvs_close, nvs_get_i32, nvs_get_str, nvs_handle_t, nvs_open, nvs_open_mode_t_NVS_READONLY,
        ESP_OK,
    };
    use std::ffi::CString;

    unsafe {
        let mut handle: nvs_handle_t = 0;
        let ns = CString::new(NVS_NAMESPACE).expect("nvs namespace");
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return false;
        }
        let mut px: i32 = 0;
        let size_key = CString::new(KEY_FONT_SIZE).expect("nvs size key");
        let mut changed = false;
        if nvs_get_i32(handle, size_key.as_ptr(), &mut px) == ESP_OK {
            if let Ok(size) = BookFontSize::from_pixels(px as u8) {
                prefs.font_size = size;
                changed = true;
            }
        }
        let face_key = CString::new(KEY_BOOK_FONT).expect("nvs face key");
        let mut buf = [0i8; 40];
        let mut len = buf.len();
        if nvs_get_str(handle, face_key.as_ptr(), buf.as_mut_ptr(), &mut len) == ESP_OK {
            let bytes =
                core::slice::from_raw_parts(buf.as_ptr() as *const u8, len.saturating_sub(1));
            if let Ok(text) = core::str::from_utf8(bytes) {
                if let Ok(font) = BookFont::parse(text) {
                    prefs.apply_parsed_book_font(font, text);
                    changed = true;
                }
            }
        }
        nvs_close(handle);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::load_reader_preferences_overlay;
    use crate::reader::ReaderPreferences;

    #[test]
    fn host_nvs_overlay_is_a_no_op() {
        let mut prefs = ReaderPreferences::default();
        assert!(!load_reader_preferences_overlay(&mut prefs));
    }
}
