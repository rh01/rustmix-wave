//! Optional NVS mirror of Reader preferences.
//!
//! SD `/RUSTMIX/READER/PREFS.TXT` remains authoritative. NVS is a fallback when
//! the card is missing and a boot-time restore source after a prefs parse error.
//! The full serialized document is stored so every preference survives, including
//! fields added after the original font and letter-spacing keys. The Wi-Fi stack
//! already owns `EspDefaultNvsPartition::take()`, so this module uses the C NVS
//! API on a private namespace.

use crate::reader::ReaderPreferences;

#[cfg(target_os = "espidf")]
use crate::reader::{BookFont, BookFontSize, LetterSpacing};

#[allow(dead_code)]
const NVS_NAMESPACE: &str = "rw_read";
#[allow(dead_code)]
const KEY_FONT_SIZE: &str = "font_px";
#[allow(dead_code)]
const KEY_BOOK_FONT: &str = "font_face";
#[allow(dead_code)]
const KEY_LETTER_SPACING: &str = "letter_px";
#[allow(dead_code)]
const KEY_PREFS_DOCUMENT: &str = "prefs";
#[allow(dead_code)]
const NVS_DOCUMENT_BYTES: usize = 1536;

/// Serialized preferences stored under the NVS `prefs` key.
#[must_use]
pub fn nvs_preferences_document(prefs: &ReaderPreferences) -> String {
    prefs.serialized()
}

/// Restore every field from an NVS document. Unknown keys inside it are ignored.
#[must_use]
pub fn apply_nvs_preferences_document(prefs: &mut ReaderPreferences, document: &str) -> bool {
    match ReaderPreferences::parse(document) {
        Ok(parsed) => {
            *prefs = parsed;
            true
        }
        Err(_) => false,
    }
}

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
        let spacing_key = CString::new(KEY_LETTER_SPACING).expect("nvs spacing key");
        let prefs_key = CString::new(KEY_PREFS_DOCUMENT).expect("nvs prefs key");
        let face = CString::new(prefs.nvs_face_marker())
            .unwrap_or_else(|_| CString::new("serif").unwrap());
        if let Ok(document) = CString::new(nvs_preferences_document(prefs)) {
            let _ = nvs_set_str(handle, prefs_key.as_ptr(), document.as_ptr());
        }
        let _ = nvs_set_i32(
            handle,
            size_key.as_ptr(),
            i32::from(prefs.font_size.pixels()),
        );
        let _ = nvs_set_str(handle, face_key.as_ptr(), face.as_ptr());
        let _ = nvs_set_i32(
            handle,
            spacing_key.as_ptr(),
            i32::from(prefs.letter_spacing.pixels()),
        );
        let _ = nvs_commit(handle);
        nvs_close(handle);
        let _ = CStr::from_ptr(ns.as_ptr());
        log::info!(
            "rustmix-wave=reader-nvs status=saved font-size={} book-font={} letter-spacing={}",
            prefs.font_size.marker(),
            prefs.nvs_face_marker(),
            prefs.letter_spacing.marker()
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
        let prefs_key = CString::new(KEY_PREFS_DOCUMENT).expect("nvs prefs key");
        let mut document = vec![0u8; NVS_DOCUMENT_BYTES];
        let mut document_len = document.len();
        if nvs_get_str(
            handle,
            prefs_key.as_ptr(),
            document.as_mut_ptr(),
            &mut document_len,
        ) == ESP_OK
        {
            let end = document_len.saturating_sub(1).min(document.len());
            if let Ok(text) = core::str::from_utf8(&document[..end]) {
                if apply_nvs_preferences_document(prefs, text) {
                    nvs_close(handle);
                    return true;
                }
            }
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
        let spacing_key = CString::new(KEY_LETTER_SPACING).expect("nvs spacing key");
        let mut spacing: i32 = 0;
        if nvs_get_i32(handle, spacing_key.as_ptr(), &mut spacing) == ESP_OK {
            if (0..=4).contains(&spacing) {
                if let Ok(step) = LetterSpacing::from_pixels(spacing as u8) {
                    prefs.letter_spacing = step;
                    changed = true;
                }
            }
        }
        // ESP-IDF 5.4 bindings use unsigned `c_char` on Xtensa.
        let mut buf = [0u8; 40];
        let mut len = buf.len();
        if nvs_get_str(handle, face_key.as_ptr(), buf.as_mut_ptr(), &mut len) == ESP_OK {
            let end = len.saturating_sub(1).min(buf.len());
            if let Ok(text) = core::str::from_utf8(&buf[..end]) {
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
    use super::{
        apply_nvs_preferences_document, load_reader_preferences_overlay, nvs_preferences_document,
    };
    use crate::reader::{BookFont, BookFontSize, LetterSpacing, LineSpacing, ReaderPreferences};

    #[test]
    fn host_nvs_overlay_is_a_no_op() {
        let mut prefs = ReaderPreferences::default();
        assert!(!load_reader_preferences_overlay(&mut prefs));
    }

    #[test]
    fn nvs_document_round_trips_every_preference_and_skips_unknown_keys() {
        let mut prefs = ReaderPreferences::default();
        prefs.book_font = BookFont::SdCjk;
        prefs.set_sd_cjk_file_name(Some("CJK.BIN"));
        prefs.font_size = BookFontSize::Px32;
        prefs.letter_spacing = LetterSpacing::Px2;
        prefs.line_spacing = LineSpacing::Px8;
        prefs.immersive = true;
        prefs.dark_mode = true;
        prefs.show_progress = false;
        prefs.status_page = false;
        let document = nvs_preferences_document(&prefs);
        assert!(document.len() < 1536);
        assert!(document.contains("show_progress=false"));
        assert!(document.contains("status_page=false"));
        let mut loaded = ReaderPreferences::default();
        assert!(apply_nvs_preferences_document(
            &mut loaded,
            &format!("{document}future_setting=1\n")
        ));
        assert_eq!(loaded, prefs);
    }
}
