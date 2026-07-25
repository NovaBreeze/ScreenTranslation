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
    pub target_lang: String,
    pub proxy: Option<String>,
    /// 翻译供应商列表；列表顺序 × 每个供应商的 `models` 顺序即失败回退链。
    /// 旧的单供应商字段（api_base/model/engine/protected_api_key）在加载时
    /// 迁移为首元素，保存后不再出现。
    pub providers: Vec<ProviderConfig>,
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
}

/// 一个翻译供应商：接入地址 + 按顺序回退的模型列表 + DPAPI 加密的 API key。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// 显示名（设置页列表中展示）。
    pub name: String,
    pub engine: TranslationEngine,
    pub api_base: String,
    /// 模型回退顺序：首选模型失败时依次尝试后续模型。
    pub models: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) protected_api_key: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: "DeepSeek".to_owned(),
            engine: TranslationEngine::OpenAiCompatible,
            api_base: "https://api.deepseek.com/v1".to_owned(),
            models: vec!["deepseek-v4-flash".to_owned()],
            protected_api_key: String::new(),
        }
    }
}

impl ProviderConfig {
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

    /// 供错误信息/日志使用的显示名：`name（已脱敏）`。
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.api_base
        } else {
            &self.name
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            target_lang: "简体中文".to_owned(),
            proxy: None,
            providers: vec![ProviderConfig::default()],
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
            Ok(text) => Self::parse(&text)
                .with_context(|| format!("failed to parse config {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to read config {}", path.display()))
            }
        }
    }

    /// 解析配置文本；旧版单供应商字段在此迁移为 `providers` 首元素。
    fn parse(text: &str) -> Result<Self> {
        let mut value: toml::Value = toml::from_str(text)?;
        migrate_legacy_provider(&mut value);
        value.try_into().map_err(|error| anyhow!("{error}"))
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

    /// 是否存在任一已配置 API key 的云端供应商（决定首次启动是否弹出设置页）。
    pub fn has_configured_cloud_provider(&self) -> bool {
        self.providers.iter().any(|provider| {
            provider.engine == TranslationEngine::OpenAiCompatible
                && provider.api_key().ok().flatten().is_some()
        })
    }
}

/// 旧版单供应商字段（api_base/model/engine/protected_api_key）→ `providers` 首元素。
/// 已有 `providers` 或连旧字段都不存在的配置不受影响。
fn migrate_legacy_provider(value: &mut toml::Value) {
    let Some(table) = value.as_table_mut() else {
        return;
    };
    if table.contains_key("providers") {
        return;
    }
    let model = table
        .remove("model")
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|model| !model.trim().is_empty());
    let Some(model) = model else {
        return;
    };
    let api_base = table
        .remove("api_base")
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let engine = table.remove("engine");
    let protected_api_key = table.remove("protected_api_key");

    let name = if api_base.contains("deepseek") {
        "DeepSeek"
    } else if api_base.contains("opencode.ai/zen/go") {
        "OpenCode Go"
    } else if api_base.contains("opencode.ai") {
        "OpenCode Zen"
    } else if api_base.contains("openai.com") {
        "OpenAI"
    } else {
        "自定义"
    };
    let mut provider = toml::map::Map::new();
    provider.insert("name".into(), toml::Value::String(name.to_owned()));
    provider.insert(
        "api_base".into(),
        toml::Value::String(api_base.clone()),
    );
    provider.insert(
        "models".into(),
        toml::Value::Array(vec![toml::Value::String(model)]),
    );
    if let Some(engine) = engine {
        if engine.as_str() == Some("ollama") {
            provider.insert("name".into(), toml::Value::String("Ollama".to_owned()));
        }
        provider.insert("engine".into(), engine);
    }
    if let Some(key) = protected_api_key {
        provider.insert("protected_api_key".into(), key);
    }
    table.insert(
        "providers".into(),
        toml::Value::Array(vec![toml::Value::Table(provider)]),
    );
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

    fn temp_config_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "screen-translator-settings-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn api_key_is_encrypted_and_survives_save_reload() {
        let path = temp_config_path();
        let mut config = AppConfig::default();
        config.providers[0]
            .set_api_key("sk-screen-translator-secret")
            .expect("DPAPI should encrypt");
        config.save_to(&path).expect("settings should save");
        let serialized = fs::read_to_string(&path).expect("settings should be readable");
        assert!(!serialized.contains("sk-screen-translator-secret"));

        let loaded = AppConfig::load_from(&path).expect("settings should reload");
        assert_eq!(
            loaded.providers[0]
                .api_key()
                .expect("DPAPI should decrypt")
                .as_deref(),
            Some("sk-screen-translator-secret")
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn legacy_single_provider_config_migrates_to_providers() {
        let path = temp_config_path();
        let legacy = r#"
api_base = "https://api.deepseek.com/v1"
model = "deepseek-v4-flash"
engine = "open_ai_compatible"
target_lang = "English"
"#;
        fs::write(&path, legacy).expect("legacy config should write");
        let loaded = AppConfig::load_from(&path).expect("legacy config should load");
        assert_eq!(loaded.providers.len(), 1);
        let provider = &loaded.providers[0];
        assert_eq!(provider.name, "DeepSeek");
        assert_eq!(provider.api_base, "https://api.deepseek.com/v1");
        assert_eq!(provider.models, ["deepseek-v4-flash"]);
        assert_eq!(provider.engine, TranslationEngine::OpenAiCompatible);
        assert_eq!(loaded.target_lang, "English");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn legacy_ollama_config_migrates_with_engine() {
        let path = temp_config_path();
        let legacy = r#"
api_base = "http://127.0.0.1:11434"
model = "qwen3"
engine = "ollama"
"#;
        fs::write(&path, legacy).expect("legacy config should write");
        let loaded = AppConfig::load_from(&path).expect("legacy config should load");
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.providers[0].engine, TranslationEngine::Ollama);
        assert_eq!(loaded.providers[0].name, "Ollama");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn providers_round_trip() {
        let path = temp_config_path();
        let mut config = AppConfig::default();
        config.providers.push(ProviderConfig {
            name: "OpenCode Zen 免费".to_owned(),
            engine: TranslationEngine::OpenAiCompatible,
            api_base: "https://opencode.ai/zen/v1".to_owned(),
            models: vec!["big-pickle".to_owned(), "mimo-v2.5-free".to_owned()],
            protected_api_key: String::new(),
        });
        config.save_to(&path).expect("settings should save");
        let loaded = AppConfig::load_from(&path).expect("settings should reload");
        assert_eq!(loaded.providers.len(), 2);
        assert_eq!(loaded.providers[1].name, "OpenCode Zen 免费");
        assert_eq!(loaded.providers[1].models.len(), 2);
        // 已有 providers 的配置不再触发迁移。
        assert!(!loaded.providers.iter().any(|p| p.name == "自定义"));
        let _ = fs::remove_file(path);
    }
}
