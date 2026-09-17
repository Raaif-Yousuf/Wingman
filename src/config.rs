use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::hotkey::Chord;
use crate::mode::Mode;
use crate::provider::openai_compat::{CompatAuth, Structured};
use crate::provider::{
    Anthropic, Chain, Gemini, Ollama, OpenAi, OpenAiCompat, Provider, DEFAULT_PROMPT,
};
use crate::secrets::{target_name, CredManagerStore, SecretStore};

/// Sentinel `hydrate_secrets` writes into a `ProviderConfig::api_key` field
/// when the store has a credential for that provider but [`SecretStore::get`]
/// returned `Err` (#175: a transient `CredReadW` failure, or a blob this
/// build cannot decode). Never a real key -- the control characters make it
/// impossible for a pasted key to collide with it by accident.
/// `push_secrets_to_store` recognises this marker and leaves that provider's
/// credential in the store untouched (instead of deleting it) unless the
/// field no longer holds the marker, i.e. the user typed something else;
/// `ui::settings` recognises it too, to show the field as
/// present-but-unreadable instead of masked stars.
pub const UNREADABLE_KEY_MARKER: &str = "\u{1}wingman-credential-unreadable\u{1}";

/// `%APPDATA%\Wingman\config.toml`. See the design spec's "Config"
/// section for the authoritative shape.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub hotkeys: Hotkeys,
    pub capture: Capture,
    pub providers: Providers,
    pub ui: Ui,
    /// Provider names (`"openai"`, `"anthropic"`) [`Config::hydrate_secrets`]
    /// could not read a stored credential for (#175). Load-time diagnostic
    /// only -- never persisted (`#[serde(skip)]`), so a caller with access to
    /// the card (`app.rs`) can surface it once right after `Config::load()`
    /// without config.toml ever carrying it forward.
    #[serde(skip)]
    pub unreadable_secrets: Vec<String>,
    /// Issue #19: `Cloud | Local | Auto | Offline`. Defaults to `Auto` per
    /// the expansion plan's Modes table. `App::run` publishes this to
    /// `mode::set_current` at startup, and `App::set_mode` keeps the
    /// process-wide mirror in sync with every tray change and re-saves it
    /// here, the same pattern `providers.order` already uses for the
    /// Provider submenu.
    pub mode: Mode,
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
    pub gemini: ProviderConfig,
    pub ollama: OllamaConfig,
    /// #16: one entry per OpenAI-compatible endpoint (OpenRouter, Groq,
    /// Mistral, DeepSeek, xAI, LM Studio, llama.cpp, vLLM, Azure, ...). Each
    /// entry's `providers.order` name is `"compat:<name>"` -- see
    /// [`compat_order_name`]. Empty by default: unlike `ollama`, there is no
    /// sensible single default endpoint to ship.
    pub compat: Vec<CompatConfig>,
}

impl Default for Providers {
    fn default() -> Self {
        Self {
            // #13: Ollama is deliberately NOT in the default order. Unlike
            // openai/anthropic (unusable until the user pastes in a key,
            // so listing them by default costs nothing), a freshly
            // installed Ollama with a vision model pulled would start
            // answering silently for a user who never asked for a local
            // provider at all. Opt-in only -- issue #19 settled this: being
            // named here IS the opt-in signal `mode::should_probe_ollama`
            // gates Auto mode's reachability probe on, which is what keeps
            // Auto free of added latency for a user who never configured
            // Ollama (see that function's doc comment).
            //
            // #17: gemini is a cloud provider (same "unusable until a key
            // is pasted in" shape as openai/anthropic), so the ollama
            // reasoning above does not apply to it -- it is still left out
            // of the default order for a simpler reason: Settings' Active
            // provider control is a two-way openai/anthropic radio with no
            // way to represent a third cloud provider at all (tracked
            // against #51). #194 fixed `ui::settings::build_config` so a
            // hand-added `"gemini"` entry now survives every Settings save
            // untouched (it only reorders openai/anthropic relative to each
            // other via `merge_provider_order`) -- so putting gemini in the
            // default order is no longer blocked by data loss, only by
            // Settings having nothing to show for it. Opt-in via a
            // hand-edited config.toml until #51.
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
            // #17: current Gemini vision models per
            // https://ai.google.dev/gemini-api/docs/models (fetched
            // 2026-09-17). `gemini-3.8-flash` is the current flagship Flash
            // model; the rest span the 3.x line plus the still-current
            // 2.5-series (see `provider::gemini::supports_thinking`'s doc
            // for why the 2.5 pair don't get `thinkingConfig.thinkingLevel`).
            gemini: ProviderConfig {
                model: "gemini-3.8-flash".to_string(),
                effort: "low".to_string(),
                api_key: String::new(),
                models: vec![
                    "gemini-3.8-flash".to_string(),
                    "gemini-3.1-pro-preview".to_string(),
                    "gemini-3.5-flash".to_string(),
                    "gemini-2.5-pro".to_string(),
                    "gemini-2.5-flash".to_string(),
                ],
            },
            ollama: OllamaConfig::default(),
            // #16: opt-in only, same reasoning as ollama above -- there is
            // no compat endpoint every install should silently start
            // talking to.
            compat: Vec::new(),
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

/// #16: one `[[providers.compat]]` entry -- an OpenAI-compatible
/// `/chat/completions` endpoint (OpenRouter, Groq, Mistral, DeepSeek, xAI,
/// Together, LM Studio, llama.cpp, vLLM, Azure, ...). `Debug` is hand-rolled
/// below to redact `api_key`, mirroring [`ProviderConfig`] (#157).
#[derive(Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CompatConfig {
    /// The endpoint's short, user-chosen id (`"openrouter"`, `"lmstudio"`).
    /// Combined with `"compat:"` to make the `providers.order` entry and the
    /// Credential Manager target name -- see [`compat_order_name`].
    pub name: String,
    /// e.g. `https://openrouter.ai/api/v1` or `http://127.0.0.1:1234/v1` --
    /// no trailing `/chat/completions`, that suffix is added by
    /// [`crate::provider::openai_compat::OpenAiCompat`]. Whether this
    /// endpoint counts as local or cloud for mode selection is decided from
    /// this URL's host via `mode::classify_host`, never from `name`.
    pub base_url: String,
    pub auth: CompatAuth,
    /// The header name used when `auth == ApiKeyHeader` (e.g. `"X-Api-Key"`
    /// for Mistral). Ignored for `Bearer`/`None`.
    pub auth_header: String,
    /// Stored in Credential Manager as `Wingman/compat:<name>` (#16), same
    /// import/hydrate/blank lifecycle as the fixed cloud providers -- see
    /// `Config::import_secrets_and_blank` et al. No env var override (#16's
    /// "no env override unless trivial": a dynamic, user-named list of
    /// endpoints has no single fixed env var name to override).
    pub api_key: String,
    pub model: String,
    /// Offered in the tray's model submenu, same role as
    /// [`ProviderConfig::models`].
    pub models: Vec<String>,
    pub structured: Structured,
}

impl std::fmt::Debug for CompatConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompatConfig")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("auth_header", &self.auth_header)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("models", &self.models)
            .field("structured", &self.structured)
            .finish()
    }
}

/// The `providers.order` entry (and Credential Manager target-name suffix)
/// for a compat endpoint named `name` -- `"compat:<name>"`. The one place
/// this string is built, so `Providers::provider_for`,
/// `Providers::build_chain_for_mode` and the secret-store loops can never
/// drift apart on the spelling.
pub fn compat_order_name(name: &str) -> String {
    format!("compat:{name}")
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
    /// so the rubric costs nothing on every call. Defaults to `false`
    /// (issue #197, owner decision 2026-09-17): the rating is being moved
    /// out of the core app and into the "Check my work" action (#37) as an
    /// opt-in, student-oriented option, so no user should pay its ~500
    /// system-prompt tokens per press unless they turn it on. `#[serde(default)]`
    /// only fills this in when the key is *absent* from config.toml, so an
    /// existing user who already has `show_difficulty = true` on disk keeps
    /// seeing the badge; only a fresh config (or one where the user never set
    /// the key) picks up the new `false` default.
    pub show_difficulty: bool,
    /// System prompt, editable by the user in the TOML file.
    pub prompt: String,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            card_seconds: 12,
            text_scale: 1.0,
            show_difficulty: false,
            prompt: DEFAULT_PROMPT.to_string(),
        }
    }
}

impl Config {
    /// `%APPDATA%\Wingman\config.toml`.
    pub fn path() -> Result<PathBuf> {
        let base = crate::known_folder::roaming_app_data()
            .context("could not determine the platform config directory")?;
        Ok(base.join("Wingman").join("config.toml"))
    }

    /// `%APPDATA%\copilot-ask\config.toml`, the pre-rename location. Only
    /// [`Config::migrate_from`] touches this path, once, to copy the file
    /// forward; nothing here ever reads its contents for any other purpose.
    pub fn old_path() -> Result<PathBuf> {
        let base = crate::known_folder::roaming_app_data()
            .context("could not determine the platform config directory")?;
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
    /// on first run. On top of [`Config::load_from`]'s file-only load, this
    /// also runs the Credential Manager import/hydrate step (#2) against the
    /// real store: any live `api_key` still sitting in the file is moved
    /// into Credential Manager and blanked on disk, then every provider's
    /// key (freshly imported or already store-only) is read back so the
    /// in-memory `Config` has it, before env vars get their final say. Env
    /// vars always take precedence, applied last.
    ///
    /// Only this entry point touches the real store -- [`Config::load_from`]
    /// stays store-free so tests that exercise it against a scratch path
    /// never write a fabricated test key into the real `Wingman/<provider>`
    /// credentials (rule 9).
    pub fn load() -> Result<Config> {
        let path = Self::path()?;
        let mut config = Self::load_from_file(&path)?;

        let store = CredManagerStore;
        if config.import_secrets_and_blank(&store) {
            // Best effort, same as the "repaired" write-back below: the
            // in-memory value is already correct even if this fails.
            let _ = config.save_to(&path);
        }
        config.hydrate_secrets(&store);
        config.apply_env_overrides();
        Ok(config)
    }

    /// Same as [`Config::load`] but against an arbitrary path, and without
    /// any Credential Manager access — the filesystem-touching core of
    /// `load()`, factored out so it can be exercised in tests against a
    /// scratch directory instead of the real `%APPDATA%`, with no risk of a
    /// test key reaching the real store. Only tests call this directly
    /// (`load()` has its own store-aware copy of this same sequence); kept
    /// `pub` as the documented store-free entry point for exactly that use.
    #[allow(dead_code)]
    pub fn load_from(path: &Path) -> Result<Config> {
        let mut config = Self::load_from_file(path)?;
        config.apply_env_overrides();
        Ok(config)
    }

    /// The file-only core shared by [`Config::load`] and
    /// [`Config::load_from`]: parse-or-default, repair-and-write-back, or
    /// create-from-defaults. Never touches env vars or the secret store.
    fn load_from_file(path: &Path) -> Result<Config> {
        if path.exists() {
            let contents = fs::read_to_string(path).unwrap_or_default();
            let (cfg, repaired) = Self::parse_reporting_repair(&contents);
            if repaired {
                // Best effort: the in-memory value is already correct, so a
                // failed write here is not worth failing the load over.
                let _ = cfg.save_to(path);
            }
            Ok(cfg)
        } else {
            let defaults = Config::default();
            defaults.save_to(path)?;
            Ok(defaults)
        }
    }

    /// Moves any non-empty `api_key` out of `self` and into `store` (target
    /// `Wingman/<provider>`), blanking the field here in memory. Returns
    /// whether anything was blanked, so the caller knows whether the file
    /// needs rewriting. Idempotent per provider: once a field is empty
    /// (already imported, or never set), calling this again is a no-op for
    /// it. If the store write fails, the key is left in place rather than
    /// blanked, so the next load retries the import instead of losing it.
    fn import_secrets_and_blank(&mut self, store: &dyn SecretStore) -> bool {
        let mut changed = false;
        for (provider, key) in [
            ("openai", &mut self.providers.openai.api_key),
            ("anthropic", &mut self.providers.anthropic.api_key),
            ("gemini", &mut self.providers.gemini.api_key),
        ] {
            if key.is_empty() {
                continue;
            }
            if store.set(&target_name(provider), key).is_ok() {
                key.clear();
                changed = true;
            }
        }
        // #16: same import-and-blank lifecycle for every configured compat
        // endpoint, keyed `Wingman/compat:<name>` (see `compat_order_name`).
        for entry in &mut self.providers.compat {
            if entry.api_key.is_empty() {
                continue;
            }
            let target = target_name(&compat_order_name(&entry.name));
            if store.set(&target, &entry.api_key).is_ok() {
                entry.api_key.clear();
                changed = true;
            }
        }
        changed
    }

    /// Fills any still-empty `api_key` field from `store`. Called after
    /// [`Config::import_secrets_and_blank`] so a freshly imported key
    /// round-trips straight back in, and on every load so a key that lives
    /// only in the store still reaches the providers. Never overwrites a
    /// non-empty field (in particular, one an env override already set),
    /// which is what keeps env vars taking precedence.
    ///
    /// Tri-state per #175, matching [`SecretStore::get`]'s own contract:
    /// `Ok(None)` (absent) leaves the field blank, exactly as before;
    /// `Ok(Some(secret))` fills it; `Err` (unreadable -- a transient
    /// `CredReadW` failure, or an undecodable blob) writes
    /// [`UNREADABLE_KEY_MARKER`] instead of leaving the field blank, and
    /// records the provider in [`Config::unreadable_secrets`]. The old code
    /// only matched `Ok(Some(_))`, so `Err` fell through to "leave blank" --
    /// indistinguishable from "never had a key" -- and the next `save()`
    /// deleted the credential this hydrate could not even read.
    fn hydrate_secrets(&mut self, store: &dyn SecretStore) {
        for (provider, key) in [
            ("openai", &mut self.providers.openai.api_key),
            ("anthropic", &mut self.providers.anthropic.api_key),
            ("gemini", &mut self.providers.gemini.api_key),
        ] {
            if !key.is_empty() {
                continue;
            }
            match store.get(&target_name(provider)) {
                Ok(Some(secret)) => *key = secret,
                Ok(None) => {}
                Err(_) => *key = UNREADABLE_KEY_MARKER.to_string(),
            }
        }
        for entry in &mut self.providers.compat {
            if !entry.api_key.is_empty() {
                continue;
            }
            let target = target_name(&compat_order_name(&entry.name));
            match store.get(&target) {
                Ok(Some(secret)) => entry.api_key = secret,
                Ok(None) => {}
                Err(_) => entry.api_key = UNREADABLE_KEY_MARKER.to_string(),
            }
        }

        for (provider, key) in [
            ("openai", &self.providers.openai.api_key),
            ("anthropic", &self.providers.anthropic.api_key),
            ("gemini", &self.providers.gemini.api_key),
        ] {
            if key == UNREADABLE_KEY_MARKER {
                self.unreadable_secrets.push(provider.to_string());
            }
        }
        for entry in &self.providers.compat {
            if entry.api_key == UNREADABLE_KEY_MARKER {
                self.unreadable_secrets.push(compat_order_name(&entry.name));
            }
        }
    }

    /// The env var name each provider's key can be overridden by, mirroring
    /// [`Config::apply_env_overrides`].
    fn env_var_name(provider: &str) -> Option<&'static str> {
        match provider {
            "openai" => Some("OPENAI_API_KEY"),
            "anthropic" => Some("ANTHROPIC_API_KEY"),
            "gemini" => Some("GEMINI_API_KEY"),
            _ => None,
        }
    }

    /// The save-path reconciler `Config::save` uses to keep the store in
    /// sync with `self` before writing the (always key-free) file. Distinct
    /// from [`Config::import_secrets_and_blank`], which is load-only and
    /// file-sourced: by the time `save` runs, `self`'s fields already carry
    /// live, hydrated values (or an env override, or a deliberate clear),
    /// so this needs different rules per provider:
    ///
    /// - **Env-overridden** (`env_is_set` returns true for its var name):
    ///   skipped entirely -- the store is left exactly as it was, so a
    ///   temporary env var for one run can never become a permanent
    ///   Credential Manager entry just because *something* called `save()`
    ///   while it was set. The field is still blanked so the file never
    ///   shows it.
    /// - **Non-empty, not overridden:** written to the store; a failed
    ///   write returns `Err` instead of leaving the key in place for the
    ///   caller to serialize to disk (rule 7: the caller shows a failure
    ///   card, never a silent plaintext fallback).
    /// - **Empty, not overridden:** any existing credential is deleted.
    ///   This is what makes clearing a key field in Settings and saving
    ///   actually remove it -- without this, the field reaching `save()`
    ///   empty was indistinguishable from "never had a key", the old
    ///   credential stayed put, and the next `hydrate_secrets` silently put
    ///   it straight back.
    /// - **Still [`UNREADABLE_KEY_MARKER`] (#175):** the field carries
    ///   [`Config::hydrate_secrets`]'s marker for a credential that could
    ///   not be read, untouched by the user (a real key or a deliberate
    ///   clear would have overwritten it with something else). The store is
    ///   left exactly as it was -- neither deleted nor overwritten -- so a
    ///   save the user never intended for this field can never destroy a
    ///   credential this build simply could not read back. The field is
    ///   still blanked so the marker itself never reaches config.toml.
    ///
    /// `env_is_set` is injected (rather than calling `std::env::var`
    /// directly) so tests exercise this without touching real process env.
    fn push_secrets_to_store(
        &mut self,
        store: &dyn SecretStore,
        env_is_set: &dyn Fn(&str) -> bool,
    ) -> Result<()> {
        for (provider, key) in [
            ("openai", &mut self.providers.openai.api_key),
            ("anthropic", &mut self.providers.anthropic.api_key),
            ("gemini", &mut self.providers.gemini.api_key),
        ] {
            let env_name = Self::env_var_name(provider)
                .expect("every provider iterated here has an env var name");
            if env_is_set(env_name) {
                key.clear();
                continue;
            }

            let target = target_name(provider);
            if key == UNREADABLE_KEY_MARKER {
                key.clear();
                continue;
            }
            if key.is_empty() {
                store
                    .delete(&target)
                    .with_context(|| format!("failed to delete {target} from the secret store"))?;
            } else {
                store
                    .set(&target, key)
                    .with_context(|| format!("failed to save {target} to the secret store"))?;
                key.clear();
            }
        }

        // #16: no env override for compat entries (a dynamic, user-named
        // list has no single fixed env var to check), so every entry goes
        // straight through the non-env-overridden rules above.
        for entry in &mut self.providers.compat {
            let target = target_name(&compat_order_name(&entry.name));
            if entry.api_key == UNREADABLE_KEY_MARKER {
                entry.api_key.clear();
                continue;
            }
            if entry.api_key.is_empty() {
                store
                    .delete(&target)
                    .with_context(|| format!("failed to delete {target} from the secret store"))?;
            } else {
                store
                    .set(&target, &entry.api_key)
                    .with_context(|| format!("failed to save {target} to the secret store"))?;
                entry.api_key.clear();
            }
        }
        Ok(())
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
        if self.providers.gemini.models.is_empty() {
            self.providers.gemini.models = d.gemini.models;
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
        if let Ok(key) = std::env::var("GEMINI_API_KEY") {
            self.providers.gemini.api_key = key;
        }
    }

    /// Writes the config to the well-known path. Any live, non-env-sourced
    /// `api_key` is pushed to the real Credential Manager store first (#2)
    /// and never reaches the file -- see [`Config::push_secrets_to_store`].
    /// This only blanks the copy that gets serialized; `self` keeps the
    /// real key the caller already has, so e.g. `self.build_chain()` right
    /// after `save()` still works.
    ///
    /// On a store failure this returns `Err` *before* touching the file, so
    /// a transient Credential Manager error can never fall back to writing
    /// the real key to disk (rule 7: the caller shows a failure card).
    ///
    /// Like [`Config::load`], only this entry point touches the real store;
    /// [`Config::save_to`] stays store-free for the same reason
    /// [`Config::load_from`] does (rule 9).
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let mut on_disk = self.clone();
        on_disk.push_secrets_to_store(&CredManagerStore, &|name| std::env::var(name).is_ok())?;
        on_disk.save_to(&path)
    }

    /// Same as [`Config::save`] but against an arbitrary path, and without
    /// any Credential Manager access -- whatever `api_key` is already on
    /// `self` is what gets written. Writes atomically (temp file + rename)
    /// so a reader never observes a half-written file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("failed to create config directory")?;
        }
        let toml_str = toml::to_string_pretty(self).context("failed to serialize config")?;

        let mut tmp_name = path.as_os_str().to_os_string();
        tmp_name.push(".tmp");
        let tmp_path = PathBuf::from(tmp_name);
        fs::write(&tmp_path, &toml_str).context("failed to write config temp file")?;
        fs::rename(&tmp_path, path).context("failed to move config temp file into place")?;

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

    /// Builds the provider fallback chain per `providers.order`, ignoring
    /// `mode` entirely. Providers not named in `order` are omitted
    /// entirely; unrecognized names are ignored. Providers with an empty
    /// API key are still included in the chain but report themselves
    /// not-`ready()`, so `Chain::ask` skips them without failing.
    ///
    /// Used for the cheap "is anything configured at all" gate
    /// (`App::ask`'s "No API key" card) and by the tray tooltip -- neither
    /// needs mode filtering, since an empty *unfiltered* chain means an
    /// empty chain under every mode too. The actual per-press request goes
    /// through [`Providers::build_chain_for_mode`] instead.
    pub fn build_chain(&self) -> Chain {
        self.providers.build_chain()
    }
}

/// #175: the unreadable-credential marker is a placeholder, not a key; a
/// provider built from it must report not ready instead of sending it.
fn unreadable_as_empty(key: &str) -> String {
    if key == UNREADABLE_KEY_MARKER {
        String::new()
    } else {
        key.to_string()
    }
}

impl Providers {
    /// Constructs the `Provider` for one `providers.order` name against
    /// `self`'s per-provider config, or `None` for an unrecognized name.
    /// The one name-to-provider mapping [`Providers::build_chain`] and
    /// [`Providers::build_chain_for_mode`] (#19) both build from, so the
    /// two can never drift apart from each other (see the
    /// `wired-to-nothing` skill's "hard-coded list" row).
    fn provider_for(&self, name: &str) -> Option<Box<dyn Provider>> {
        match name {
            "openai" => Some(Box::new(OpenAi::new(
                unreadable_as_empty(&self.openai.api_key),
                self.openai.model.clone(),
                self.openai.effort.clone(),
            ))),
            "anthropic" => Some(Box::new(Anthropic::new(
                unreadable_as_empty(&self.anthropic.api_key),
                self.anthropic.model.clone(),
                self.anthropic.effort.clone(),
            ))),
            "gemini" => Some(Box::new(Gemini::new(
                unreadable_as_empty(&self.gemini.api_key),
                self.gemini.model.clone(),
                self.gemini.effort.clone(),
            ))),
            "ollama" => Some(Box::new(Ollama::new(
                self.ollama.base_url.clone(),
                self.ollama.model.clone(),
                self.ollama.effort.clone(),
            ))),
            _ => {
                // #16: `"compat:<name>"` order entries resolve against
                // `self.compat` by `name`, not by position -- an entry
                // reordered or removed in `providers.order` simply
                // disappears from the chain, same as an unrecognized name.
                let compat_name = name.strip_prefix("compat:")?;
                let cfg = self.compat.iter().find(|c| c.name == compat_name)?;
                Some(Box::new(OpenAiCompat::new(
                    cfg.base_url.clone(),
                    cfg.model.clone(),
                    cfg.auth,
                    cfg.auth_header.clone(),
                    unreadable_as_empty(&cfg.api_key),
                    cfg.structured,
                )))
            }
        }
    }

    /// [`Config::build_chain`]'s implementation, kept here (rather than
    /// needing a whole `Config`) so [`App::ask`]'s worker thread (`app.rs`,
    /// issue #19) can build a chain from just the cloned `Providers` it
    /// carries across the thread boundary, without also carrying
    /// `hotkeys`/`capture`/`ui` along for no reason.
    pub fn build_chain(&self) -> Chain {
        let providers: Vec<Box<dyn Provider>> = self
            .order
            .iter()
            .filter_map(|name| self.provider_for(name))
            .collect();
        Chain::new(providers)
    }

    /// Issue #19: like [`Providers::build_chain`], but `order` is first
    /// filtered and reordered by `mode::select_providers` for `mode`.
    /// `ollama_ready` is the caller's already-computed answer to "is Ollama
    /// reachable with the model loaded, right now" (see
    /// `mode::should_probe_ollama` / `mode::probe_ollama_ready`) -- this
    /// function itself does no network I/O, so it is cheap enough to call
    /// fresh on every press rather than caching a chain across presses
    /// (which is exactly what Auto mode needs: Ollama's reachability can
    /// change between two presses, and rule 5 rules out polling to keep a
    /// cached answer warm).
    pub fn build_chain_for_mode(&self, mode: Mode, ollama_ready: bool) -> Chain {
        // #16: a compat endpoint's locality is decided by its configured
        // `base_url` host, never by name (`mode::is_local_provider`'s doc) --
        // this is the one place that host check happens, right before
        // `select_providers` needs the answer.
        let local_compat_names: Vec<String> = self
            .compat
            .iter()
            .filter(|c| {
                matches!(
                    crate::mode::classify_host(&c.base_url),
                    crate::mode::HostClass::Loopback
                )
            })
            .map(|c| compat_order_name(&c.name))
            .collect();
        let selected =
            crate::mode::select_providers(mode, &self.order, ollama_ready, &local_compat_names);
        let providers: Vec<Box<dyn Provider>> = selected
            .iter()
            .filter_map(|name| self.provider_for(name))
            .collect();
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
        assert!(!config.ui.show_difficulty);
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
    fn build_chain_never_hands_the_unreadable_marker_to_a_provider() {
        // #175's marker keeps an unreadable credential from being deleted on
        // save; it is not a key. A provider built from it must report not
        // ready rather than send the marker as an API key.
        let mut config = Config::default();
        config.providers.order = vec!["openai".to_string(), "anthropic".to_string()];
        config.providers.openai.api_key = UNREADABLE_KEY_MARKER.to_string();
        config.providers.anthropic.api_key = UNREADABLE_KEY_MARKER.to_string();
        assert!(config.build_chain().ready_provider_names().is_empty());
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

    // -- #19: Mode -------------------------------------------------------

    #[test]
    fn default_mode_is_auto() {
        assert_eq!(Config::default().mode, Mode::Auto);
    }

    #[test]
    fn mode_persists_through_a_toml_round_trip() {
        for mode in [Mode::Cloud, Mode::Local, Mode::Auto, Mode::Offline] {
            let config = Config {
                mode,
                ..Config::default()
            };
            let text = toml::to_string_pretty(&config).expect("serialize");
            assert!(
                text.contains(&format!("mode = \"{}\"", mode_wire_name(mode))),
                "{text}"
            );
            let parsed: Config = toml::from_str(&text).expect("deserialize");
            assert_eq!(parsed.mode, mode, "round trip failed for {mode:?}");
        }
    }

    #[test]
    fn an_older_config_missing_the_mode_key_backfills_to_auto() {
        // A config.toml written before #19 has no `mode` key at all;
        // #[serde(default)] on `Config` must still produce `Auto`, not a
        // parse failure or a bogus default.
        let old = r#"
[providers.openai]
model = "gpt-5.5"
"#;
        let cfg = Config::parse_or_default(old);
        assert_eq!(cfg.mode, Mode::Auto);
    }

    // -- #17: Gemini --------------------------------------------------------

    #[test]
    fn gemini_default_is_not_in_the_default_order() {
        // Unlike openai/anthropic, gemini is not yet reachable from the
        // Win32 settings dialog at all: the Active provider control is a
        // two-way openai/anthropic radio with no third option. #194 fixed
        // `build_config` (now `merge_provider_order`) so a hand-added
        // gemini entry survives every Settings save untouched -- so it
        // stays opt-in, like ollama (#13), only until Settings can
        // represent a third cloud provider (#51), not because saving would
        // delete it.
        let config = Config::default();
        assert!(!config.providers.order.contains(&"gemini".to_string()));
        assert_eq!(config.providers.order, vec!["openai", "anthropic"]);
    }

    #[test]
    fn gemini_default_has_a_model_and_a_nonempty_model_list() {
        let config = Config::default();
        assert_eq!(config.providers.gemini.model, "gemini-3.8-flash");
        assert!(!config.providers.gemini.models.is_empty());
        assert!(config
            .providers
            .gemini
            .models
            .contains(&"gemini-3.8-flash".to_string()));
        assert_eq!(config.providers.gemini.api_key, "");
    }

    #[test]
    fn build_chain_includes_gemini_only_when_explicitly_ordered() {
        let mut config = Config::default();
        config.providers.order = vec!["openai".to_string(), "gemini".to_string()];
        config.providers.gemini.api_key = "AIza-real".to_string();
        let chain = config.build_chain();
        assert_eq!(chain.provider_names(), vec!["openai", "gemini"]);
        // Gemini needs a key to be ready -- unlike ollama.
        assert_eq!(chain.ready_provider_names(), vec!["gemini"]);
    }

    #[test]
    fn build_chain_never_hands_the_unreadable_marker_to_gemini() {
        let mut config = Config::default();
        config.providers.order = vec!["gemini".to_string()];
        config.providers.gemini.api_key = UNREADABLE_KEY_MARKER.to_string();
        assert!(config.build_chain().ready_provider_names().is_empty());
    }

    #[test]
    fn a_gemini_section_missing_from_an_older_config_backfills_to_defaults() {
        let old = r#"
[providers.openai]
model = "gpt-5.5"
"#;
        let cfg = Config::parse_or_default(old);
        assert_eq!(cfg.providers.gemini.model, "gemini-3.8-flash");
        assert!(!cfg.providers.gemini.models.is_empty());
    }

    fn mode_wire_name(m: Mode) -> &'static str {
        match m {
            Mode::Cloud => "cloud",
            Mode::Local => "local",
            Mode::Auto => "auto",
            Mode::Offline => "offline",
        }
    }

    #[test]
    fn build_chain_for_mode_cloud_excludes_ollama() {
        let mut config = Config::default();
        config.providers.order = vec![
            "openai".to_string(),
            "anthropic".to_string(),
            "ollama".to_string(),
        ];
        let chain = config.providers.build_chain_for_mode(Mode::Cloud, true);
        assert_eq!(chain.provider_names(), vec!["openai", "anthropic"]);
    }

    #[test]
    fn build_chain_for_mode_local_is_ollama_only() {
        let mut config = Config::default();
        config.providers.order = vec![
            "openai".to_string(),
            "anthropic".to_string(),
            "ollama".to_string(),
        ];
        let chain = config.providers.build_chain_for_mode(Mode::Local, false);
        assert_eq!(chain.provider_names(), vec!["ollama"]);
    }

    #[test]
    fn build_chain_for_mode_offline_is_ollama_only() {
        let mut config = Config::default();
        config.providers.order = vec![
            "openai".to_string(),
            "anthropic".to_string(),
            "ollama".to_string(),
        ];
        let chain = config.providers.build_chain_for_mode(Mode::Offline, false);
        assert_eq!(chain.provider_names(), vec!["ollama"]);
    }

    #[test]
    fn build_chain_for_mode_auto_puts_ollama_first_only_when_ready() {
        let mut config = Config::default();
        config.providers.order = vec![
            "openai".to_string(),
            "anthropic".to_string(),
            "ollama".to_string(),
        ];

        let ready = config.providers.build_chain_for_mode(Mode::Auto, true);
        assert_eq!(
            ready.provider_names(),
            vec!["ollama", "openai", "anthropic"]
        );

        let not_ready = config.providers.build_chain_for_mode(Mode::Auto, false);
        assert_eq!(not_ready.provider_names(), vec!["openai", "anthropic"]);
    }

    #[test]
    fn build_chain_and_build_chain_for_mode_agree_when_every_provider_is_cloud() {
        // With no local provider configured at all, mode filtering has
        // nothing to remove -- both builders must produce the exact same
        // chain for every mode, proving `provider_for` never drifted
        // between the two call sites.
        let config = Config::default(); // order: ["openai", "anthropic"], no ollama
        let unfiltered = config.build_chain().provider_names();
        for mode in [Mode::Cloud, Mode::Auto] {
            assert_eq!(
                config
                    .providers
                    .build_chain_for_mode(mode, true)
                    .provider_names(),
                unfiltered,
                "mode {mode:?}"
            );
        }
    }

    // -- #16: OpenAI-compatible generic providers -------------------------

    fn compat_entry(name: &str, base_url: &str) -> CompatConfig {
        CompatConfig {
            name: name.to_string(),
            base_url: base_url.to_string(),
            auth: CompatAuth::Bearer,
            auth_header: String::new(),
            api_key: "sk-compat-real".to_string(),
            model: "some-model".to_string(),
            models: vec!["some-model".to_string()],
            structured: Structured::JsonSchema,
        }
    }

    #[test]
    fn compat_order_name_is_compat_colon_name() {
        assert_eq!(compat_order_name("openrouter"), "compat:openrouter");
    }

    #[test]
    fn build_chain_includes_a_compat_provider_named_in_order() {
        let mut config = Config::default();
        config.providers.compat = vec![compat_entry("openrouter", "https://openrouter.ai/api/v1")];
        config.providers.order = vec!["openai".to_string(), "compat:openrouter".to_string()];
        let chain = config.build_chain();
        assert_eq!(chain.provider_names(), vec!["openai", "openai-compat"]);
        // Has a key configured -> ready.
        assert_eq!(chain.ready_provider_names(), vec!["openai-compat"]);
    }

    #[test]
    fn build_chain_drops_a_compat_order_entry_with_no_matching_config() {
        // "compat:ghost" names no entry in `providers.compat` -- dropped
        // silently, same as any other unrecognized `providers.order` name
        // (`build_chain_ignores_unknown_provider_names`'s neighbour).
        let mut config = Config::default();
        config.providers.order = vec!["openai".to_string(), "compat:ghost".to_string()];
        let chain = config.build_chain();
        assert_eq!(chain.provider_names(), vec!["openai"]);
    }

    #[test]
    fn build_chain_marks_a_keyless_bearer_compat_provider_not_ready() {
        let mut config = Config::default();
        let mut entry = compat_entry("lmstudio", "http://127.0.0.1:1234/v1");
        entry.api_key.clear();
        config.providers.compat = vec![entry];
        config.providers.order = vec!["compat:lmstudio".to_string()];
        let chain = config.build_chain();
        assert!(chain.ready_provider_names().is_empty());
    }

    /// The classification the task requires: a compat endpoint on a
    /// loopback host counts as local, a remote one as cloud -- decided by
    /// `base_url` via `mode::classify_host`, never by name.
    #[test]
    fn build_chain_for_mode_classifies_a_compat_provider_by_its_base_url_host() {
        let mut config = Config::default();
        config.providers.compat = vec![
            compat_entry("lmstudio", "http://127.0.0.1:1234/v1"),
            compat_entry("openrouter", "https://openrouter.ai/api/v1"),
        ];
        config.providers.order = vec![
            "openai".to_string(),
            "compat:lmstudio".to_string(),
            "compat:openrouter".to_string(),
        ];

        let local = config.providers.build_chain_for_mode(Mode::Local, false);
        assert_eq!(
            local.provider_names(),
            vec!["openai-compat"],
            "only the loopback entry is local"
        );

        let cloud = config.providers.build_chain_for_mode(Mode::Cloud, false);
        // Both resolve to the same `id()` ("openai-compat"); count instead.
        assert_eq!(
            cloud.provider_names().len(),
            2,
            "openai + the remote compat entry"
        );
    }

    #[test]
    fn build_chain_for_mode_auto_puts_a_local_compat_provider_first_alongside_ollama() {
        let mut config = Config::default();
        config.providers.ollama.model = "gemma3:4b".to_string();
        config.providers.compat = vec![compat_entry("lmstudio", "http://127.0.0.1:1234/v1")];
        config.providers.order = vec![
            "openai".to_string(),
            "ollama".to_string(),
            "compat:lmstudio".to_string(),
        ];

        let chain = config.providers.build_chain_for_mode(Mode::Auto, true);
        // ollama, then the local compat entry, then cloud -- both local
        // providers precede openai.
        let names = chain.provider_names();
        assert_eq!(names, vec!["ollama", "openai-compat", "openai"]);
    }

    #[test]
    fn compat_config_debug_never_contains_the_api_key() {
        let entry = compat_entry("openrouter", "https://openrouter.ai/api/v1");
        let debug_output = format!("{entry:?}");
        assert!(!debug_output.contains("sk-compat-real"), "{debug_output}");
        assert!(debug_output.contains("redacted"));
    }

    #[test]
    fn compat_config_round_trips_through_toml() {
        let mut config = Config::default();
        config.providers.compat = vec![compat_entry("openrouter", "https://openrouter.ai/api/v1")];
        let text = toml::to_string_pretty(&config).expect("serialize");
        let parsed: Config = toml::from_str(&text).expect("deserialize");
        assert_eq!(parsed.providers.compat.len(), 1);
        assert_eq!(parsed.providers.compat[0].name, "openrouter");
        assert_eq!(parsed.providers.compat[0].auth, CompatAuth::Bearer);
        assert_eq!(
            parsed.providers.compat[0].structured,
            Structured::JsonSchema
        );
    }

    #[test]
    fn compat_auth_and_structured_serialize_to_the_documented_toml_strings() {
        // A bare enum has no top-level TOML text form (TOML documents are
        // always tables) -- `toml::Value::try_from` serializes through the
        // same serde path without that document requirement, so this still
        // exercises exactly what `#[serde(rename_all = ...)]` produces.
        assert_eq!(
            toml::Value::try_from(CompatAuth::Bearer).unwrap().as_str(),
            Some("bearer")
        );
        assert_eq!(
            toml::Value::try_from(CompatAuth::ApiKeyHeader)
                .unwrap()
                .as_str(),
            Some("api-key-header")
        );
        assert_eq!(
            toml::Value::try_from(CompatAuth::None).unwrap().as_str(),
            Some("none")
        );
        assert_eq!(
            toml::Value::try_from(Structured::JsonSchema)
                .unwrap()
                .as_str(),
            Some("json_schema")
        );
        assert_eq!(
            toml::Value::try_from(Structured::JsonObject)
                .unwrap()
                .as_str(),
            Some("json_object")
        );
        assert_eq!(
            toml::Value::try_from(Structured::Prompt).unwrap().as_str(),
            Some("prompt")
        );
    }

    // -- #16: compat secrets lifecycle (import / hydrate / push) -----------

    #[test]
    fn import_and_blank_moves_a_live_compat_key_into_the_store_and_blanks_the_field() {
        let mut config = Config::default();
        config.providers.compat = vec![compat_entry("openrouter", "https://openrouter.ai/api/v1")];
        let store = crate::secrets::InMemoryStore::default();

        let changed = config.import_secrets_and_blank(&store);
        assert!(changed);
        assert_eq!(config.providers.compat[0].api_key, "");
        assert_eq!(
            store.get("Wingman/compat:openrouter").unwrap().as_deref(),
            Some("sk-compat-real")
        );
    }

    #[test]
    fn hydrate_fills_an_empty_compat_field_from_the_store() {
        let mut config = Config::default();
        config.providers.compat = vec![compat_entry("openrouter", "https://openrouter.ai/api/v1")];
        config.providers.compat[0].api_key.clear();
        let store = crate::secrets::InMemoryStore::default();
        store
            .set("Wingman/compat:openrouter", "sk-from-store")
            .unwrap();

        config.hydrate_secrets(&store);
        assert_eq!(config.providers.compat[0].api_key, "sk-from-store");
    }

    #[test]
    fn hydrate_marks_a_compat_field_unreadable_when_the_store_get_errors() {
        let mut config = Config::default();
        config.providers.compat = vec![compat_entry("openrouter", "https://openrouter.ai/api/v1")];
        config.providers.compat[0].api_key.clear();
        let store = crate::secrets::InMemoryStore::default();
        store.poison("Wingman/compat:openrouter");

        config.hydrate_secrets(&store);
        assert_eq!(config.providers.compat[0].api_key, UNREADABLE_KEY_MARKER);
        assert!(config
            .unreadable_secrets
            .contains(&"compat:openrouter".to_string()));
    }

    #[test]
    fn push_secrets_to_store_saves_a_live_compat_key() {
        let mut config = Config::default();
        config.providers.compat = vec![compat_entry("openrouter", "https://openrouter.ai/api/v1")];
        let store = crate::secrets::InMemoryStore::default();

        config.push_secrets_to_store(&store, &|_| false).unwrap();
        assert_eq!(config.providers.compat[0].api_key, "");
        assert_eq!(
            store.get("Wingman/compat:openrouter").unwrap().as_deref(),
            Some("sk-compat-real")
        );
    }

    #[test]
    fn push_secrets_to_store_deletes_the_compat_credential_when_the_field_is_cleared() {
        let mut config = Config::default();
        config.providers.compat = vec![compat_entry("openrouter", "https://openrouter.ai/api/v1")];
        let store = crate::secrets::InMemoryStore::default();
        store.set("Wingman/compat:openrouter", "sk-old").unwrap();
        config.providers.compat[0].api_key.clear();

        config.push_secrets_to_store(&store, &|_| false).unwrap();
        assert_eq!(store.get("Wingman/compat:openrouter").unwrap(), None);
    }

    #[test]
    fn gemini_config_from_an_older_build_gets_its_model_list_backfilled() {
        let old = r#"
[providers.gemini]
model = "gemini-3.8-flash"
api_key = "AIza-x"
"#;
        let cfg = Config::parse_or_default(old);
        assert!(
            !cfg.providers.gemini.models.is_empty(),
            "an absent models list must be backfilled, not left empty"
        );
        assert_eq!(cfg.providers.gemini.api_key, "AIza-x");
    }

    #[test]
    fn a_customized_gemini_model_list_is_never_overwritten() {
        let custom = r#"
[providers.gemini]
models = ["gemini-2.5-pro"]
"#;
        let cfg = Config::parse_or_default(custom);
        assert_eq!(
            cfg.providers.gemini.models,
            vec!["gemini-2.5-pro".to_string()]
        );
    }

    #[test]
    fn gemini_env_var_overrides_the_file_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("GEMINI_API_KEY");
        std::env::set_var("GEMINI_API_KEY", "env-gemini-key");

        let toml_str = r#"
            [providers.gemini]
            api_key = "file-gemini-key"
        "#;
        let mut config = Config::parse_or_default(toml_str);
        assert_eq!(config.providers.gemini.api_key, "file-gemini-key");
        config.apply_env_overrides();
        assert_eq!(config.providers.gemini.api_key, "env-gemini-key");

        std::env::remove_var("GEMINI_API_KEY");
    }

    #[test]
    fn gemini_debug_format_never_contains_the_api_key() {
        let mut config = Config::default();
        config.providers.gemini.api_key = "AIza-real-secret-gemini".to_string();
        let debug_output = format!("{config:?}");
        assert!(
            !debug_output.contains("AIza-real-secret-gemini"),
            "{debug_output}"
        );
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
        let cfg = Config::parse_or_default(
            "[ui]
text_scale = 0.0
",
        );
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

    // -- #197: show_difficulty default flipped to false ---------------------

    #[test]
    fn default_show_difficulty_is_false() {
        // Owner decision 2026-09-17: the difficulty rating is being moved
        // out of the core app, so a fresh config must not request or pay
        // for the rubric unless the user opts in.
        assert!(!Config::default().ui.show_difficulty);
    }

    #[test]
    fn an_existing_config_with_show_difficulty_true_keeps_it_on_load() {
        // #[serde(default)] only fills a field in when the key is absent
        // from the TOML text. A config.toml written by an older build (or
        // hand-edited) with `show_difficulty = true` must not be silently
        // flipped off just because the shipped default changed.
        let toml_str = "[ui]\nshow_difficulty = true\n";
        let cfg = Config::parse_or_default(toml_str);
        assert!(cfg.ui.show_difficulty);
    }

    #[test]
    fn an_older_config_missing_show_difficulty_backfills_to_the_new_default() {
        let toml_str = "[capture]\nmax_edge = 999\n";
        let cfg = Config::parse_or_default(toml_str);
        assert!(!cfg.ui.show_difficulty);
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

        let migrated =
            Config::migrate_from(&old_path, &new_path).expect("migration should succeed");

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

        let migrated =
            Config::migrate_from(&old_path, &new_path).expect("migration should succeed");

        assert!(!migrated, "an existing new file must never be overwritten");
        assert_eq!(
            fs::read_to_string(&new_path).unwrap(),
            "new content, already set up"
        );

        cleanup(&root.join("dummy"));
    }

    #[test]
    fn migrate_does_nothing_when_the_old_file_is_absent() {
        let root = scratch_dir("no-old");
        let old_path = root.join("old").join("config.toml");
        let new_path = root.join("new").join("config.toml");
        assert!(!old_path.exists());
        assert!(!new_path.exists());

        let migrated =
            Config::migrate_from(&old_path, &new_path).expect("migration should succeed");

        assert!(
            !migrated,
            "nothing to migrate when there was never an old file"
        );
        assert!(
            !new_path.exists(),
            "no new file should be created out of nothing"
        );

        cleanup(&root.join("dummy"));
    }

    #[test]
    fn migrate_creates_the_new_config_directory() {
        // The new directory ("Wingman") never existed before the rename, so
        // migration must create it -- unlike Config::save, which is only ever
        // called after the directory has already been created once.
        let root = scratch_dir("mkdir");
        let old_path = root.join("old").join("config.toml");
        let new_path = root
            .join("brand-new-dir")
            .join("nested")
            .join("config.toml");
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
        assert_eq!(
            old.parent().unwrap().parent(),
            new.parent().unwrap().parent()
        );
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

    // -- #2: Credential Manager import/hydrate, against a stub store --------
    // Uses `secrets::InMemoryStore`, never `secrets::CredManagerStore`, so
    // these tests never touch the real `Wingman/<provider>` credentials
    // (rule 9). `Config::load`/`Config::save` (the only real-store entry
    // points) are not called anywhere in this module's tests.

    use crate::secrets::InMemoryStore;

    #[test]
    fn import_and_blank_moves_a_live_key_into_the_store_and_blanks_the_field() {
        let mut config = Config::default();
        config.providers.openai.api_key = "sk-legacy-in-file".to_string();
        let store = InMemoryStore::default();

        let changed = config.import_secrets_and_blank(&store);

        assert!(changed, "a live key must be reported as blanked");
        assert_eq!(
            config.providers.openai.api_key, "",
            "the field must be blanked in memory, not just on disk"
        );
        assert_eq!(
            store.get(&target_name("openai")).unwrap().as_deref(),
            Some("sk-legacy-in-file"),
            "the real value must have reached the store"
        );
    }

    #[test]
    fn import_and_blank_against_a_temp_config_file_never_writes_the_key_to_disk() {
        // The scenario issue #2 names directly: an upgrade finds an old
        // config.toml with a live key on disk.
        let path = scratch_path("secrets-import");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "[providers.openai]\napi_key = \"sk-legacy-in-file\"\n",
        )
        .unwrap();

        let mut config = Config::load_from(&path).expect("load should succeed");
        assert_eq!(config.providers.openai.api_key, "sk-legacy-in-file");

        let store = InMemoryStore::default();
        assert!(config.import_secrets_and_blank(&store));
        config.save_to(&path).expect("save should succeed");

        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("sk-legacy-in-file"),
            "the real key must never reach config.toml: {on_disk}"
        );
        assert!(
            on_disk.contains("api_key = \"\""),
            "the file must show a blank openai key: {on_disk}"
        );

        // The next load's blank-field hydrate step gets the key back from
        // the store, exactly like `Config::load()` does after `import`.
        let mut reloaded = Config::load_from(&path).expect("reload should succeed");
        assert_eq!(reloaded.providers.openai.api_key, "");
        reloaded.hydrate_secrets(&store);
        assert_eq!(reloaded.providers.openai.api_key, "sk-legacy-in-file");

        cleanup(&path);
    }

    #[test]
    fn import_and_blank_is_a_no_op_once_the_field_is_already_blank() {
        let mut config = Config::default(); // api_key fields start empty
        let store = InMemoryStore::default();

        let changed = config.import_secrets_and_blank(&store);

        assert!(
            !changed,
            "nothing to import must not be reported as a change"
        );
        assert_eq!(store.get(&target_name("openai")).unwrap(), None);
        assert_eq!(store.get(&target_name("anthropic")).unwrap(), None);
    }

    #[test]
    fn hydrate_fills_an_empty_field_from_the_store() {
        let mut config = Config::default();
        let store = InMemoryStore::default();
        store
            .set(&target_name("anthropic"), "sk-ant-from-store")
            .unwrap();

        config.hydrate_secrets(&store);

        assert_eq!(config.providers.anthropic.api_key, "sk-ant-from-store");
        assert_eq!(
            config.providers.openai.api_key, "",
            "a provider absent from the store must stay blank, not error"
        );
    }

    #[test]
    fn hydrate_never_overwrites_an_already_populated_field() {
        // This is what keeps env-var precedence intact: `Config::load()`
        // calls `hydrate_secrets` before `apply_env_overrides`, but if
        // something upstream already populated the field, hydrate must
        // leave it alone rather than clobbering it with the store's value.
        let mut config = Config::default();
        config.providers.openai.api_key = "already-set".to_string();
        let store = InMemoryStore::default();
        store.set(&target_name("openai"), "store-value").unwrap();

        config.hydrate_secrets(&store);

        assert_eq!(config.providers.openai.api_key, "already-set");
    }

    // -- #17: Gemini secrets, against the InMemoryStore stub ----------------

    #[test]
    fn import_and_blank_moves_a_live_gemini_key_into_the_store_and_blanks_the_field() {
        let mut config = Config::default();
        config.providers.gemini.api_key = "AIza-legacy-in-file".to_string();
        let store = InMemoryStore::default();

        let changed = config.import_secrets_and_blank(&store);

        assert!(changed);
        assert_eq!(config.providers.gemini.api_key, "");
        assert_eq!(
            store.get(&target_name("gemini")).unwrap().as_deref(),
            Some("AIza-legacy-in-file")
        );
    }

    #[test]
    fn hydrate_fills_an_empty_gemini_field_from_the_store() {
        let mut config = Config::default();
        let store = InMemoryStore::default();
        store
            .set(&target_name("gemini"), "AIza-from-store")
            .unwrap();

        config.hydrate_secrets(&store);

        assert_eq!(config.providers.gemini.api_key, "AIza-from-store");
    }

    #[test]
    fn hydrate_marks_gemini_unreadable_when_the_store_get_errors() {
        let store = InMemoryStore::default();
        store
            .set(&target_name("gemini"), "AIza-really-there")
            .unwrap();
        store.poison(&target_name("gemini"));

        let mut config = Config::default();
        config.hydrate_secrets(&store);

        assert_eq!(config.providers.gemini.api_key, UNREADABLE_KEY_MARKER);
        assert_eq!(config.unreadable_secrets, vec!["gemini".to_string()]);
    }

    // -- #175: hydrate/push must never delete a credential it could not read --

    #[test]
    fn hydrate_marks_the_field_unreadable_when_the_store_get_errors() {
        // Bug: `hydrate_secrets` only matched `Ok(Some(_))`, so an `Err`
        // (a transient CredReadW failure) fell through to "leave the field
        // blank" -- indistinguishable from "never had a key" (#175).
        let store = InMemoryStore::default();
        store
            .set(&target_name("openai"), "sk-really-there")
            .unwrap();
        store.poison(&target_name("openai"));

        let mut config = Config::default();
        config.hydrate_secrets(&store);

        assert_eq!(
            config.providers.openai.api_key, UNREADABLE_KEY_MARKER,
            "an unreadable credential must be marked, not left blank"
        );
        assert_eq!(
            config.unreadable_secrets,
            vec!["openai".to_string()],
            "the provider must be reported so a caller can show a card"
        );
    }

    #[test]
    fn hydrate_of_a_genuinely_absent_credential_stays_blank_and_unreported() {
        // The other half of the matrix: Ok(None) (no credential at all)
        // must NOT be confused with Err (a credential that exists but could
        // not be read).
        let store = InMemoryStore::default();
        let mut config = Config::default();

        config.hydrate_secrets(&store);

        assert_eq!(config.providers.openai.api_key, "");
        assert!(config.unreadable_secrets.is_empty());
    }

    /// Wraps [`InMemoryStore`], recording every `set`/`delete` target so a
    /// test can assert push_secrets_to_store issued NEITHER for an
    /// unreadable provider -- stronger than checking the end state, which a
    /// delete-then-reset could satisfy by accident.
    struct RecordingStore {
        inner: InMemoryStore,
        set_calls: std::sync::Mutex<Vec<String>>,
        delete_calls: std::sync::Mutex<Vec<String>>,
    }
    impl RecordingStore {
        fn new() -> Self {
            Self {
                inner: InMemoryStore::default(),
                set_calls: std::sync::Mutex::new(Vec::new()),
                delete_calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    impl SecretStore for RecordingStore {
        fn get(&self, target: &str) -> Result<Option<String>> {
            self.inner.get(target)
        }
        fn set(&self, target: &str, secret: &str) -> Result<()> {
            self.set_calls.lock().unwrap().push(target.to_string());
            self.inner.set(target, secret)
        }
        fn delete(&self, target: &str) -> Result<()> {
            self.delete_calls.lock().unwrap().push(target.to_string());
            self.inner.delete(target)
        }
    }

    #[test]
    fn push_secrets_to_store_never_deletes_a_credential_it_could_not_read() {
        // The exact scenario #175 names: hydrate fails to read a real,
        // still-present credential, then something calls save() (pick_model,
        // set_provider, on_learned, open_settings, edit_settings all do).
        // Recording set/delete calls is a stronger check than the end
        // state: a delete-then-recreate could satisfy an end-state-only
        // assertion by accident.
        let store = RecordingStore::new();
        store
            .inner
            .set(&target_name("openai"), "sk-must-survive")
            .unwrap();
        store.inner.poison(&target_name("openai"));

        let mut config = Config::default();
        config.hydrate_secrets(&store);
        assert_eq!(config.providers.openai.api_key, UNREADABLE_KEY_MARKER);

        config.push_secrets_to_store(&store, &|_| false).unwrap();

        // anthropic legitimately gets a `delete` call here too (it has no
        // key, and that is the normal, correct empty-field path -- see
        // `push_secrets_to_store_leaves_an_untouched_key_alone`), so the
        // assertion below targets the openai entry specifically.
        let openai_target = target_name("openai");
        assert!(
            !store.delete_calls.lock().unwrap().contains(&openai_target),
            "an unreadable credential must never be deleted"
        );
        assert!(
            !store.set_calls.lock().unwrap().contains(&openai_target),
            "an unreadable credential must never be overwritten either"
        );
        assert_eq!(
            config.providers.openai.api_key, "",
            "the field must still be blanked before the disk write, so the marker never reaches config.toml"
        );
    }

    #[test]
    fn push_secrets_to_store_leaves_the_stored_credential_readable_after_an_unreadable_push() {
        // A store that was never poisoned in the first place is the direct
        // proof that push's marker path issues no delete/set call at all:
        // reading it back afterwards (through a fresh, unpoisoned target)
        // still finds it.
        let store = InMemoryStore::default();
        store
            .set(&target_name("anthropic"), "sk-must-survive")
            .unwrap();
        store.poison(&target_name("openai")); // a DIFFERENT provider is unreadable

        let mut config = Config::default();
        config.hydrate_secrets(&store);
        assert_eq!(config.providers.openai.api_key, UNREADABLE_KEY_MARKER);
        assert_eq!(config.providers.anthropic.api_key, "sk-must-survive");

        config.push_secrets_to_store(&store, &|_| false).unwrap();

        assert_eq!(
            store.get(&target_name("anthropic")).unwrap().as_deref(),
            Some("sk-must-survive"),
            "anthropic's own hydrated key must still reach the store normally"
        );
    }

    #[test]
    fn push_secrets_to_store_lets_a_retyped_key_replace_an_unreadable_marker() {
        // The user CAN still fix an unreadable credential by typing a new
        // key over it in Settings -- resolve_key_field (ui/settings.rs)
        // replaces the marker with whatever was typed, so by the time
        // push_secrets_to_store runs the field is a real key, not the
        // marker, and the normal set path must run.
        let store = InMemoryStore::default();
        let mut config = Config::default();
        config.providers.openai.api_key = "sk-brand-new".to_string();

        config.push_secrets_to_store(&store, &|_| false).unwrap();

        assert_eq!(
            store.get(&target_name("openai")).unwrap().as_deref(),
            Some("sk-brand-new")
        );
    }

    #[test]
    fn push_secrets_to_store_lets_the_user_clear_an_unreadable_marker() {
        // Typing nothing (clearing the field) over an unreadable marker is a
        // deliberate "remove this key" -- not the marker anymore, so the
        // ordinary empty-field delete path must run, same as any other
        // clear.
        let store = InMemoryStore::default();
        store
            .set(&target_name("openai"), "sk-old-and-unreadable")
            .unwrap();

        let mut config = Config::default();
        config.providers.openai.api_key = String::new(); // as resolve_key_field would set it

        config.push_secrets_to_store(&store, &|_| false).unwrap();

        assert_eq!(store.get(&target_name("openai")).unwrap(), None);
    }

    #[test]
    fn an_unreadable_marker_never_reaches_the_saved_file() {
        let path = scratch_path("secrets-unreadable-marker-never-on-disk");
        let store = InMemoryStore::default();
        store.set(&target_name("openai"), "sk-hidden").unwrap();
        store.poison(&target_name("openai"));

        let mut config = Config::default();
        config.hydrate_secrets(&store);
        config.push_secrets_to_store(&store, &|_| false).unwrap();
        config.save_to(&path).unwrap();

        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("wingman-credential-unreadable"),
            "the marker must never reach config.toml: {on_disk}"
        );
        assert!(on_disk.contains("api_key = \"\""));

        cleanup(&path);
    }

    #[test]
    fn env_override_wins_over_a_hydrated_store_value() {
        // Mirrors `Config::load()`'s exact ordering: hydrate, then
        // apply_env_overrides. "Env vars still override" (#2) means env
        // must win even when the store has a key.
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENAI_API_KEY", "env-wins");

        let mut config = Config::default();
        let store = InMemoryStore::default();
        store.set(&target_name("openai"), "store-value").unwrap();

        config.hydrate_secrets(&store);
        assert_eq!(config.providers.openai.api_key, "store-value");

        config.apply_env_overrides();
        assert_eq!(
            config.providers.openai.api_key, "env-wins",
            "env must win over a hydrated store value"
        );

        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    fn save_to_never_touches_the_store_only_save_does() {
        // save_to is the store-free core `load_from`'s sibling relies on
        // (rule 9): saving a live key through it must write that key
        // straight to disk, unlike `Config::save()`.
        let path = scratch_path("secrets-save-to-is-store-free");
        let mut config = Config::default();
        config.providers.openai.api_key = "sk-goes-straight-to-disk".to_string();

        config.save_to(&path).expect("save_to should succeed");

        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("sk-goes-straight-to-disk"),
            "save_to must not blank or redirect the key -- only Config::save does: {on_disk}"
        );

        cleanup(&path);
    }

    #[test]
    fn save_to_writes_atomically_leaving_no_temp_file_behind() {
        let path = scratch_path("secrets-atomic-save");
        let config = Config::default();

        config.save_to(&path).expect("save_to should succeed");

        assert!(path.exists());
        let mut tmp_name = path.as_os_str().to_os_string();
        tmp_name.push(".tmp");
        assert!(
            !PathBuf::from(tmp_name).exists(),
            "the temp file must be renamed away, not left behind"
        );

        cleanup(&path);
    }

    // -- #2 orchestrator review: push_secrets_to_store (the save-path -----
    // reconciler used by `Config::save`, distinct from `import_secrets_and_
    // blank`'s load-path, file-only semantics). Never mutates real process
    // env; the env check is an injected closure throughout.

    /// A [`SecretStore`] whose `set`/`delete` always fail, to prove a save
    /// never falls back to writing a real key to disk when the store is
    /// unreachable.
    struct FailingStore;
    impl SecretStore for FailingStore {
        fn get(&self, _target: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn set(&self, target: &str, _secret: &str) -> Result<()> {
            anyhow::bail!("simulated Credential Manager failure writing {target}")
        }
        fn delete(&self, target: &str) -> Result<()> {
            anyhow::bail!("simulated Credential Manager failure deleting {target}")
        }
    }

    #[test]
    fn push_secrets_to_store_deletes_the_credential_when_the_field_is_cleared() {
        // Bug: a cleared field was silently skipped by import_secrets_and_
        // blank (it only acts on non-empty fields), so the old credential
        // stayed in the store and the next hydrate put it straight back.
        let store = InMemoryStore::default();
        store
            .set(&target_name("openai"), "sk-should-be-removed")
            .unwrap();

        let mut config = Config::default(); // field already resolved to "" (user cleared it)
        config.push_secrets_to_store(&store, &|_| false).unwrap();

        assert_eq!(
            store.get(&target_name("openai")).unwrap(),
            None,
            "clearing the field and saving must delete the stored credential"
        );
    }

    #[test]
    fn clearing_a_key_and_saving_stops_the_next_hydrate_from_reviving_it() {
        let store = InMemoryStore::default();
        store.set(&target_name("openai"), "sk-old").unwrap();

        // As if Config::load() had hydrated this key, then the user cleared
        // the field in Settings (resolve_key_field already covers that part).
        let mut config = Config::default();
        config.providers.openai.api_key = String::new();

        config.push_secrets_to_store(&store, &|_| false).unwrap();

        let mut reloaded = Config::default();
        reloaded.hydrate_secrets(&store);
        assert_eq!(
            reloaded.providers.openai.api_key, "",
            "a deleted credential must not be revived by the next hydrate"
        );
    }

    #[test]
    fn push_secrets_to_store_leaves_an_untouched_key_alone() {
        // Saving without having cleared or changed anything must not issue
        // a spurious delete for a provider that simply has no key.
        let store = InMemoryStore::default();
        let mut config = Config::default();
        config.push_secrets_to_store(&store, &|_| false).unwrap();
        assert_eq!(store.get(&target_name("anthropic")).unwrap(), None);
    }

    #[test]
    fn push_secrets_to_store_saves_a_live_gemini_key() {
        let store = InMemoryStore::default();
        let mut config = Config::default();
        config.providers.gemini.api_key = "AIza-brand-new".to_string();

        config.push_secrets_to_store(&store, &|_| false).unwrap();

        assert_eq!(
            store.get(&target_name("gemini")).unwrap().as_deref(),
            Some("AIza-brand-new")
        );
        assert_eq!(
            config.providers.gemini.api_key, "",
            "the field must be blanked before the disk write"
        );
    }

    #[test]
    fn push_secrets_to_store_deletes_the_gemini_credential_when_the_field_is_cleared() {
        let store = InMemoryStore::default();
        store
            .set(&target_name("gemini"), "AIza-should-be-removed")
            .unwrap();
        let mut config = Config::default();
        // gemini.api_key left empty (the default) -- a deliberate clear.

        config.push_secrets_to_store(&store, &|_| false).unwrap();

        assert_eq!(store.get(&target_name("gemini")).unwrap(), None);
    }

    #[test]
    fn push_secrets_to_store_never_persists_an_env_sourced_gemini_key() {
        let store = InMemoryStore::default();
        let mut config = Config::default();
        config.providers.gemini.api_key = "env-temporary-value".to_string();

        config
            .push_secrets_to_store(&store, &|name| name == "GEMINI_API_KEY")
            .unwrap();

        assert_eq!(store.get(&target_name("gemini")).unwrap(), None);
        assert_eq!(config.providers.gemini.api_key, "");
    }

    #[test]
    fn push_secrets_to_store_never_persists_an_env_sourced_key() {
        // Bug: apply_env_overrides bakes the env value into the same field
        // Settings saves from, so any save while the env var was set wrote
        // that temporary value permanently into the store.
        let store = InMemoryStore::default();
        let mut config = Config::default();
        config.providers.openai.api_key = "env-temporary-value".to_string();

        config
            .push_secrets_to_store(&store, &|name| name == "OPENAI_API_KEY")
            .unwrap();

        assert_eq!(
            store.get(&target_name("openai")).unwrap(),
            None,
            "an env-sourced key must never reach the store"
        );
        assert_eq!(
            config.providers.openai.api_key, "",
            "the field must still be blanked before the disk write"
        );
    }

    #[test]
    fn push_secrets_to_store_does_not_touch_an_existing_credential_while_env_overrides_it() {
        // A real stored key must survive a save made while a temporary env
        // var happens to be set for an unrelated reason.
        let store = InMemoryStore::default();
        store.set(&target_name("openai"), "sk-permanent").unwrap();

        let mut config = Config::default();
        config.providers.openai.api_key = "env-temporary-value".to_string();

        config
            .push_secrets_to_store(&store, &|name| name == "OPENAI_API_KEY")
            .unwrap();

        assert_eq!(
            store.get(&target_name("openai")).unwrap().as_deref(),
            Some("sk-permanent"),
            "an env override must not disturb an already-stored key"
        );
    }

    #[test]
    fn push_secrets_to_store_only_skips_the_env_overridden_provider() {
        let store = InMemoryStore::default();
        let mut config = Config::default();
        config.providers.openai.api_key = "env-temporary-value".to_string();
        config.providers.anthropic.api_key = "sk-anthropic-real".to_string();

        config
            .push_secrets_to_store(&store, &|name| name == "OPENAI_API_KEY")
            .unwrap();

        assert_eq!(store.get(&target_name("openai")).unwrap(), None);
        assert_eq!(
            store.get(&target_name("anthropic")).unwrap().as_deref(),
            Some("sk-anthropic-real")
        );
    }

    #[test]
    fn push_secrets_to_store_errors_on_store_failure_instead_of_falling_back_to_disk() {
        let mut config = Config::default();
        config.providers.openai.api_key = "sk-should-never-reach-disk".to_string();

        let result = config.push_secrets_to_store(&FailingStore, &|_| false);

        assert!(
            result.is_err(),
            "a store write failure must propagate, not silently succeed"
        );
    }

    #[test]
    fn save_like_flow_never_writes_the_file_when_the_store_write_fails() {
        // Mirrors exactly what Config::save() does: reconcile the store,
        // then (only on success) write the file. Proves the file-write
        // side never runs when the store side fails, so a transient
        // Credential Manager error can never leave a plaintext key on disk.
        let path = scratch_path("secrets-save-failure");
        let mut on_disk = Config::default();
        on_disk.providers.openai.api_key = "sk-should-not-reach-disk".to_string();

        let result: Result<()> = (|| {
            on_disk.push_secrets_to_store(&FailingStore, &|_| false)?;
            on_disk.save_to(&path)
        })();

        assert!(result.is_err());
        assert!(
            !path.exists(),
            "the file must never be written when the store write fails"
        );
    }

    // -- #135: golden-config compatibility across the app's shape history ---
    //
    // Each fixture under `tests/fixtures/config/` is a config.toml written
    // by an earlier build (or, for the last one, a hypothetically newer
    // one), with every user-set value deliberately non-default so a
    // regression that silently resets a field to its built-in default is
    // caught rather than masked by a value that already matches it.
    // `write_fixture` copies the committed fixture text into a scratch temp
    // dir before `Config::load_from` ever sees it (rule 9: never touch the
    // real `%APPDATA%`).

    fn write_fixture(tag: &str, contents: &str) -> PathBuf {
        let path = scratch_path(tag);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn pre_rename_copilot_ask_config_survives_upgrade() {
        // Commit 80b2dcc's shape: no [providers.ollama], no
        // [providers.gemini], no top-level `mode` key at all.
        let contents = include_str!("../tests/fixtures/config/pre_rename_copilot_ask.toml");
        let path = write_fixture("golden-pre-rename", contents);

        let cfg = Config::load_from(&path).expect("load should succeed");

        assert_eq!(
            cfg.hotkeys.primary,
            Chord {
                vk: 112,
                ctrl: true,
                shift: false,
                alt: true,
                win: false
            }
        );
        assert_eq!(
            cfg.hotkeys.secondary,
            Chord {
                vk: 75,
                ctrl: true,
                shift: true,
                alt: false,
                win: true
            }
        );
        assert_eq!(cfg.capture.max_edge, 2048);
        assert_eq!(cfg.capture.monitor, "primary");
        assert_eq!(cfg.ui.card_seconds, 45);
        assert_eq!(cfg.ui.text_scale, 1.25);
        assert!(!cfg.ui.show_difficulty);
        assert_eq!(
            cfg.ui.prompt,
            "Pre-rename custom prompt: check only the arithmetic."
        );
        assert_eq!(cfg.providers.order, vec!["anthropic", "openai"]);
        assert_eq!(cfg.providers.openai.model, "gpt-5.4-mini");
        assert_eq!(cfg.providers.openai.effort, "medium");
        assert_eq!(
            cfg.providers.openai.models,
            vec!["gpt-5.4-mini", "gpt-4.1", "custom-openai-model"]
        );
        assert_eq!(cfg.providers.anthropic.model, "claude-sonnet-5");
        assert_eq!(cfg.providers.anthropic.effort, "high");
        assert_eq!(
            cfg.providers.anthropic.models,
            vec!["claude-sonnet-5", "claude-haiku-4-5", "custom-claude-model"]
        );
        // Fields this era's config.toml has no key for at all must still
        // backfill to something usable, never a parse failure or an empty
        // hole a newer build depends on.
        assert_eq!(cfg.mode, Mode::Auto);
        assert!(!cfg.providers.ollama.base_url.is_empty());
        assert!(!cfg.providers.ollama.model.is_empty());
        assert!(!cfg.providers.gemini.models.is_empty());

        cleanup(&path);
    }

    #[test]
    fn before_gemini_and_ollama_config_survives_upgrade() {
        // Commit 05a3829~1's shape: renamed to Wingman, but still before
        // Ollama (#13) and Gemini (#17) -- neither [providers.ollama] nor
        // [providers.gemini] exists yet, and there is still no `mode` key.
        let contents = include_str!("../tests/fixtures/config/before_gemini_ollama.toml");
        let path = write_fixture("golden-before-gemini-ollama", contents);

        let cfg = Config::load_from(&path).expect("load should succeed");

        assert_eq!(
            cfg.hotkeys.primary,
            Chord {
                vk: 66,
                ctrl: true,
                shift: true,
                alt: false,
                win: false
            }
        );
        assert_eq!(
            cfg.hotkeys.secondary,
            Chord {
                vk: 219,
                ctrl: false,
                shift: true,
                alt: true,
                win: true
            }
        );
        assert_eq!(cfg.capture.max_edge, 1800);
        assert_eq!(cfg.capture.monitor, "active");
        assert_eq!(cfg.ui.card_seconds, 8);
        assert_eq!(cfg.ui.text_scale, 1.5);
        assert!(!cfg.ui.show_difficulty);
        assert_eq!(cfg.ui.prompt, "Post-rename, pre-expansion custom prompt.");
        assert_eq!(cfg.providers.order, vec!["openai", "anthropic"]);
        assert_eq!(cfg.providers.openai.model, "gpt-5.1");
        assert_eq!(
            cfg.providers.openai.models,
            vec!["gpt-5.1", "gpt-5", "custom-openai-pre-ollama"]
        );
        assert_eq!(cfg.providers.anthropic.model, "claude-fable-5-1");
        assert_eq!(
            cfg.providers.anthropic.models,
            vec![
                "claude-fable-5-1",
                "claude-opus-5",
                "custom-claude-pre-ollama"
            ]
        );
        assert_eq!(cfg.mode, Mode::Auto);
        assert!(!cfg.providers.ollama.base_url.is_empty());
        assert!(!cfg.providers.gemini.models.is_empty());

        cleanup(&path);
    }

    #[test]
    fn before_modes_config_survives_upgrade() {
        // Commit f8789e6~1's shape: Ollama (#13) exists, but Mode (#19) and
        // Gemini (#17) do not -- [providers.ollama] is present, there is no
        // `mode` key and no [providers.gemini].
        let contents = include_str!("../tests/fixtures/config/before_modes.toml");
        let path = write_fixture("golden-before-modes", contents);

        let cfg = Config::load_from(&path).expect("load should succeed");

        assert_eq!(
            cfg.hotkeys.primary,
            Chord {
                vk: 122,
                ctrl: false,
                shift: true,
                alt: true,
                win: false
            }
        );
        assert_eq!(
            cfg.hotkeys.secondary,
            Chord {
                vk: 190,
                ctrl: true,
                shift: false,
                alt: false,
                win: true
            }
        );
        assert_eq!(cfg.capture.max_edge, 1200);
        assert_eq!(cfg.capture.monitor, "primary");
        assert_eq!(cfg.ui.card_seconds, 30);
        assert_eq!(cfg.ui.text_scale, 0.9);
        assert!(cfg.ui.show_difficulty);
        assert_eq!(cfg.ui.prompt, "Before-Modes custom prompt: focus on units.");
        assert_eq!(cfg.providers.order, vec!["ollama", "anthropic", "openai"]);
        assert_eq!(cfg.providers.openai.model, "gpt-5.2");
        assert_eq!(
            cfg.providers.openai.models,
            vec!["gpt-5.2", "gpt-5.1", "custom-openai-before-modes"]
        );
        assert_eq!(cfg.providers.anthropic.model, "claude-opus-4-8");
        assert_eq!(
            cfg.providers.anthropic.models,
            vec![
                "claude-opus-4-8",
                "claude-sonnet-5",
                "custom-claude-before-modes"
            ]
        );
        assert_eq!(cfg.providers.ollama.base_url, "http://127.0.0.1:11434");
        assert_eq!(cfg.providers.ollama.model, "llava:13b");
        assert_eq!(cfg.providers.ollama.effort, "high");
        // No `mode` key in this era's file -- must backfill to Auto rather
        // than failing to parse or silently landing on some other variant.
        assert_eq!(cfg.mode, Mode::Auto);
        assert!(!cfg.providers.gemini.models.is_empty());

        cleanup(&path);
    }

    #[test]
    fn unknown_future_keys_do_not_fail_parsing_or_evict_known_values() {
        // A config.toml from a build newer than this one: today's full
        // shape plus keys nothing in this codebase has ever defined, at the
        // top level and inside every section. No struct in this module
        // carries `#[serde(deny_unknown_fields)]`, so these must be
        // silently ignored, never turn the load into a fallback-to-defaults
        // (which `parse_or_default`/`load_from` only do for text that fails
        // to parse as TOML at all, not for unrecognized keys within valid
        // TOML).
        let contents = include_str!("../tests/fixtures/config/unknown_future_keys.toml");
        let path = write_fixture("golden-unknown-keys", contents);

        let cfg = Config::load_from(&path).expect("load should succeed despite unknown keys");

        assert_eq!(
            cfg.hotkeys.primary,
            Chord {
                vk: 27,
                ctrl: true,
                shift: true,
                alt: true,
                win: false
            }
        );
        assert_eq!(
            cfg.hotkeys.secondary,
            Chord {
                vk: 9,
                ctrl: false,
                shift: false,
                alt: false,
                win: true
            }
        );
        assert_eq!(cfg.capture.max_edge, 3000);
        assert_eq!(cfg.ui.card_seconds, 99);
        assert_eq!(cfg.ui.text_scale, 2.0);
        assert_eq!(cfg.ui.prompt, "Future config custom prompt.");
        assert_eq!(cfg.providers.order, vec!["anthropic", "ollama", "openai"]);
        assert_eq!(cfg.providers.openai.model, "gpt-5.5-pro");
        assert_eq!(cfg.providers.anthropic.model, "claude-opus-5");
        assert_eq!(cfg.providers.gemini.model, "gemini-3.8-flash");
        assert_eq!(cfg.providers.ollama.model, "gemma3:4b");
        assert_eq!(cfg.mode, Mode::Cloud);
        // The whole point: a parse that tripped over an unknown key and
        // fell back wholesale would produce exactly `Config::default()`.
        assert_ne!(cfg, Config::default());

        cleanup(&path);
    }
}
