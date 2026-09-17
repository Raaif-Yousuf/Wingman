use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::hotkey::Chord;
use crate::provider::{Anthropic, Chain, Ollama, OpenAi, Provider, DEFAULT_PROMPT};

/// `%APPDATA%\Wingman\config.toml`. See the design spec's "Config"
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
    pub ollama: OllamaConfig,
}

impl Default for Providers {
    fn default() -> Self {
        Self {
            // #13: Ollama is deliberately NOT in the default order. Unlike
            // openai/anthropic (unusable until the user pastes in a key,
            // so listing them by default costs nothing), a freshly
            // installed Ollama with a vision model pulled would start
            // answering silently for a user who never asked for a local
            // provider at all. Opt-in only, until the Mode work (#19)
            // decides what "Auto" should default to.
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
            ollama: OllamaConfig::default(),
        }
    }
}

/// `Debug` is hand-rolled below to redact `api_key`; every other derive
/// still applies, including `Serialize`/`Deserialize`, so config.toml round
/// trips are unaffected (#157).
#[derive(Clone, PartialEq, Deserialize, Serialize)]
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

/// Redacts `api_key` so a future `eprintln!("{cfg:?}")`, panic hook or
/// diagnostics dump can never print a live key by accident (#157). `Config`
/// and `Providers` keep their derived `Debug`, which calls into this impl
/// for the nested field.
impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("api_key", &"<redacted>")
            .field("models", &self.models)
            .finish()
    }
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

/// #13: local Ollama server. No `api_key` field -- a local server has
/// nothing to authenticate with, unlike `ProviderConfig`'s cloud providers.
/// Not in `Providers::order` by default; see the comment on
/// `Providers::default`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct OllamaConfig {
    /// `http://127.0.0.1:11434`, never `localhost` (CLAUDE.md rule 6).
    pub base_url: String,
    pub model: String,
    pub effort: String,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            base_url: crate::provider::ollama::DEFAULT_BASE_URL.to_string(),
            model: "gemma3:4b".to_string(),
            effort: "low".to_string(),
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
    /// Show the 1-10/Ultra difficulty badge in the card's bottom-right
    /// corner. When off, the rating is not requested from the model at all,
    /// so the rubric costs nothing on every call.
    pub show_difficulty: bool,
    /// System prompt, editable by the user in the TOML file.
    pub prompt: String,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            card_seconds: 12,
            text_scale: 1.0,
            show_difficulty: true,
            prompt: DEFAULT_PROMPT.to_string(),
        }
    }
}

impl Config {
    /// `%APPDATA%\Wingman\config.toml`.
    pub fn path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("could not determine the platform config directory")?;
        Ok(base.join("Wingman").join("config.toml"))
    }

    /// `%APPDATA%\copilot-ask\config.toml`, the pre-rename location. Only
    /// [`Config::migrate_from`] touches this path, once, to copy the file
    /// forward; nothing here ever reads its contents for any other purpose.
    pub fn old_path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("could not determine the platform config directory")?;
        Ok(base.join("copilot-ask").join("config.toml"))
    }

    /// One-time config migration for the copilot-ask -> Wingman rename.
    ///
    /// If `new_path` already exists, the migration already happened (or the
    /// user started fresh under the new name) and this is a no-op. Otherwise,
    /// if `old_path` exists, its bytes are copied to `new_path` verbatim and
    /// `old_path` is left untouched -- exactly what `uninstall.ps1` already
    /// promises for the pre-rename file, so that promise keeps holding across
    /// the rename too. Returns whether a copy happened.
    ///
    /// Pure w.r.t. paths (both are parameters, not read from `dirs`), so this
    /// is exercised in tests against scratch directories and never against
    /// the real `%APPDATA%`.
    pub fn migrate_from(old_path: &Path, new_path: &Path) -> Result<bool> {
        if new_path.exists() {
            return Ok(false);
        }
        if !old_path.exists() {
            return Ok(false);
        }
        if let Some(parent) = new_path.parent() {
            fs::create_dir_all(parent).context("failed to create the new config directory")?;
        }
        fs::copy(old_path, new_path).context("failed to copy the old config forward")?;
        Ok(true)
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
            let (cfg, repaired) = Self::parse_reporting_repair(&contents);
            if repaired {
                // Best effort: the in-memory value is already correct, so a
                // failed write here is not worth failing the load over.
                let _ = cfg.save_to(path);
            }
            cfg
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
    /// Parse without caring whether anything needed repairing. `load_from`
    /// uses [`Config::parse_reporting_repair`] so it can write the fix back;
    /// this is the plain form the tests read against.
    #[allow(dead_code)]
    pub fn parse_or_default(toml_str: &str) -> Config {
        let mut cfg: Config = toml::from_str(toml_str).unwrap_or_default();
        cfg.backfill();
        cfg
    }

    /// Like [`Config::parse_or_default`], but also reports whether the parse
    /// had to repair anything -- the caller writes the file back so the
    /// on-disk copy stops disagreeing with what is actually being sent.
    fn parse_reporting_repair(toml_str: &str) -> (Config, bool) {
        let mut cfg: Config = toml::from_str(toml_str).unwrap_or_default();
        let repaired = cfg.backfill();
        (cfg, repaired)
    }

    /// Fills in values a config written by an older build has no key for.
    ///
    /// `#[serde(default)]` resolves a missing key to the *field's* default,
    /// not to the curated value this build ships in `Providers::default()`.
    /// For `models` that means an existing config.toml silently yields an
    /// empty model list and an empty tray submenu, so an absent list is
    /// backfilled here rather than left empty. A list the user has genuinely
    /// customized is never touched.
    fn backfill(&mut self) -> bool {
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
        self.repair_refusal_trigger()
    }

    /// Rewrites one sentence of a stored prompt that makes Claude refuse.
    ///
    /// An earlier default told the model the detail field was "your scratchpad
    /// -- reason it out before committing to a verdict". Anthropic's safety
    /// classifier reads that as an attempt to extract the model's internal
    /// reasoning and declines outright: `stop_reason: "refusal"`, category
    /// `reasoning_extraction`, reproducible every time. The rubric appended by
    /// the difficulty toggle happened to mask it, so the failure only showed
    /// up with that toggle OFF.
    ///
    /// The prompt is user-editable and lives in config.toml, so fixing the
    /// constant does not fix configs already written. Only this one sentence
    /// is replaced, and only where it appears verbatim -- any other edits the
    /// user has made to their prompt are left alone.
    fn repair_refusal_trigger(&mut self) -> bool {
        const TRIGGER: &str =
            "This is your scratchpad \u{2014} reason it out before committing to a verdict.";
        const REPLACEMENT: &str = "Write this out in full before you write the headline, so that the headline states the conclusion this working actually reaches.";

        if self.ui.prompt.contains(TRIGGER) {
            self.ui.prompt = self.ui.prompt.replace(TRIGGER, REPLACEMENT);
            return true;
        }
        false
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
                "ollama" => providers.push(Box::new(Ollama::new(
                    self.providers.ollama.base_url.clone(),
                    self.providers.ollama.model.clone(),
                    self.providers.ollama.effort.clone(),
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
            "wingman-test-{tag}-{}-{}",
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

    // -- #13: Ollama config ---------------------------------------------

    #[test]
    fn default_order_does_not_include_ollama() {
        // Opt-in only -- see the comment on `Providers::default`. A
        // freshly installed Ollama with a vision model pulled must not
        // start answering for a user who never configured a local
        // provider.
        let config = Config::default();
        assert!(!config.providers.order.contains(&"ollama".to_string()));
    }

    #[test]
    fn ollama_default_base_url_is_never_localhost() {
        let config = Config::default();
        assert_eq!(config.providers.ollama.base_url, "http://127.0.0.1:11434");
        assert!(!config.providers.ollama.base_url.contains("localhost"));
    }

    #[test]
    fn ollama_default_model_is_a_vision_model() {
        let config = Config::default();
        assert_eq!(config.providers.ollama.model, "gemma3:4b");
    }

    #[test]
    fn build_chain_includes_ollama_only_when_explicitly_ordered() {
        let mut config = Config::default();
        config.providers.order = vec!["openai".to_string(), "ollama".to_string()];
        let chain = config.build_chain();
        assert_eq!(chain.provider_names(), vec!["openai", "ollama"]);
        // Ollama needs no API key to be ready -- unlike openai here.
        assert_eq!(chain.ready_provider_names(), vec!["ollama"]);
    }

    #[test]
    fn an_ollama_section_missing_from_an_older_config_backfills_to_defaults() {
        // A config.toml written before #13 has no [providers.ollama] table
        // at all; #[serde(default)] on `Providers` must still produce a
        // usable, non-empty OllamaConfig rather than an all-empty one.
        let old = r#"
[providers.openai]
model = "gpt-5.5"
api_key = "sk-x"
"#;
        let cfg = Config::parse_or_default(old);
        assert_eq!(cfg.providers.ollama.base_url, "http://127.0.0.1:11434");
        assert_eq!(cfg.providers.ollama.model, "gemma3:4b");
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

    #[test]
    fn a_stored_prompt_that_makes_claude_refuse_is_repaired() {
        // Configs written by an earlier build carry the sentence that trips
        // Anthropic's reasoning_extraction classifier. Loading must fix it,
        // or Claude refuses every request with the difficulty toggle off.
        let toml = "[ui]\nprompt = \"before. This is your scratchpad \u{2014} reason it out before committing to a verdict. after\"\n";
        let cfg = Config::parse_or_default(toml);
        assert!(
            !cfg.ui.prompt.contains("scratchpad"),
            "the refusal trigger must be gone: {}",
            cfg.ui.prompt
        );
        // Surrounding text the user may have written is preserved.
        assert!(cfg.ui.prompt.starts_with("before. "));
        assert!(cfg.ui.prompt.ends_with(" after"));
    }

    #[test]
    fn an_unrelated_custom_prompt_is_left_alone() {
        let toml = "[ui]\nprompt = \"Just check my algebra please.\"\n";
        let cfg = Config::parse_or_default(toml);
        assert_eq!(cfg.ui.prompt, "Just check my algebra please.");
    }

    #[test]
    fn the_shipped_default_contains_no_refusal_trigger() {
        assert!(!Config::default().ui.prompt.contains("scratchpad"));
    }

    // -- copilot-ask -> Wingman config migration -----------------------------

    fn scratch_dir(tag: &str) -> PathBuf {
        let unique = format!(
            "wingman-migrate-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn migrate_copies_the_old_file_when_the_new_one_is_absent() {
        let root = scratch_dir("copies");
        let old_path = root.join("old").join("config.toml");
        let new_path = root.join("new").join("config.toml");
        fs::create_dir_all(old_path.parent().unwrap()).unwrap();
        fs::write(&old_path, "[providers.openai]\napi_key = \"sk-old\"\n").unwrap();
        assert!(!new_path.exists());

        let migrated = Config::migrate_from(&old_path, &new_path).expect("migration should succeed");

        assert!(migrated, "a fresh old file with no new file should migrate");
        assert!(new_path.exists(), "the new path should now hold the config");
        assert_eq!(
            fs::read_to_string(&new_path).unwrap(),
            fs::read_to_string(&old_path).unwrap(),
            "the copy must be byte-for-byte identical"
        );
        // The old file is left alone -- uninstall.ps1's promise not to
        // delete it must keep holding across the rename.
        assert!(old_path.exists(), "the old file must survive the migration");

        cleanup(&root.join("dummy"));
    }

    #[test]
    fn migrate_does_nothing_when_the_new_file_already_exists() {
        let root = scratch_dir("new-exists");
        let old_path = root.join("old").join("config.toml");
        let new_path = root.join("new").join("config.toml");
        fs::create_dir_all(old_path.parent().unwrap()).unwrap();
        fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        fs::write(&old_path, "old content").unwrap();
        fs::write(&new_path, "new content, already set up").unwrap();

        let migrated = Config::migrate_from(&old_path, &new_path).expect("migration should succeed");

        assert!(!migrated, "an existing new file must never be overwritten");
        assert_eq!(fs::read_to_string(&new_path).unwrap(), "new content, already set up");

        cleanup(&root.join("dummy"));
    }

    #[test]
    fn migrate_does_nothing_when_the_old_file_is_absent() {
        let root = scratch_dir("no-old");
        let old_path = root.join("old").join("config.toml");
        let new_path = root.join("new").join("config.toml");
        assert!(!old_path.exists());
        assert!(!new_path.exists());

        let migrated = Config::migrate_from(&old_path, &new_path).expect("migration should succeed");

        assert!(!migrated, "nothing to migrate when there was never an old file");
        assert!(!new_path.exists(), "no new file should be created out of nothing");

        cleanup(&root.join("dummy"));
    }

    #[test]
    fn migrate_creates_the_new_config_directory() {
        // The new directory ("Wingman") never existed before the rename, so
        // migration must create it -- unlike Config::save, which is only ever
        // called after the directory has already been created once.
        let root = scratch_dir("mkdir");
        let old_path = root.join("old").join("config.toml");
        let new_path = root.join("brand-new-dir").join("nested").join("config.toml");
        fs::create_dir_all(old_path.parent().unwrap()).unwrap();
        fs::write(&old_path, "content").unwrap();
        assert!(!new_path.parent().unwrap().exists());

        Config::migrate_from(&old_path, &new_path).expect("migration should succeed");

        assert!(new_path.exists());

        cleanup(&root.join("dummy"));
    }

    #[test]
    fn old_path_and_path_differ_only_in_the_app_directory_name() {
        // Guards against the two paths silently pointing at the same place,
        // which would make every migration test above pass while migrating
        // nothing in production.
        let old = Config::old_path().unwrap();
        let new = Config::path().unwrap();
        assert_ne!(old, new);
        assert!(old.to_string_lossy().contains("copilot-ask"));
        assert!(new.to_string_lossy().contains("Wingman"));
        assert_eq!(old.parent().unwrap().parent(), new.parent().unwrap().parent());
    }

    /// #157: `{:?}` on `Config`/`ProviderConfig` must never leak the raw
    /// `api_key`, however deeply nested.
    #[test]
    fn debug_format_never_contains_the_api_key() {
        let mut config = Config::default();
        config.providers.openai.api_key = "sk-real-secret-openai".to_string();
        config.providers.anthropic.api_key = "sk-ant-real-secret-anthropic".to_string();

        let debug_output = format!("{config:?}");
        assert!(
            !debug_output.contains("sk-real-secret-openai"),
            "{debug_output}"
        );
        assert!(
            !debug_output.contains("sk-ant-real-secret-anthropic"),
            "{debug_output}"
        );
        assert!(debug_output.contains("redacted"), "{debug_output}");

        // Same check directly on the sub-struct, in case `Config`'s own
        // Debug impl is ever hand-rolled and stops delegating.
        let provider_debug = format!("{:?}", config.providers.openai);
        assert!(
            !provider_debug.contains("sk-real-secret-openai"),
            "{provider_debug}"
        );
    }

    /// #157: the redaction must be Debug-only. Serde/TOML round trips keep
    /// the real key, byte for byte.
    #[test]
    fn api_key_round_trips_through_toml_unredacted() {
        let mut config = Config::default();
        config.providers.openai.api_key = "sk-real-secret-value".to_string();

        let text = toml::to_string_pretty(&config).expect("serialize");
        assert!(
            text.contains("sk-real-secret-value"),
            "config.toml must keep the real key on disk; only Debug redacts"
        );

        let parsed: Config = toml::from_str(&text).expect("deserialize");
        assert_eq!(parsed.providers.openai.api_key, "sk-real-secret-value");
        assert_eq!(config, parsed);
    }
}
