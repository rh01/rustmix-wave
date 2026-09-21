//! SD-card Wi-Fi and SNTP provisioning configuration.
//!
//! Credentials are loaded at boot from removable storage. Keep parsing here so
//! firmware wiring never embeds, renders, or logs the password.

use std::{collections::BTreeMap, fs, path::Path};

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
        if password.len() > 63 {
            bail!("password must contain at most 63 UTF-8 bytes");
        }
        if !password.is_empty() && password.len() < 8 {
            bail!("secured Wi-Fi password must contain at least 8 UTF-8 bytes");
        }

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
