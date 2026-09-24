//! Issue #124: the "Copy diagnostics" tray item. Builds a plain-text report
//! and puts it on the clipboard -- no file writes, no network, nothing
//! leaves this process except onto the clipboard the user explicitly asked
//! for (`App::copy_diagnostics` in `app.rs`).
//!
//! Split per AGENTS.md rule 8: [`render_report`] is pure and unit-tested
//! directly, including the redaction guarantee (never a substring of a real
//! key, see `render_report_never_leaks_any_part_of_a_configured_key`
//! below). [`collect`] gathers the real values from Win32 and `Config` and
//! is checked by hand (rule 8) -- there is no way to unit test
//! `GetCurrentPackageFullName` or the registry build number without a real
//! process, and this module makes no attempt to fake one.
//!
//! # Never a key, not even a fragment
//!
//! [`DiagnosticsInput`] never holds a raw `api_key` string at all --
//! [`KeyStatus`] is computed once, in [`provider_rows`], and only the
//! three-way classification survives into the struct. There is structurally
//! nothing in this module capable of printing a key, unlike a design that
//! carried the string through and redacted it at render time.

use crate::config::{Config, UNREADABLE_KEY_MARKER};
use crate::hotkey::chord_to_string;
use crate::mode::Mode;
use crate::provider::ollama_admin;

/// Whether a provider's API key is configured, absent, or present in
/// Credential Manager but unreadable (#175's marker) -- never the key
/// itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStatus {
    Set,
    Unset,
    Unreadable,
}

impl KeyStatus {
    pub fn from_key(key: &str) -> Self {
        if key == UNREADABLE_KEY_MARKER {
            KeyStatus::Unreadable
        } else if key.is_empty() {
            KeyStatus::Unset
        } else {
            KeyStatus::Set
        }
    }

    fn label(self) -> &'static str {
        match self {
            KeyStatus::Set => "set",
            KeyStatus::Unset => "unset",
            KeyStatus::Unreadable => "unreadable",
        }
    }
}

/// Review of #349/#426: the full (already redacted, #253) chain behind the
/// most recent error card, plus enough context to identify it. `App` keeps
/// this in memory only (Hard Rule 5, and privacy: never written to disk)
/// and hands a fresh one to [`collect`] on every "Copy diagnostics" click,
/// so the card's "use Copy diagnostics for details" pointer is only ever
/// shown when it is actually true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastError {
    /// The same headline the error card itself showed.
    pub action: String,
    /// How long ago the error happened, computed by the caller (`App`, from
    /// its own stored `SystemTime`) so this module stays free of a real
    /// clock read in its pure half.
    pub seconds_ago: u64,
    pub chain: String,
}

/// One entry in the report's provider table, in `providers.order`.
/// `key` is `None` for Ollama, which has no API key at all (a local server
/// has nothing to authenticate with -- see `config.rs`'s `OllamaConfig`
/// doc comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub name: String,
    pub model: String,
    pub key: Option<KeyStatus>,
}

/// Everything [`render_report`] needs, already reduced to display-safe
/// values. Built by [`collect`] against the real process/config, or by a
/// test against a synthetic one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsInput {
    pub wingman_version: String,
    /// `None` when the registry read failed (see `windows_build_number`).
    pub windows_build: Option<String>,
    pub package_identity: bool,
    pub dpi_per_monitor_aware: bool,
    /// `None` when monitor enumeration itself failed.
    pub monitor_count: Option<usize>,
    pub mode: Mode,
    pub paused: bool,
    /// `providers.order`, each resolved to its model and key status.
    pub providers: Vec<ProviderRow>,
    /// Env var *names* only (`OPENAI_API_KEY`, ...) that are currently set
    /// and therefore override the corresponding config value -- never the
    /// value itself.
    pub env_overrides: Vec<String>,
    pub config_path: Option<String>,
    pub autostart_enabled: bool,
    pub primary_hotkey: String,
    pub secondary_hotkey: String,
    /// [`ollama_admin::OllamaHealth::message`] for whoever is (or isn't)
    /// listening on the configured Ollama port -- Win32-only, no HTTP (see
    /// that module's doc comment).
    pub ollama_health: String,
    /// `None` when nothing has failed yet this session. See [`LastError`].
    pub last_error: Option<LastError>,
}

/// Reduces `config.providers` to the report's provider table, in
/// `providers.order`, the same order the tray tooltip and `build_chain` use.
/// Reads [`crate::config::Providers::describe_all`], the single source of
/// truth `config.rs` and this module now share (issue #201) instead of each
/// keeping its own copy of the provider-name-to-config mapping -- before
/// this fix, this function's own hand-copied match had no `"compat:*"` arm
/// at all, so a configured compat provider was silently invisible in every
/// diagnostics report. `KeyStatus::from_key` is applied here, at the one
/// place a raw `api_key` value is ever looked at, rather than inside
/// `describe_all` (which stays a plain data accessor, not diagnostics-aware).
pub fn provider_rows(config: &Config) -> Vec<ProviderRow> {
    config
        .providers
        .describe_all()
        .into_iter()
        .map(|d| ProviderRow {
            name: d.name,
            model: d.model,
            key: d.api_key.as_deref().map(KeyStatus::from_key),
        })
        .collect()
}

/// The env var names from [`crate::config::ENV_OVERRIDE_VARS`] that are
/// currently set in this process's environment. Names only -- never
/// `std::env::var`'s `Ok` value. Reads the same list `Config::env_var_name`
/// does (issue #201), rather than a hand-copied local one that could drift
/// from it.
fn env_overrides_present() -> Vec<String> {
    crate::config::ENV_OVERRIDE_VARS
        .iter()
        .map(|(_, var)| *var)
        .filter(|var| std::env::var(var).is_ok())
        .map(|var| var.to_string())
        .collect()
}

/// Renders `input` as a plain-text report, ready for the clipboard and for
/// pasting into a bug report's "Steps to reproduce" / attachment field. Pure
/// -- no Win32, no I/O -- so every line is unit-tested directly. No em
/// dashes (AGENTS.md rule 11): this text is meant to be pasted verbatim into
/// a public GitHub issue.
pub fn render_report(input: &DiagnosticsInput) -> String {
    let mut out = String::new();
    out.push_str("Wingman diagnostics\n");
    out.push_str("====================\n");
    out.push_str(&format!("Version: {}\n", input.wingman_version));
    out.push_str(&format!(
        "Windows build: {}\n",
        input.windows_build.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "Package identity: {}\n",
        if input.package_identity {
            "present"
        } else {
            "not packaged"
        }
    ));
    out.push_str(&format!(
        "DPI awareness: {}\n",
        if input.dpi_per_monitor_aware {
            "per monitor v2"
        } else {
            "not per monitor"
        }
    ));
    out.push_str(&format!(
        "Monitors: {}\n",
        input
            .monitor_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    ));
    out.push_str(&format!("Mode: {}\n", input.mode.label()));
    out.push_str(&format!(
        "Paused: {}\n",
        if input.paused { "yes" } else { "no" }
    ));
    out.push_str(&format!(
        "Config path: {}\n",
        input.config_path.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "Autostart: {}\n",
        if input.autostart_enabled {
            "enabled"
        } else {
            "disabled"
        }
    ));
    out.push_str(&format!(
        "Hotkeys: primary={}, secondary={}\n",
        input.primary_hotkey, input.secondary_hotkey
    ));
    out.push_str(&format!("Ollama: {}\n", input.ollama_health));

    out.push_str("\nEnv overrides present:");
    if input.env_overrides.is_empty() {
        out.push_str(" none\n");
    } else {
        out.push('\n');
        for name in &input.env_overrides {
            out.push_str(&format!("- {name}\n"));
        }
    }

    out.push_str("\nLast error:");
    match &input.last_error {
        None => out.push_str(" none\n"),
        Some(e) => {
            out.push_str(&format!(" {} ({} seconds ago)\n", e.action, e.seconds_ago));
            out.push_str(&format!("{}\n", e.chain));
        }
    }

    out.push_str("\nProviders (in order):\n");
    if input.providers.is_empty() {
        out.push_str("(none configured)\n");
    } else {
        for row in &input.providers {
            match row.key {
                Some(status) => out.push_str(&format!(
                    "- {}: model={}, key={}\n",
                    row.name,
                    row.model,
                    status.label()
                )),
                None => out.push_str(&format!("- {}: model={}\n", row.name, row.model)),
            }
        }
    }

    out
}

/// Issue #106's "way to see it that works today": the "Copy egress log"
/// tray item, alongside "Copy diagnostics" above. There is no settings
/// window yet to give the log its own page (#44/#51 -- the WebView2
/// settings host does not exist), so this is the same "no page, but not
/// invisible either" answer #124 already gave diagnostics: read the real
/// `egress.log` (`egress::read_all_human`) and put it on the clipboard,
/// human-readable, ready to paste into a bug report. See `app.rs`'s
/// `copy_egress_log` for the clipboard write itself.
pub fn egress_report() -> String {
    crate::egress::read_all_human()
}

// ===========================================================================
// Win32/Config data collection -- checked by hand (AGENTS.md rule 8), never
// unit tested against the real process. The manual check: run the app, open
// the tray menu, click "Copy diagnostics", paste the clipboard and confirm
// every line above is populated (not "unknown") on this machine -- see
// issue #166.
// ===========================================================================

/// Builds the real report input from the running process and `config`.
/// `last_error` is `App`'s own in-memory record (see [`LastError`]), passed
/// in rather than read from anywhere here -- this module has no error state
/// of its own and never will (Hard Rule 5: nothing here polls or persists).
pub fn collect(config: &Config, last_error: Option<LastError>) -> DiagnosticsInput {
    let ollama_port =
        ollama_admin::port_from_base_url(&config.providers.ollama.base_url).unwrap_or(11434);
    let ollama_health = ollama_admin::query_ollama_health(ollama_port).message();

    DiagnosticsInput {
        wingman_version: env!("CARGO_PKG_VERSION").to_string(),
        windows_build: windows_build_number(),
        package_identity: has_package_identity(),
        dpi_per_monitor_aware: is_per_monitor_dpi_aware(),
        monitor_count: xcap::Monitor::all().ok().map(|m| m.len()),
        mode: crate::mode::current(),
        paused: crate::pause::is_paused_now(),
        providers: provider_rows(config),
        env_overrides: env_overrides_present(),
        config_path: Config::path().ok().map(|p| p.display().to_string()),
        autostart_enabled: crate::autostart::is_enabled(),
        primary_hotkey: chord_to_string(&config.hotkeys.primary),
        secondary_hotkey: chord_to_string(&config.hotkeys.secondary),
        ollama_health,
        last_error,
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads `CurrentBuildNumber` from
/// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`. `None` on any
/// registry failure (best-effort, same as `autostart.rs`'s value reads);
/// the report shows "unknown" rather than failing to build at all.
fn windows_build_number() -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    let key_path = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let value_name = wide("CurrentBuildNumber");

    unsafe {
        let mut key = HKEY::default();
        // RegOpenKeyExW returns a WIN32_ERROR, whose `.ok()` converts it to
        // `windows_core::Result<()>` (not an `Option`) -- the second `.ok()`
        // discards that into the `Option` this function actually returns.
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            KEY_READ,
            &mut key,
        )
        .ok()
        .ok()?;

        let mut len: u32 = 0;
        let probe = RegQueryValueExW(
            key,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut len),
        );
        if probe.is_err() || len == 0 {
            let _ = RegCloseKey(key);
            return None;
        }

        let mut buf = vec![0u8; len as usize];
        let read = RegQueryValueExW(
            key,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        );
        let _ = RegCloseKey(key);
        if read.is_err() {
            return None;
        }

        let units: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        Some(String::from_utf16_lossy(&units))
    }
}

/// `GetProcessDpiAwareness(None)` (current process) == `PROCESS_PER_MONITOR_DPI_AWARE`.
/// `App::run` sets `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` at startup
/// (see `app.rs`); this reads back what actually took effect rather than
/// assuming it did.
fn is_per_monitor_dpi_aware() -> bool {
    use windows::Win32::UI::HiDpi::{GetProcessDpiAwareness, PROCESS_PER_MONITOR_DPI_AWARE};
    matches!(
        unsafe { GetProcessDpiAwareness(None) },
        Ok(v) if v == PROCESS_PER_MONITOR_DPI_AWARE
    )
}

/// `GetCurrentPackageFullName`: `true` when this process has package
/// identity (running under the sparse MSIX via its AUMID), `false` for
/// `APPMODEL_ERROR_NO_PACKAGE` (a direct exe launch, e.g. the `Run` key --
/// see AGENTS.md's "Windows OCR under a sparse package" pitfall, which this
/// diagnostic exists to help answer). Any other, unexpected error also
/// reports `false` -- best-effort, same as the rest of this module.
fn has_package_identity() -> bool {
    use windows::Win32::Foundation::APPMODEL_ERROR_NO_PACKAGE;
    use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;

    let mut len: u32 = 0;
    let result = unsafe { GetCurrentPackageFullName(&mut len, None) };
    result != APPMODEL_ERROR_NO_PACKAGE && len > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input() -> DiagnosticsInput {
        DiagnosticsInput {
            wingman_version: "0.1.0".to_string(),
            windows_build: Some("26200".to_string()),
            package_identity: false,
            dpi_per_monitor_aware: true,
            monitor_count: Some(2),
            mode: Mode::Auto,
            paused: false,
            providers: vec![
                ProviderRow {
                    name: "openai".to_string(),
                    model: "gpt-5.5".to_string(),
                    key: Some(KeyStatus::Unset),
                },
                ProviderRow {
                    name: "anthropic".to_string(),
                    model: "claude-opus-5".to_string(),
                    key: Some(KeyStatus::Set),
                },
                ProviderRow {
                    name: "ollama".to_string(),
                    model: "gemma3:4b".to_string(),
                    key: None,
                },
            ],
            env_overrides: vec!["OPENAI_API_KEY".to_string()],
            config_path: Some(r"C:\Users\test\AppData\Roaming\Wingman\config.toml".to_string()),
            autostart_enabled: true,
            primary_hotkey: "Win+Shift+F23".to_string(),
            secondary_hotkey: "Ctrl+Shift+/".to_string(),
            ollama_health: "Ollama is not running.".to_string(),
            last_error: None,
        }
    }

    // -- KeyStatus::from_key ------------------------------------------------

    #[test]
    fn from_key_classifies_empty_as_unset() {
        assert_eq!(KeyStatus::from_key(""), KeyStatus::Unset);
    }

    #[test]
    fn from_key_classifies_a_real_looking_key_as_set() {
        assert_eq!(KeyStatus::from_key("sk-TESTSECRET1234"), KeyStatus::Set);
    }

    #[test]
    fn from_key_classifies_the_unreadable_marker_as_unreadable() {
        assert_eq!(
            KeyStatus::from_key(UNREADABLE_KEY_MARKER),
            KeyStatus::Unreadable
        );
    }

    // -- provider_rows --------------------------------------------------------

    #[test]
    fn provider_rows_follows_configured_order() {
        let mut config = Config::default();
        config.providers.order = vec!["anthropic".to_string(), "openai".to_string()];
        let rows = provider_rows(&config);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "anthropic");
        assert_eq!(rows[1].name, "openai");
    }

    #[test]
    fn provider_rows_skips_unrecognized_names() {
        let mut config = Config::default();
        config.providers.order = vec!["bogus".to_string(), "openai".to_string()];
        let rows = provider_rows(&config);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "openai");
    }

    #[test]
    fn provider_rows_ollama_has_no_key_status() {
        let mut config = Config::default();
        config.providers.order = vec!["ollama".to_string()];
        let rows = provider_rows(&config);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, None);
    }

    #[test]
    fn provider_rows_reports_key_status_per_provider() {
        let mut config = Config::default();
        config.providers.order = vec!["openai".to_string(), "anthropic".to_string()];
        config.providers.openai.api_key = "sk-real".to_string();
        // anthropic left empty.
        let rows = provider_rows(&config);
        assert_eq!(rows[0].key, Some(KeyStatus::Set));
        assert_eq!(rows[1].key, Some(KeyStatus::Unset));
    }

    #[test]
    fn provider_rows_shows_a_configured_compat_provider() {
        // Issue #201's core repro: `provider_rows` used to keep its own
        // hand-copied match with no `"compat:*"` arm, so a provider that
        // genuinely exists in `config.providers.compat` and is named in
        // `providers.order` never showed up in a diagnostics report at all
        // -- silently, with no error. Reading `Providers::describe_all`
        // (the shared source of truth) fixes that; this test fails again if
        // `provider_rows` ever grows its own copy of the provider list.
        use crate::provider::openai_compat::{CompatAuth, Structured};
        let mut config = Config::default();
        config.providers.compat = vec![crate::config::CompatConfig {
            name: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            auth: CompatAuth::Bearer,
            auth_header: String::new(),
            api_key: "sk-compat-real".to_string(),
            model: "some-model".to_string(),
            models: vec!["some-model".to_string()],
            structured: Structured::JsonSchema,
            vision: true,
        }];
        config.providers.order = vec!["compat:openrouter".to_string()];

        let rows = provider_rows(&config);
        assert_eq!(rows.len(), 1, "the compat provider must be visible");
        assert_eq!(rows[0].name, "compat:openrouter");
        assert_eq!(rows[0].model, "some-model");
        assert_eq!(rows[0].key, Some(KeyStatus::Set));
    }

    // -- render_report: content -----------------------------------------------

    #[test]
    fn render_report_contains_version_and_windows_build() {
        let text = render_report(&sample_input());
        assert!(text.contains("Version: 0.1.0"), "{text}");
        assert!(text.contains("Windows build: 26200"), "{text}");
    }

    #[test]
    fn render_report_contains_mode_and_paused_state() {
        let mut input = sample_input();
        input.mode = Mode::Offline;
        input.paused = true;
        let text = render_report(&input);
        assert!(text.contains("Mode: Offline"), "{text}");
        assert!(text.contains("Paused: yes"), "{text}");
    }

    #[test]
    fn render_report_lists_providers_in_order_with_model_and_key_status() {
        let text = render_report(&sample_input());
        let openai_line = text
            .lines()
            .find(|l| l.contains("openai"))
            .expect("openai line");
        assert!(openai_line.contains("model=gpt-5.5"), "{openai_line}");
        assert!(openai_line.contains("key=unset"), "{openai_line}");
        let anthropic_line = text
            .lines()
            .find(|l| l.contains("anthropic"))
            .expect("anthropic line");
        assert!(anthropic_line.contains("key=set"), "{anthropic_line}");
        // Providers must render in the same order as the input, so the
        // model that answers first in a real request also appears first
        // here.
        let openai_pos = text.find("openai").unwrap();
        let anthropic_pos = text.find("anthropic").unwrap();
        assert!(openai_pos < anthropic_pos);
    }

    #[test]
    fn render_report_ollama_row_has_no_key_field() {
        let text = render_report(&sample_input());
        let ollama_line = text
            .lines()
            .find(|l| l.contains("ollama"))
            .expect("ollama line");
        assert!(!ollama_line.contains("key="), "{ollama_line}");
        assert!(ollama_line.contains("model=gemma3:4b"), "{ollama_line}");
    }

    #[test]
    fn render_report_lists_env_overrides_by_name() {
        let text = render_report(&sample_input());
        assert!(text.contains("OPENAI_API_KEY"), "{text}");
    }

    #[test]
    fn render_report_with_no_env_overrides_says_none() {
        let mut input = sample_input();
        input.env_overrides = Vec::new();
        let text = render_report(&input);
        assert!(text.contains("Env overrides present: none"), "{text}");
    }

    #[test]
    fn render_report_with_no_providers_configured_says_so() {
        let mut input = sample_input();
        input.providers = Vec::new();
        let text = render_report(&input);
        assert!(text.contains("(none configured)"), "{text}");
    }

    #[test]
    fn render_report_shows_unknown_for_missing_optional_fields() {
        let mut input = sample_input();
        input.windows_build = None;
        input.monitor_count = None;
        input.config_path = None;
        let text = render_report(&input);
        assert!(text.contains("Windows build: unknown"), "{text}");
        assert!(text.contains("Monitors: unknown"), "{text}");
        assert!(text.contains("Config path: unknown"), "{text}");
    }

    #[test]
    fn render_report_shows_package_identity_and_dpi_awareness() {
        let mut input = sample_input();
        input.package_identity = true;
        input.dpi_per_monitor_aware = false;
        let text = render_report(&input);
        assert!(text.contains("Package identity: present"), "{text}");
        assert!(text.contains("DPI awareness: not per monitor"), "{text}");
    }

    // -- Last error (review of #349/#426) --------------------------------

    #[test]
    fn render_report_says_none_when_nothing_has_failed() {
        let text = render_report(&sample_input());
        assert!(text.contains("Last error: none"), "{text}");
    }

    #[test]
    fn render_report_includes_the_last_error_action_age_and_full_chain() {
        let mut input = sample_input();
        input.last_error = Some(LastError {
            action: "Couldn't add the event".to_string(),
            seconds_ago: 42,
            chain: "TzSpecificLocalTimeToSystemTime failed: os error 87".to_string(),
        });
        let text = render_report(&input);
        assert!(
            text.contains("Last error: Couldn't add the event (42 seconds ago)"),
            "{text}"
        );
        assert!(
            text.contains("TzSpecificLocalTimeToSystemTime failed: os error 87"),
            "{text}: the full chain must be present so Copy diagnostics is not an empty promise"
        );
    }

    #[test]
    fn render_report_contains_hotkeys_and_ollama_health() {
        let text = render_report(&sample_input());
        assert!(text.contains("Win+Shift+F23"), "{text}");
        assert!(text.contains("Ctrl+Shift+/"), "{text}");
        assert!(text.contains("Ollama is not running."), "{text}");
    }

    #[test]
    fn no_report_line_contains_an_em_dash() {
        // AGENTS.md rule 11: this text is meant to be pasted into a public
        // GitHub issue verbatim.
        let text = render_report(&sample_input());
        assert!(!text.contains('\u{2014}'), "{text}");
    }

    // -- the redaction guarantee ------------------------------------------

    #[test]
    fn render_report_never_leaks_any_part_of_a_configured_key() {
        // #124's core requirement: build the report from a config carrying
        // a realistic-looking secret and prove no substring of it (length
        // >= 4) survives into the rendered text. This exercises the real
        // path from `Config` through `provider_rows` into `render_report`,
        // not just `render_report` alone -- `DiagnosticsInput` never even
        // holds the raw key (see this module's doc comment), so this test
        // also guards against a future change reintroducing one.
        let secret = "sk-TESTSECRET1234567890ABCDEF";
        let mut config = Config::default();
        config.providers.order = vec!["openai".to_string(), "anthropic".to_string()];
        config.providers.openai.api_key = secret.to_string();
        config.providers.anthropic.api_key = crate::config::UNREADABLE_KEY_MARKER.to_string();

        let rows = provider_rows(&config);
        let mut input = sample_input();
        input.providers = rows;
        let text = render_report(&input);

        assert!(text.contains("key=set"), "{text}");
        assert!(text.contains("key=unreadable"), "{text}");

        // Every substring of length >= 4 of the secret must be absent.
        let bytes = secret.as_bytes();
        for start in 0..bytes.len().saturating_sub(3) {
            for len in 4..=(bytes.len() - start) {
                let fragment = std::str::from_utf8(&bytes[start..start + len]).unwrap();
                assert!(
                    !text.contains(fragment),
                    "leaked key fragment {fragment:?} in report:\n{text}"
                );
            }
        }
    }

    // -- egress_report (#106) ------------------------------------------------

    #[test]
    fn egress_report_never_panics_and_returns_readable_text() {
        // This module's own tests don't stub the log file (that's
        // `provider::common`'s and `egress`'s job); it just proves the
        // wrapper never panics and always hands back SOMETHING pasteable,
        // whatever this test-binary run's shared temp egress log happens to
        // contain at the moment.
        let text = egress_report();
        assert!(
            !text.is_empty(),
            "must never be blank, even with nothing logged"
        );
    }

    #[test]
    fn env_overrides_present_never_returns_an_unrecognized_name() {
        // Pure guard: whatever is actually set in this test process's
        // environment, the function must only ever report names from the
        // fixed allowlist, never an arbitrary env var (which could carry
        // anything, including a secret set by an unrelated tool).
        for name in env_overrides_present() {
            assert!(
                crate::config::ENV_OVERRIDE_VARS
                    .iter()
                    .any(|(_, var)| *var == name),
                "{name}"
            );
        }
    }
}
