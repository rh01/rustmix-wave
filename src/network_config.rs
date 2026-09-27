//! SD-card Wi-Fi and SNTP provisioning configuration.
//!
//! Credentials are loaded at boot from removable storage. Keep parsing here so
//! firmware wiring never embeds, renders, or logs the password.

use std::{collections::BTreeMap, fs, io::ErrorKind, path::Path};

use anyhow::{bail, Context, Result};

/// Read-only provisioning file consumed at boot.
pub const WIFI_CONFIG_PATH: &str = "/sdcard/RUSTMIX/WIFI.TXT";
/// Default SNTP pool used when the optional key is omitted.
pub const DEFAULT_NTP_SERVER: &str = "pool.ntp.org";
/// Default timezone profile used when the optional key is omitted.
pub const DEFAULT_TIMEZONE: &str = "America/New_York";

/// Validated boot-time network configuration.
#[derive(Clone, Eq, PartialEq)]
pub struct NetworkConfig {
    pub ssid: String,
    pub password: String,
    pub timezone: String,
    pub ntp_server: String,
}

impl core::fmt::Debug for NetworkConfig {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("NetworkConfig")
            .field("ssid", &self.ssid)
            .field("password", &"<redacted>")
            .field("timezone", &self.timezone)
            .field("ntp_server", &self.ntp_server)
            .finish()
    }
}

impl NetworkConfig {
    /// Load and validate the read-only boot configuration.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .with_context(|| format!("unable to read {}", path.display()))?;
        Self::parse(&contents)
            .with_context(|| format!("invalid network configuration in {}", path.display()))
    }

    /// Parse the intentionally small `key=value` provisioning format.
    pub fn parse(contents: &str) -> Result<Self> {
        let mut values = BTreeMap::<String, String>::new();
        for (line_number, raw_line) in contents.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("line {} must use key=value", line_number + 1))?;
            let key = key.trim();
            let value = value.trim();
            if !matches!(key, "ssid" | "password" | "timezone" | "ntp_server") {
                bail!("line {} uses unsupported key {key:?}", line_number + 1);
            }
            if values.insert(key.into(), value.into()).is_some() {
                bail!("line {} repeats key {key:?}", line_number + 1);
            }
        }

        let ssid = required(&values, "ssid")?.to_owned();
        if ssid.is_empty() || ssid.len() > 32 {
            bail!("ssid must contain 1 to 32 UTF-8 bytes");
        }

        let password = values.get("password").cloned().unwrap_or_default();
        validate_wifi_secret(&password)?;

        let timezone = values
            .get("timezone")
            .cloned()
            .unwrap_or_else(|| DEFAULT_TIMEZONE.into());
        if !matches!(timezone.as_str(), "America/New_York" | "UTC") {
            bail!("timezone must be America/New_York or UTC in this milestone");
        }

        let ntp_server = values
            .get("ntp_server")
            .cloned()
            .unwrap_or_else(|| DEFAULT_NTP_SERVER.into());
        if ntp_server.is_empty()
            || ntp_server.len() > 63
            || ntp_server.chars().any(char::is_whitespace)
        {
            bail!("ntp_server must be a non-empty hostname without whitespace");
        }

        Ok(Self {
            ssid,
            password,
            timezone,
            ntp_server,
        })
    }

    /// Serialize credentials for SD `/RUSTMIX/WIFI.TXT` (and SoftAP write-back).
    #[must_use]
    pub fn serialized(&self) -> String {
        format!(
            "# Rustmix Wave Wi-Fi. Edit this file or use Settings > Network > Configure Wi-Fi.\n\
ssid={}\n\
password={}\n\
timezone={}\n\
ntp_server={}\n",
            self.ssid, self.password, self.timezone, self.ntp_server
        )
    }

    /// Atomically replace `WIFI.TXT` when the SD card is present.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let temporary = path.with_extension("TMP");
        fs::write(&temporary, self.serialized())
            .with_context(|| format!("write {}", temporary.display()))?;
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| format!("replace {}", path.display()));
        }
        Ok(())
    }
}

fn required<'a>(values: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    values
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required key {key:?}"))
}

/// Accept an open network, an 8–63 byte passphrase, or a 64-character hex PSK.
fn validate_wifi_secret(password: &str) -> Result<()> {
    if password.is_empty() || (8..=63).contains(&password.len()) {
        return Ok(());
    }
    if password.len() == 64 && password.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(());
    }
    if password.len() == 64 {
        bail!("64-character Wi-Fi key must be a hexadecimal PSK");
    }
    bail!("password must contain 8 to 63 UTF-8 bytes, or 64 hexadecimal PSK characters");
}

/// What `/RUSTMIX/WIFI.TXT` contributed at boot.
#[derive(Debug)]
pub enum SdWifiTxt {
    /// The file is not on the card. NVS may supply credentials.
    Missing,
    /// The file is present but unusable. It stays first: do not replace it with NVS.
    Invalid {
        detail: String,
    },
    Ready(NetworkConfig),
}

/// Where boot credentials came from after applying the SD-first rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootWifiSource {
    Sd,
    Nvs,
    None,
}

/// Boot credential choice. An invalid `WIFI.TXT` never falls through to NVS.
#[derive(Debug)]
pub struct BootWifiResolution {
    pub credentials: Option<NetworkConfig>,
    pub source: BootWifiSource,
    /// Set when `WIFI.TXT` exists but cannot be used. Includes the words `WIFI.TXT is invalid`.
    pub invalid_wifi_txt: Option<String>,
}

/// Read SD `WIFI.TXT`, distinguishing a missing file from an invalid one.
pub fn load_sd_wifi_txt(path: impl AsRef<Path>) -> SdWifiTxt {
    let path = path.as_ref();
    match fs::read_to_string(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => SdWifiTxt::Missing,
        Err(error) => SdWifiTxt::Invalid {
            detail: format!("unable to read WIFI.TXT ({error})"),
        },
        Ok(contents) => match NetworkConfig::parse(&contents) {
            Ok(config) => SdWifiTxt::Ready(config),
            Err(error) => SdWifiTxt::Invalid {
                detail: format!("{error:#}"),
            },
        },
    }
}

/// Keep `WIFI.TXT` first. NVS is only the fallback when that file is missing.
#[must_use]
pub fn resolve_boot_wifi(sd: SdWifiTxt, nvs: Option<NetworkConfig>) -> BootWifiResolution {
    match sd {
        SdWifiTxt::Ready(config) => BootWifiResolution {
            credentials: Some(config),
            source: BootWifiSource::Sd,
            invalid_wifi_txt: None,
        },
        SdWifiTxt::Missing => match nvs {
            Some(config) => BootWifiResolution {
                credentials: Some(config),
                source: BootWifiSource::Nvs,
                invalid_wifi_txt: None,
            },
            None => BootWifiResolution {
                credentials: None,
                source: BootWifiSource::None,
                invalid_wifi_txt: None,
            },
        },
        SdWifiTxt::Invalid { detail } => BootWifiResolution {
            credentials: None,
            source: BootWifiSource::None,
            invalid_wifi_txt: Some(format!("WIFI.TXT is invalid: {detail}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{NetworkConfig, DEFAULT_NTP_SERVER, DEFAULT_TIMEZONE};

    #[test]
    fn parses_minimal_configuration_with_safe_defaults() {
        let config = NetworkConfig::parse("ssid=Lab WiFi\npassword=correct-horse\n").unwrap();
        assert_eq!(config.ssid, "Lab WiFi");
        assert_eq!(config.password, "correct-horse");
        assert_eq!(config.timezone, DEFAULT_TIMEZONE);
        assert_eq!(config.ntp_server, DEFAULT_NTP_SERVER);
    }

    #[test]
    fn parses_comments_open_network_and_explicit_timezone() {
        let config = NetworkConfig::parse(
            "# removable SD provisioning\nssid=Guest\npassword=\ntimezone=UTC\nntp_server=time.example.org\n",
        )
        .unwrap();
        assert_eq!(config.ssid, "Guest");
        assert!(config.password.is_empty());
        assert_eq!(config.timezone, "UTC");
        assert_eq!(config.ntp_server, "time.example.org");
    }

    #[test]
    fn debug_output_never_leaks_password() {
        let config = NetworkConfig::parse("ssid=Lab\npassword=secret123\n").unwrap();
        let debug = format!("{config:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret123"));
    }

    #[test]
    fn rejects_unknown_duplicate_and_short_password_keys() {
        assert!(NetworkConfig::parse("ssid=Lab\nextra=value\n").is_err());
        assert!(NetworkConfig::parse("ssid=Lab\nssid=Other\n").is_err());
        assert!(NetworkConfig::parse("ssid=Lab\npassword=short\n").is_err());
    }

    #[test]
    fn accepts_64_character_hex_psk() {
        let psk = "ab".repeat(32);
        assert_eq!(psk.len(), 64);
        let config = NetworkConfig::parse(&format!("ssid=Lab\npassword={psk}\n")).unwrap();
        assert_eq!(config.password, psk);

        let upper = "AB".repeat(32);
        let config = NetworkConfig::parse(&format!("ssid=Lab\npassword={upper}\n")).unwrap();
        assert_eq!(config.password, upper);
    }

    #[test]
    fn rejects_64_character_non_hex_and_overlong_secrets() {
        let non_hex = "z".repeat(64);
        assert!(NetworkConfig::parse(&format!("ssid=Lab\npassword={non_hex}\n")).is_err());
        let overlong = "a".repeat(65);
        assert!(NetworkConfig::parse(&format!("ssid=Lab\npassword={overlong}\n")).is_err());
        assert!(NetworkConfig::parse("ssid=Lab\npassword=short\n").is_err());
        let passphrase = "p".repeat(63);
        assert!(NetworkConfig::parse(&format!("ssid=Lab\npassword={passphrase}\n")).is_ok());
    }

    #[test]
    fn invalid_wifi_txt_stays_first_and_does_not_use_nvs() {
        let nvs = NetworkConfig::parse("ssid=FromNvs\npassword=correct-horse\n").unwrap();
        let decision = super::resolve_boot_wifi(
            super::SdWifiTxt::Invalid {
                detail:
                    "password must contain 8 to 63 UTF-8 bytes, or 64 hexadecimal PSK characters"
                        .into(),
            },
            Some(nvs),
        );
        assert!(decision.credentials.is_none());
        assert_eq!(decision.source, super::BootWifiSource::None);
        let message = decision.invalid_wifi_txt.expect("invalid file message");
        assert!(message.contains("WIFI.TXT is invalid"));
        assert!(!message.contains("FromNvs"));
    }

    #[test]
    fn missing_wifi_txt_falls_back_to_nvs_and_valid_sd_wins() {
        let nvs = NetworkConfig::parse("ssid=FromNvs\npassword=correct-horse\n").unwrap();
        let missing = super::resolve_boot_wifi(super::SdWifiTxt::Missing, Some(nvs.clone()));
        assert_eq!(missing.source, super::BootWifiSource::Nvs);
        assert_eq!(missing.credentials.unwrap().ssid, "FromNvs");
        assert!(missing.invalid_wifi_txt.is_none());

        let sd = NetworkConfig::parse("ssid=FromSd\npassword=correct-horse\n").unwrap();
        let ready = super::resolve_boot_wifi(super::SdWifiTxt::Ready(sd), Some(nvs));
        assert_eq!(ready.source, super::BootWifiSource::Sd);
        assert_eq!(ready.credentials.unwrap().ssid, "FromSd");
    }

    #[test]
    fn load_sd_wifi_txt_reports_missing_and_invalid_separately() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rustmix-wifi-status-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing-WIFI.TXT");
        assert!(matches!(
            super::load_sd_wifi_txt(&missing),
            super::SdWifiTxt::Missing
        ));
        let invalid = dir.join("WIFI.TXT");
        std::fs::write(&invalid, "ssid=Lab\npassword=short\n").unwrap();
        match super::load_sd_wifi_txt(&invalid) {
            super::SdWifiTxt::Invalid { detail } => {
                assert!(!detail.is_empty());
            }
            other => panic!("expected invalid WIFI.TXT, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_round_trips_through_wifi_txt() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rustmix-wifi-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("WIFI.TXT");
        let original = NetworkConfig::parse("ssid=Lab WiFi\npassword=correct-horse\n").unwrap();
        original.save_to_path(&path).unwrap();
        let loaded = NetworkConfig::load_from_path(&path).unwrap();
        assert_eq!(loaded.ssid, "Lab WiFi");
        assert_eq!(loaded.password, "correct-horse");
        let _ = std::fs::remove_dir_all(dir);
    }
}
