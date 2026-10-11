use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub downloads_dir: Option<String>,
    /// "default" | "disabled" | "custom"
    pub relay_mode: String,
    pub relay_urls: Vec<String>,
    pub relay_token: Option<String>,
    /// "strict" | "public", what to do when a custom relay is unreachable.
    pub relay_fallback: String,
    /// "default" | "custom"
    pub discovery_mode: String,
    pub discovery_pkarr_relay_url: Option<String>,
    pub discovery_dns_origin: Option<String>,
    pub history_enabled: bool,
    /// "everyone" | "paired-only" | "off"
    pub discoverability: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            downloads_dir: None,
            relay_mode: "default".to_string(),
            relay_urls: Vec::new(),
            relay_token: None,
            relay_fallback: "strict".to_string(),
            discovery_mode: "default".to_string(),
            discovery_pkarr_relay_url: None,
            discovery_dns_origin: None,
            history_enabled: true,
            discoverability: "everyone".to_string(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).expect("settings serialize");
        std::fs::write(path, json)
    }

    pub fn relay_mode(&self) -> crate::engine::RelayModeOption {
        use std::str::FromStr;
        match self.relay_mode.as_str() {
            "disabled" => crate::engine::RelayModeOption::Disabled,
            "custom" => crate::engine::RelayModeOption::Custom {
                urls: self
                    .relay_urls
                    .iter()
                    .filter_map(|raw| {
                        let raw = raw.trim();
                        (!raw.is_empty()).then(|| iroh::RelayUrl::from_str(raw).ok())?
                    })
                    .collect(),
                auth_token: self.relay_token.clone().filter(|t| !t.is_empty()),
            },
            _ => crate::engine::RelayModeOption::Default,
        }
    }

    /// Relay config in the engine's IPC shape, for verify/status/fallback calls.
    pub fn relay_config_arg(&self) -> crate::engine::RelayConfigArg {
        crate::engine::RelayConfigArg {
            mode: self.relay_mode.clone(),
            urls: self.relay_urls.clone(),
            auth_token: self.relay_token.clone().filter(|t| !t.trim().is_empty()),
            fallback: Some(self.relay_fallback.clone()),
        }
    }

    pub fn relay_fallback(&self) -> crate::engine::RelayFallbackPolicy {
        match self.relay_fallback.as_str() {
            "public" => crate::engine::RelayFallbackPolicy::Public,
            _ => crate::engine::RelayFallbackPolicy::Strict,
        }
    }

    /// Discovery config in the engine's IPC shape, for verify/status calls.
    pub fn discovery_config_arg(&self) -> crate::engine::DiscoveryConfigArg {
        crate::engine::DiscoveryConfigArg {
            mode: self.discovery_mode.clone(),
            pkarr_relay_url: self
                .discovery_pkarr_relay_url
                .clone()
                .filter(|s| !s.trim().is_empty()),
            dns_origin: self
                .discovery_dns_origin
                .clone()
                .filter(|s| !s.trim().is_empty()),
        }
    }

    /// Resolved discovery mode; invalid persisted config falls back to default.
    pub fn discovery_mode(&self) -> crate::engine::DiscoveryModeOption {
        crate::engine::build_discovery_mode(Some(self.discovery_config_arg()))
            .unwrap_or(crate::engine::DiscoveryModeOption::Default)
    }

    pub fn discoverability(&self) -> crate::engine::Discoverability {
        match self.discoverability.as_str() {
            "paired-only" => crate::engine::Discoverability::PairedOnly,
            "off" => crate::engine::Discoverability::Off,
            _ => crate::engine::Discoverability::Everyone,
        }
    }

    /// The folder the user picked (or the system Downloads dir). This is what
    /// the settings page shows and saves back into `downloads_dir`.
    pub fn downloads_base(&self) -> Option<std::path::PathBuf> {
        // Android has no folder picker: earlier versions saved the shown
        // app-private default here, which must not pin it.
        #[cfg(target_os = "android")]
        return default_downloads_dir();
        #[cfg(not(target_os = "android"))]
        self.downloads_dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(default_downloads_dir)
    }

    /// Root received files are saved under: `downloads_base()` plus the
    /// app-owned "flipflop" subfolder, so received files never mix with
    /// unrelated downloads; the per-peer subfolder is joined by the receive
    /// flow on top of this.
    pub fn downloads_path(&self) -> Option<std::path::PathBuf> {
        Some(self.downloads_base()?.join("flipflop"))
    }
}

#[cfg(target_os = "android")]
fn default_downloads_dir() -> Option<std::path::PathBuf> {
    Some(crate::android::downloads_dir())
}

#[cfg(not(target_os = "android"))]
fn default_downloads_dir() -> Option<std::path::PathBuf> {
    // Without an XDG user-dirs config `download_dir()` is None on Linux;
    // ~/Downloads is the conventional spot then.
    dirs::download_dir().or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_corrupt_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("tm-slint-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        assert!(Settings::load(&path).history_enabled);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(Settings::load(&path).relay_mode, "default");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("tm-slint-settings-rt-{}", std::process::id()));
        let path = dir.join("nested/settings.json");
        let settings = Settings {
            downloads_dir: Some("/data/in".into()),
            relay_mode: "custom".into(),
            relay_urls: vec!["https://relay.example".into()],
            discoverability: "off".into(),
            history_enabled: false,
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.downloads_dir.as_deref(), Some("/data/in"));
        assert_eq!(loaded.relay_urls, ["https://relay.example"]);
        assert!(!loaded.history_enabled);
        assert!(matches!(
            loaded.discoverability(),
            crate::engine::Discoverability::Off
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn partial_files_fill_in_defaults() {
        let s: Settings = serde_json::from_str(r#"{"relay_mode":"disabled"}"#).unwrap();
        assert!(matches!(s.relay_mode(), crate::engine::RelayModeOption::Disabled));
        assert_eq!(s.relay_fallback, "strict");
        assert!(s.history_enabled);
    }

    #[test]
    fn custom_relays_skip_blank_and_invalid_urls() {
        let s = Settings {
            relay_mode: "custom".into(),
            relay_urls: vec![
                " ".into(),
                "not a url".into(),
                "https://relay.example".into(),
            ],
            relay_token: Some(String::new()),
            ..Settings::default()
        };
        match s.relay_mode() {
            crate::engine::RelayModeOption::Custom { urls, auth_token } => {
                assert_eq!(urls.len(), 1);
                assert!(auth_token.is_none());
            }
            other => panic!("expected custom relays, got {other:?}"),
        }
    }

    #[test]
    fn downloads_path_appends_app_folder_once() {
        let s = Settings {
            downloads_dir: Some(" /data/in ".into()),
            ..Settings::default()
        };
        assert_eq!(s.downloads_base(), Some("/data/in".into()));
        assert_eq!(s.downloads_path(), Some("/data/in/flipflop".into()));
    }
}
