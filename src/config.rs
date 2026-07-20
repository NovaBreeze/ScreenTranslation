//! Persistent application configuration.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::security::dpapi;

const APP_DIR: &str = "ScreenTranslator";
const CONFIG_FILE: &str = "settings.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TranslationEngine {
    #[default]
    OpenAiCompatible,
    Ollama,
}

/// User-editable translation settings plus a DPAPI-protected API key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub api_base: String,
    pub model: String,
    pub target_lang: String,
    pub proxy: Option<String>,
    pub engine: TranslationEngine,
    pub hotkey: String,
    pub autostart: bool,
    pub copy_source: bool,
    pub copy_translation: bool,
    pub save_history: bool,
    pub prewarm_ocr: bool,
    pub multimodal_fallback: bool,
    pub multimodal_privacy_confirmed: bool,
    pub multimodal_confidence_threshold: f32,
    pub translation_font: String,
    pub smart_text_color: bool,
    pub translation_text_color: String,
    pub translation_background_color: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    protected_api_key: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.deepseek.com/v1".to_owned(),
            model: "deepseek-v4-flash".to_owned(),
            target_lang: "简体中文".to_owned(),
            proxy: None,
            engine: TranslationEngine::OpenAiCompatible,
            hotkey: "Ctrl+Alt+T".to_owned(),
            autostart: false,
            copy_source: false,
            copy_translation: false,
            save_history: true,
            prewarm_ocr: false,
            multimodal_fallback: false,
            multimodal_privacy_confirmed: false,
            multimodal_confidence_threshold: 0.55,
            translation_font: "Microsoft YaHei UI".to_owned(),
            smart_text_color: true,
            translation_text_color: "#101828".to_owned(),
            translation_background_color: "#FFFFFF".to_owned(),
            protected_api_key: String::new(),
        }
    }
}

impl AppConfig {
    /// Load the default config file, returning defaults when it does not exist.
    pub fn load() -> Result<Self> {
        Self::load_from(config_path()?)
    }

    /// Save this config to the default config file.
    pub fn save(&self) -> Result<()> {
        self.save_to(config_path()?)
    }

    /// Load from an explicit path. Useful for migrations and tests.
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("failed to parse config {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to read config {}", path.display()))
            }
        }
    }

    /// Save to an explicit path, creating its parent directory when necessary.
    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        let text = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(path, text).with_context(|| format!("failed to write config {}", path.display()))
    }

    /// Encrypt and retain an API key for the current OS user.
    pub fn set_api_key(&mut self, api_key: impl AsRef<str>) -> Result<()> {
        let api_key = api_key.as_ref();
        if api_key.is_empty() {
            self.protected_api_key.clear();
            return Ok(());
        }
        self.protected_api_key = STANDARD.encode(dpapi::protect(api_key.as_bytes())?);
        Ok(())
    }

    /// Decrypt the stored API key. `None` means no key has been configured.
    pub fn api_key(&self) -> Result<Option<String>> {
        if self.protected_api_key.is_empty() {
            return Ok(None);
        }
        let encrypted = STANDARD
            .decode(&self.protected_api_key)
            .context("stored API key is not valid base64")?;
        let plain = dpapi::unprotect(&encrypted)?;
        String::from_utf8(plain)
            .map(Some)
            .map_err(|_| anyhow!("stored API key is not valid UTF-8"))
    }
}

/// `%APPDATA%\ScreenTranslator` on Windows, platform config directory elsewhere.
pub fn config_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let app_data =
            std::env::var_os("APPDATA").ok_or_else(|| anyhow!("%APPDATA% is not defined"))?;
        return Ok(PathBuf::from(app_data).join(APP_DIR));
    }

    #[cfg(not(windows))]
    {
        directories::ProjectDirs::from("", "", APP_DIR)
            .map(|dirs| dirs.config_dir().to_path_buf())
            .ok_or_else(|| anyhow!("could not determine platform config directory"))
    }
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn api_key_is_encrypted_and_survives_save_reload() {
        let path = std::env::temp_dir().join(format!(
            "screen-translator-settings-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let mut config = AppConfig::default();
        config
            .set_api_key("sk-screen-translator-secret")
            .expect("DPAPI should encrypt");
        config.save_to(&path).expect("settings should save");
        let serialized = fs::read_to_string(&path).expect("settings should be readable");
        assert!(!serialized.contains("sk-screen-translator-secret"));

        let loaded = AppConfig::load_from(&path).expect("settings should reload");
        assert_eq!(
            loaded.api_key().expect("DPAPI should decrypt").as_deref(),
            Some("sk-screen-translator-secret")
        );
        let _ = fs::remove_file(path);
    }
}
