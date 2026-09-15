use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::hotkey::Chord;
use crate::provider::{Anthropic, Chain, OpenAi, Provider, DEFAULT_PROMPT};

/// `%APPDATA%\copilot-ask\config.toml`. See the design spec's "Config"
/// section for the authoritative shape.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub hotkeys: Hotkeys,
    pub capture: Capture,
    pub providers: Providers,
    pub ui: Ui,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Hotkeys {
    /// Copilot key. Windows emits it as LeftWin + LeftShift + F23 (VK 0x86).
    pub primary: Chord,
    /// Secondary, always active alongside the primary. Ctrl+Shift+/.
    pub secondary: Chord,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            primary: Chord {
                vk: 0x86,
                ctrl: false,
                shift: true,
                alt: false,
                win: true,
            },
            secondary: Chord {
                vk: 0xBF,
                ctrl: true,
                shift: true,
                alt: false,
                win: false,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Capture {
    /// Long-edge downscale target.
    pub max_edge: u32,
    /// "active" | "primary"
    pub monitor: String,
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            max_edge: 1568,
            monitor: "active".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Providers {
    pub order: Vec<String>,
    pub openai: ProviderConfig,
    pub anthropic: ProviderConfig,
}

impl Default for Providers {
    fn default() -> Self {
        Self {
            order: vec!["openai".to_string(), "anthropic".to_string()],
            openai: ProviderConfig {
                model: "gpt-5.5".to_string(),
                effort: "low".to_string(),
                api_key: String::new(),
                models: vec![
                    "gpt-5.5".to_string(),
                    "gpt-5.5-pro".to_string(),
                    "gpt-5.4".to_string(),
                    "gpt-5.4-mini".to_string(),
                    "gpt-5.4-nano".to_string(),
                    "gpt-5.2".to_string(),
                    "gpt-5.1".to_string(),
                    "gpt-5".to_string(),
                    "gpt-5-mini".to_string(),
                    "gpt-4.1".to_string(),
                ],
            },
            anthropic: ProviderConfig {
                model: "claude-opus-5".to_string(),
                effort: "low".to_string(),
                api_key: String::new(),
                models: vec![
                    "claude-opus-5".to_string(),
                    "claude-sonnet-5".to_string(),
                    "claude-opus-4-8".to_string(),
                    "claude-haiku-4-5".to_string(),
                    "claude-fable-5-1".to_string(),
                ],
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub model: String,
    pub effort: String,
    pub api_key: String,
    /// Offered in the tray's model submenu. Editable here so a new model can
    /// be added without a rebuild; the active `model` is always shown even if
    /// it is missing from this list.
    pub models: Vec<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            effort: "low".to_string(),
            api_key: String::new(),
            models: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Ui {
    /// Auto-dismiss for the collapsed card, in seconds; 0 = never.
    pub card_seconds: u32,
    /// Multiplies every font size in the card. 1.0 is the built-in default;
    /// lower is smaller. Clamped by the card to a readable range.
    pub text_scale: f32,
    /// System prompt, editable by the user in the TOML file.
    pub prompt: String,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            card_seconds: 12,
            text_scale: 1.0,
            prompt: DEFAULT_PROMPT.to_string(),
        }
    }
}

impl Config {
    /// `%APPDATA%\copilot-ask\config.toml`.
    pub fn path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("could not determine the platform config directory")?;
        Ok(base.join("copilot-ask").join("config.toml"))
    }

    /// Loads the config from the well-known path, creating it from defaults
    /// on first run. Env vars always take precedence over the file, applied
    /// after loading.
    pub fn load() -> Result<Config> {
        let path = Self::path()?;
        Self::load_from(&path)
    }

    /// Same as [`Config::load`] but against an arbitrary path — the
    /// filesystem-touching core of `load()`, factored out so it can be
    /// exercised in tests against a scratch directory instead of the real
    /// `%APPDATA%`.
    pub fn load_from(path: &Path) -> Result<Config> {
        let mut config = if path.exists() {
            let contents = fs::read_to_string(path).unwrap_or_default();
            Self::parse_or_default(&contents)
        } else {
            let defaults = Config::default();
            defaults.save_to(path)?;
            defaults
        };
        config.apply_env_overrides();
        Ok(config)
    }

    /// Parses TOML into a `Config`, falling back to defaults field-by-field
    /// for missing keys/sections (via `#[serde(default)]` on every struct in
    /// this module), and to `Config::default()` wholesale if the text does
    /// not parse as TOML at all. Never errors — the app must always start.
    pub fn parse_or_default(toml_str: &str) -> Config {
        let mut cfg: Config = toml::from_str(toml_str).unwrap_or_default();
        cfg.backfill();
        cfg
    }

    /// Fills in values a config written by an older build has no key for.
    ///
    /// `#[serde(default)]` resolves a missing key to the *field's* default,
    /// not to the curated value this build ships in `Providers::default()`.
    /// For `models` that means an existing config.toml silently yields an
    /// empty model list and an empty tray submenu, so an absent list is
    /// backfilled here rather than left empty. A list the user has genuinely
    /// customized is never touched.
    fn backfill(&mut self) {
        let d = Providers::default();
        if self.providers.openai.models.is_empty() {
            self.providers.openai.models = d.openai.models;
        }
        if self.providers.anthropic.models.is_empty() {
            self.providers.anthropic.models = d.anthropic.models;
        }
        if self.ui.text_scale <= 0.0 {
            self.ui.text_scale = Ui::default().text_scale;
        }
    }

    /// `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`, when set, override the
    /// corresponding value loaded from the file.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            self.providers.openai.api_key = key;
        }
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            self.providers.anthropic.api_key = key;
        }
    }

    /// Writes the config to the well-known path.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        self.save_to(&path)
    }

    /// Same as [`Config::save`] but against an arbitrary path.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("failed to create config directory")?;
        }
        let toml_str = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(path, toml_str).context("failed to write config file")?;

        #[cfg(windows)]
        Self::restrict_acl(path);

        Ok(())
    }

    /// Restricts the config file's ACL to the current user only, via
    /// `icacls`. Best-effort: any failure (missing binary, non-NTFS volume,
    /// etc.) is ignored rather than propagated, since the config file is
    /// still perfectly usable without it.
    #[cfg(windows)]
    fn restrict_acl(path: &Path) {
        let username = match std::env::var("USERNAME") {
            Ok(u) if !u.is_empty() => u,
            _ => return,
        };
        let _ = std::process::Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{username}:F"))
            .output();
    }

    /// Builds the provider fallback chain per `providers.order`. Providers
    /// not named in `order` are omitted entirely; unrecognized names are
    /// ignored. Providers with an empty API key are still included in the
    /// chain but report themselves not-`ready()`, so `Chain::ask` skips
    /// them without failing.
    pub fn build_chain(&self) -> Chain {
        let mut providers: Vec<Box<dyn Provider>> = Vec::new();
        for name in &self.providers.order {
            match name.as_str() {
                "openai" => providers.push(Box::new(OpenAi::new(
                    self.providers.openai.api_key.clone(),
                    self.providers.openai.model.clone(),
                    self.providers.openai.effort.clone(),
                ))),
                "anthropic" => providers.push(Box::new(Anthropic::new(
                    self.providers.anthropic.api_key.clone(),
                    self.providers.anthropic.model.clone(),
                    self.providers.anthropic.effort.clone(),
                ))),
                _ => {}
            }
        }
        Chain::new(providers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serializes tests that mutate process-wide environment variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn scratch_path(tag: &str) -> PathBuf {
        let unique = format!(
            "copilot-ask-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("config.toml")
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn toml_round_trip() {
        let config = Config::default();
        let text = toml::to_string_pretty(&config).expect("serialize");
        let parsed: Config = toml::from_str(&text).expect("deserialize");
        assert_eq!(config, parsed);
    }

    #[test]
    fn defaults_on_missing_file() {
        let path = scratch_path("missing");
        assert!(!path.exists());

        let config = Config::load_from(&path).expect("load should succeed");
        assert_eq!(config.providers.order, vec!["openai", "anthropic"]);
        assert_eq!(config.hotkeys.primary.vk, 0x86);
        assert_eq!(config.capture.max_edge, 1568);
        assert_eq!(config.ui.card_seconds, 12);
        // The file should now exist, created from defaults.
        assert!(path.exists());

        cleanup(&path);
    }

    #[test]
    fn malformed_file_falls_back_to_defaults() {
        let path = scratch_path("malformed");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "this is not { valid toml at all [[[").unwrap();

        let config = Config::load_from(&path).expect("load should still succeed");
        assert_eq!(config, Config::default());

        cleanup(&path);
    }

    #[test]
    fn partially_missing_fields_fall_back_field_by_field() {
        // Valid TOML, but only sets one nested field; everything else,
        // including whole sections, should come from the defaults.
        let toml_str = r#"
            [capture]
            max_edge = 999
        "#;
        let config = Config::parse_or_default(toml_str);
        assert_eq!(config.capture.max_edge, 999);
        assert_eq!(config.capture.monitor, "active"); // default, section partially present
        assert_eq!(config.ui.card_seconds, 12); // default, whole section absent
        assert_eq!(config.providers.order, vec!["openai", "anthropic"]);
    }

    #[test]
    fn env_vars_take_precedence_over_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENAI_API_KEY", "env-openai-key");
        std::env::set_var("ANTHROPIC_API_KEY", "env-anthropic-key");

        let toml_str = r#"
            [providers.openai]
            api_key = "file-openai-key"
            [providers.anthropic]
            api_key = "file-anthropic-key"
        "#;
        let mut config = Config::parse_or_default(toml_str);
        assert_eq!(config.providers.openai.api_key, "file-openai-key");
        config.apply_env_overrides();
        assert_eq!(config.providers.openai.api_key, "env-openai-key");
        assert_eq!(config.providers.anthropic.api_key, "env-anthropic-key");

        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn no_env_vars_leaves_file_values_untouched() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");

        let toml_str = r#"
            [providers.openai]
            api_key = "file-openai-key"
        "#;
        let mut config = Config::parse_or_default(toml_str);
        config.apply_env_overrides();
        assert_eq!(config.providers.openai.api_key, "file-openai-key");
    }

    #[test]
    fn build_chain_respects_configured_order() {
        let mut config = Config::default();
        config.providers.order = vec!["anthropic".to_string(), "openai".to_string()];
        let chain = config.build_chain();
        assert_eq!(chain.provider_names(), vec!["anthropic", "openai"]);
    }

    #[test]
    fn build_chain_ignores_unknown_provider_names() {
        let mut config = Config::default();
        config.providers.order = vec!["bogus".to_string(), "openai".to_string()];
        let chain = config.build_chain();
        assert_eq!(chain.provider_names(), vec!["openai"]);
    }

    #[test]
    fn build_chain_marks_empty_key_providers_not_ready() {
        let mut config = Config::default();
        config.providers.anthropic.api_key = "sk-ant-real".to_string();
        // openai.api_key left empty (the default) -> should report unready.

        let chain = config.build_chain();
        assert_eq!(chain.provider_names(), vec!["openai", "anthropic"]);
        assert_eq!(chain.ready_provider_names(), vec!["anthropic"]);
    }

    #[test]
    fn config_from_an_older_build_gets_model_lists_backfilled() {
        // A config.toml written before the `models` key existed.
        let old = r#"
[providers.openai]
model = "gpt-5.5"
effort = "low"
api_key = "sk-x"
"#;
        let cfg = Config::parse_or_default(old);
        assert!(
            !cfg.providers.openai.models.is_empty(),
            "an absent models list must be backfilled, not left empty"
        );
        assert!(cfg.providers.openai.models.iter().any(|m| m == "gpt-5.5"));
        assert!(!cfg.providers.anthropic.models.is_empty());
        // The user's own values survive the backfill.
        assert_eq!(cfg.providers.openai.api_key, "sk-x");
        assert_eq!(cfg.ui.text_scale, 1.0);
    }

    #[test]
    fn a_customized_model_list_is_never_overwritten() {
        let custom = r#"
[providers.openai]
models = ["gpt-5-mini"]
[providers.anthropic]
models = ["claude-sonnet-5"]
"#;
        let cfg = Config::parse_or_default(custom);
        assert_eq!(cfg.providers.openai.models, vec!["gpt-5-mini".to_string()]);
        assert_eq!(
            cfg.providers.anthropic.models,
            vec!["claude-sonnet-5".to_string()]
        );
    }

    #[test]
    fn a_nonsense_text_scale_falls_back_to_the_default() {
        let cfg = Config::parse_or_default("[ui]
text_scale = 0.0
");
        assert_eq!(cfg.ui.text_scale, 1.0);
    }
}
