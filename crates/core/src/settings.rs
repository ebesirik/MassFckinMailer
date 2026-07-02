//! App-wide preferences (currently just the UI language), persisted to
//! `{config_dir}/massfckinmailer/settings.toml`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// UI theme preference. `Auto` follows the operating system's light/dark setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePref {
    Light,
    Dark,
    #[default]
    Auto,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    /// BCP-47-ish locale code, e.g. `en`, `tr`, `pt`.
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub theme: ThemePref,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            language: default_language(),
            theme: ThemePref::default(),
        }
    }
}

fn default_language() -> String {
    "en".to_string()
}

impl AppSettings {
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("massfckinmailer").join("settings.toml"))
    }

    /// Load settings; any error (missing file, parse failure) yields defaults.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }
}
