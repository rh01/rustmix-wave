//! Optional NVS mirror of Wi-Fi station credentials.
//!
//! SD `/RUSTMIX/WIFI.TXT` remains the first-class boot path. NVS is used when
//! that file is missing and as the always-written companion after SoftAP save.
//! The Wi-Fi stack already owns `EspDefaultNvsPartition::take()`, so this
//! module uses the C NVS API on a private namespace. The password is stored
//! but never logged.

use crate::network_config::NetworkConfig;

#[allow(dead_code)]
const NVS_NAMESPACE: &str = "rw_wifi";
#[allow(dead_code)]
const KEY_SSID: &str = "ssid";
#[allow(dead_code)]
const KEY_PASSWORD: &str = "pass";
#[allow(dead_code)]
const KEY_TIMEZONE: &str = "tz";
#[allow(dead_code)]
const KEY_NTP: &str = "ntp";

pub fn save_network_config(config: &NetworkConfig) {
    #[cfg(target_os = "espidf")]
    save_espidf(config);
    #[cfg(not(target_os = "espidf"))]
    {
        let _ = config;
    }
}

#[must_use]
pub fn load_network_config() -> Option<NetworkConfig> {
    #[cfg(target_os = "espidf")]
    {
        return load_espidf();
    }
    #[cfg(not(target_os = "espidf"))]
    None
}

#[cfg(target_os = "espidf")]
fn save_espidf(config: &NetworkConfig) {
    use core::ffi::CStr;
    use esp_idf_svc::sys::{
        nvs_close, nvs_commit, nvs_handle_t, nvs_open, nvs_open_mode_t_NVS_READWRITE, nvs_set_str,
        ESP_OK,
    };
    use std::ffi::CString;

    unsafe {
        let mut handle: nvs_handle_t = 0;
        let ns = CString::new(NVS_NAMESPACE).expect("nvs namespace");
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            log::warn!("rustmix-wave=wifi-nvs status=open-failed");
            return;
        }
        let ssid_key = CString::new(KEY_SSID).expect("nvs ssid key");
        let pass_key = CString::new(KEY_PASSWORD).expect("nvs pass key");
        let tz_key = CString::new(KEY_TIMEZONE).expect("nvs tz key");
        let ntp_key = CString::new(KEY_NTP).expect("nvs ntp key");
        let ssid = CString::new(config.ssid.as_str()).unwrap_or_else(|_| CString::new("").unwrap());
        let password =
            CString::new(config.password.as_str()).unwrap_or_else(|_| CString::new("").unwrap());
        let timezone =
            CString::new(config.timezone.as_str()).unwrap_or_else(|_| CString::new("UTC").unwrap());
        let ntp = CString::new(config.ntp_server.as_str())
            .unwrap_or_else(|_| CString::new("pool.ntp.org").unwrap());
        let _ = nvs_set_str(handle, ssid_key.as_ptr(), ssid.as_ptr());
        let _ = nvs_set_str(handle, pass_key.as_ptr(), password.as_ptr());
        let _ = nvs_set_str(handle, tz_key.as_ptr(), timezone.as_ptr());
        let _ = nvs_set_str(handle, ntp_key.as_ptr(), ntp.as_ptr());
        let _ = nvs_commit(handle);
        nvs_close(handle);
        let _ = CStr::from_ptr(ns.as_ptr());
        log::info!(
            "rustmix-wave=wifi-nvs status=saved ssid={} timezone={} ntp-server={}",
            config.ssid,
            config.timezone,
            config.ntp_server
        );
    }
}

#[cfg(target_os = "espidf")]
fn load_espidf() -> Option<NetworkConfig> {
    use crate::network_config::{DEFAULT_NTP_SERVER, DEFAULT_TIMEZONE};
    use esp_idf_svc::sys::{
        nvs_close, nvs_get_str, nvs_handle_t, nvs_open, nvs_open_mode_t_NVS_READONLY, ESP_OK,
    };
    use std::ffi::CString;

    fn read_str(
        handle: esp_idf_svc::sys::nvs_handle_t,
        key: &str,
        buf: &mut [i8],
    ) -> Option<String> {
        let key = CString::new(key).ok()?;
        let mut len = buf.len();
        let status = unsafe { nvs_get_str(handle, key.as_ptr(), buf.as_mut_ptr(), &mut len) };
        if status != ESP_OK || len == 0 {
            return None;
        }
        let bytes = unsafe {
            core::slice::from_raw_parts(buf.as_ptr() as *const u8, len.saturating_sub(1))
        };
        core::str::from_utf8(bytes).ok().map(str::to_owned)
    }

    unsafe {
        let mut handle: nvs_handle_t = 0;
        let ns = CString::new(NVS_NAMESPACE).expect("nvs namespace");
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return None;
        }
        let mut ssid_buf = [0i8; 40];
        let mut pass_buf = [0i8; 80];
        let mut tz_buf = [0i8; 40];
        let mut ntp_buf = [0i8; 80];
        let ssid = read_str(handle, KEY_SSID, &mut ssid_buf);
        let password = read_str(handle, KEY_PASSWORD, &mut pass_buf).unwrap_or_default();
        let timezone =
            read_str(handle, KEY_TIMEZONE, &mut tz_buf).unwrap_or_else(|| DEFAULT_TIMEZONE.into());
        let ntp_server =
            read_str(handle, KEY_NTP, &mut ntp_buf).unwrap_or_else(|| DEFAULT_NTP_SERVER.into());
        nvs_close(handle);
        let ssid = ssid?;
        let rendered = format!(
            "ssid={ssid}\npassword={password}\ntimezone={timezone}\nntp_server={ntp_server}\n"
        );
        match NetworkConfig::parse(&rendered) {
            Ok(config) => {
                log::info!(
                    "rustmix-wave=wifi-nvs status=loaded ssid={} timezone={} ntp-server={}",
                    config.ssid,
                    config.timezone,
                    config.ntp_server
                );
                Some(config)
            }
            Err(error) => {
                log::warn!("rustmix-wave=wifi-nvs status=invalid error={error:#}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::load_network_config;

    #[test]
    fn host_nvs_load_is_empty() {
        assert!(load_network_config().is_none());
    }
}
