//! Mode (issue #19): `Cloud | Local | Auto | Offline`, the process-wide flag
//! the Offline guard in `provider/common.rs` reads on every request, pure
//! provider selection, and the on-demand (never polling, rule 5) Ollama
//! reachability probe Auto mode uses.
//!
//! See `docs/superpowers/specs/2026-09-16-expansion-plan-design.md`, the
//! "Modes" table under section 5, and `gh issue view 19`.
//!
//! | mode | providers | network |
//! |---|---|---|
//! | Cloud | configured cloud providers in order | yes |
//! | Local | Ollama and any loopback endpoint | loopback only |
//! | Auto (default) | Local first if Ollama is up with the model loaded, then Cloud | yes |
//! | Offline | Local only; the guard refuses non-loopback hosts at the socket layer; connectors disabled; update check disabled | loopback only, enforced |
//!
//! # Division of labor
//!
//! This module owns the *decision*: what `Mode` is active, which provider
//! names it selects, and whether a URL's host counts as loopback. It does
//! NOT own the *enforcement* -- that lives at the single HTTP send path in
//! `provider/common.rs` (`offline_guard`, called from `post_json_with`,
//! `post_json_with_connect_timeout` and `get_text_with_timeout`), which
//! reads [`is_offline_now`] and calls [`classify_host`] before any socket
//! opens. Putting the enforcement in the one file every provider already
//! funnels through (rather than trusting each provider to check) is what
//! makes the guard something a new provider cannot forget: see that file's
//! `offline_guard` doc comment.
//!
//! # CONNECTORS HOOK (not built yet)
//!
//! `connectors/*.rs` (Phase 3+) and the opt-in update checker (Phase 5) do
//! not exist in this crate yet. When they land, each must call
//! `provider::common`-style guarded transport (or, if their HTTP needs
//! diverge enough to need their own send path, call [`is_offline_now`] and
//! [`classify_host`] themselves before opening a socket) -- the Modes
//! table above requires both to be disabled outright in Offline mode, not
//! merely loopback-restricted like providers are.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// `Cloud | Local | Auto | Offline`. `Auto` is the plan's documented
/// default. `Copy` because it is threaded through by value everywhere
/// (config, tray, the worker thread) rather than borrowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Cloud,
    Local,
    #[default]
    Auto,
    Offline,
}

impl Mode {
    /// Capitalized display label, shared by the tray submenu (and anywhere
    /// else that wants the same four strings) so they cannot drift from
    /// each other.
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Cloud => "Cloud",
            Mode::Local => "Local",
            Mode::Auto => "Auto",
            Mode::Offline => "Offline",
        }
    }
}

// ---------------------------------------------------------------------
// Process-wide current mode (the Offline guard's fast, lock-free read).
//
// Mirrors `pause.rs`'s `PAUSE_DEADLINE` pattern: the authoritative,
// persisted value lives in `Config.mode` and on `App`; this atomic is a
// mirror every thread (including a future worker or connector thread with
// no access to `App`) can consult cheaply and without holding a lock.
// `App::run` must call `set_current(config.mode)` once at startup, before
// the hook or tray can generate the first request, and `App::set_mode`
// keeps it in sync on every change -- see that method's doc comment.
// ---------------------------------------------------------------------

const CLOUD_U8: u8 = 0;
const LOCAL_U8: u8 = 1;
const AUTO_U8: u8 = 2;
const OFFLINE_U8: u8 = 3;

static CURRENT_MODE: AtomicU8 = AtomicU8::new(AUTO_U8);

fn to_u8(m: Mode) -> u8 {
    match m {
        Mode::Cloud => CLOUD_U8,
        Mode::Local => LOCAL_U8,
        Mode::Auto => AUTO_U8,
        Mode::Offline => OFFLINE_U8,
    }
}

fn from_u8(v: u8) -> Mode {
    match v {
        CLOUD_U8 => Mode::Cloud,
        LOCAL_U8 => Mode::Local,
        OFFLINE_U8 => Mode::Offline,
        // Any other bit pattern (there should never be one) degrades to the
        // safe, documented default rather than an invalid enum value.
        _ => Mode::Auto,
    }
}

/// Publish `m` as the process-wide current mode. Called once at startup
/// (from the persisted `Config.mode`) and again on every tray Mode change.
pub fn set_current(m: Mode) {
    CURRENT_MODE.store(to_u8(m), Ordering::Relaxed);
}

/// The process-wide current mode, as last published by [`set_current`].
pub fn current() -> Mode {
    from_u8(CURRENT_MODE.load(Ordering::Relaxed))
}

/// Whether Offline mode is active right now. The one predicate
/// `provider/common.rs`'s guard actually calls.
pub fn is_offline_now() -> bool {
    current() == Mode::Offline
}

/// Test-only lock serializing every test in this crate that mutates
/// [`CURRENT_MODE`] (here and in `provider/common.rs`'s guard tests) --
/// `cargo test` runs tests in parallel by default, and this atomic is
/// process-wide, so two such tests running concurrently without a shared
/// lock could observe each other's writes. Mirrors `config.rs`'s
/// `ENV_LOCK` for the same reason (process-wide env vars).
#[cfg(test)]
pub(crate) static MODE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---------------------------------------------------------------------
// Provider selection (pure).
// ---------------------------------------------------------------------

/// Whether `name` identifies a "local" provider for mode-selection
/// purposes: one that only ever talks to a loopback endpoint. Today that
/// is exactly Ollama; a future loopback `openai_compat` entry (the
/// expansion plan's Providers table: "Local: Ollama and any loopback
/// endpoint") would extend this, not replace it.
pub fn is_local_provider_name(name: &str) -> bool {
    name == "ollama"
}

/// Filters and reorders `configured` (normally `Config.providers.order`,
/// in the user's configured order) for `mode`, per the Modes table above.
///
/// `ollama_ready` is the caller's already-computed answer to "is Ollama
/// configured, reachable, and does it have the model loaded, right now" --
/// see [`should_probe_ollama`] and [`probe_ollama_ready`] for how a caller
/// gets that answer cheaply and only when it matters. This function itself
/// does no I/O and makes no decision about whether to probe; it only acts
/// on the bool it is handed, which is what keeps it a pure, table-testable
/// function.
///
/// - `Cloud`: every non-local configured provider, in order.
/// - `Local` / `Offline`: every local configured provider, in order.
///   (`Offline`'s additional non-loopback-host refusal is enforced
///   separately, at the socket layer -- see the module docs.)
/// - `Auto`: local providers first, then cloud, but ONLY when
///   `ollama_ready` is true; otherwise cloud only -- local is not
///   attempted at all, not even as a later fallback (matching the plan's
///   "local first if Ollama is up ... then Cloud", not "cloud, then try
///   local too").
pub fn select_providers(mode: Mode, configured: &[String], ollama_ready: bool) -> Vec<String> {
    let local: Vec<String> = configured
        .iter()
        .filter(|n| is_local_provider_name(n))
        .cloned()
        .collect();
    let cloud: Vec<String> = configured
        .iter()
        .filter(|n| !is_local_provider_name(n))
        .cloned()
        .collect();

    match mode {
        Mode::Cloud => cloud,
        Mode::Local | Mode::Offline => local,
        Mode::Auto => {
            if ollama_ready {
                let mut selected = local;
                selected.extend(cloud);
                selected
            } else {
                cloud
            }
        }
    }
}

/// Whether Auto mode should even attempt the (network) reachability probe
/// below. `false` means "skip it entirely" -- the IMPORTANT product
/// constraint from issue #19: a user who never configured Ollama must see
/// zero added latency in Auto mode. Ollama's opt-in signal is exactly the
/// same one `Providers::default`'s doc comment already establishes: being
/// named in `providers.order` (see #13). A blank `base_url` is also
/// treated as "not configured", matching `Ollama::ready()`'s own rule.
pub fn should_probe_ollama(configured_order: &[String], ollama_base_url: &str) -> bool {
    !ollama_base_url.trim().is_empty() && configured_order.iter().any(|n| n == "ollama")
}

// ---------------------------------------------------------------------
// URL host classification (pure) -- the Offline guard's decision, called
// from `provider/common.rs`'s `offline_guard` before any socket opens.
// Deliberately NOT a general URL parser / no `url` crate dependency (rule
// 2): this implements just enough of RFC 3986 authority parsing to defeat
// the tricks a malicious or hand-edited `base_url` could use against a
// safety guard -- userinfo, a port suffix, IPv6 brackets, and case.
// ---------------------------------------------------------------------

/// How a URL's host classifies for the Offline guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    /// A literal loopback address (`127.0.0.0/8`, `::1`, or the
    /// IPv4-mapped `::ffff:127.x.x.x`): allowed while Offline.
    Loopback,
    /// The name `localhost` specifically: refused, but with guidance,
    /// since CLAUDE.md rule 6 already establishes that Wingman never uses
    /// it deliberately (IPv6-first resolution stalls ~2s on Windows) --
    /// a config that names it is almost always meant to mean 127.0.0.1.
    Localhost,
    /// Anything else, including a host this parser could not make sense
    /// of at all: refused. Parsing failure fails CLOSED (refused), never
    /// open -- see [`classify_host`]'s doc.
    NotLoopback,
}

/// Classifies `url`'s host for the Offline guard. Never resolves DNS (pure
/// string parsing only, as issue #19 requires: the guard must decide
/// before a socket opens, let alone a lookup completes) and never panics
/// on malformed input -- an unparseable or empty host classifies as
/// [`HostClass::NotLoopback`], the safe (refused) answer, not a panic or a
/// silent allow.
///
/// # IPv6 scope, decided and documented
///
/// Only the literal `::1` and the IPv4-mapped `::ffff:a.b.c.d` form (any
/// hex case) are recognized as IPv6 loopback. Other valid spellings of the
/// same address (`0:0:0:0:0:0:0:1`, zero-padded groups, ...) are NOT
/// special-cased and classify as `NotLoopback`. This under-recognizes
/// rather than over-recognizes on purpose: this codebase never produces
/// such a form (every loopback `base_url` this crate ships or documents
/// uses `127.0.0.1` or a bracketed `[::1]`, rule 6), so failing closed on
/// an unusual hand-edited spelling costs nothing in practice and cannot be
/// used to sneak a non-loopback host past the guard.
///
/// An IPv6 literal is only recognized when bracketed (`[::1]`), matching
/// RFC 3986 -- an unbracketed IPv6 address in a URL authority is not
/// well-formed, so this parser does not special-case it either (it falls
/// through the port-stripping branch for hostnames instead, which for a
/// string containing multiple `:` does not resemble a valid host at all
/// and lands on `NotLoopback`, again failing closed).
pub fn classify_host(url: &str) -> HostClass {
    let Some(host) = extract_host(url) else {
        return HostClass::NotLoopback;
    };

    if host == "localhost" {
        return HostClass::Localhost;
    }
    if let Some(octets) = parse_ipv4(&host) {
        return if octets[0] == 127 {
            HostClass::Loopback
        } else {
            HostClass::NotLoopback
        };
    }
    if host == "::1" {
        return HostClass::Loopback;
    }
    if let Some(rest) = host.strip_prefix("::ffff:") {
        if let Some(octets) = parse_ipv4(rest) {
            if octets[0] == 127 {
                return HostClass::Loopback;
            }
        }
        return HostClass::NotLoopback;
    }

    HostClass::NotLoopback
}

/// Extracts just the host portion of `url`: no scheme, no userinfo, no
/// port, no path/query/fragment, IPv6 brackets stripped, lowercased.
/// Returns `None` only when there is no discoverable host at all (an empty
/// string, or an authority that is empty after userinfo is removed).
fn extract_host(url: &str) -> Option<String> {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let end = after_scheme.find(['/', '?', '#']).unwrap_or(after_scheme.len());
    let authority = &after_scheme[..end];
    if authority.is_empty() {
        return None;
    }

    // Userinfo: everything up to and including the LAST '@' is discarded,
    // matching the WHATWG URL host-parsing algorithm's use of the last
    // '@'. `http://127.0.0.1@evil.com/`'s host is `evil.com`, not the
    // loopback-looking userinfo before it.
    let host_and_port = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    if host_and_port.is_empty() {
        return None;
    }

    let host = if let Some(rest) = host_and_port.strip_prefix('[') {
        let close = rest.find(']')?;
        &rest[..close]
    } else {
        match host_and_port.rfind(':') {
            // A trailing ":<digits>" is a port. IPv4 addresses and
            // hostnames never contain ':' themselves, so this split is
            // unambiguous for both.
            Some(i)
                if !host_and_port[i + 1..].is_empty()
                    && host_and_port[i + 1..].bytes().all(|b| b.is_ascii_digit()) =>
            {
                &host_and_port[..i]
            }
            _ => host_and_port,
        }
    };

    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Strict dotted-quad IPv4 parse: exactly four `.`-separated groups, each
/// 1-3 ASCII digits, each `<= 255`. Rejects anything else, including a
/// 5-label string like `127.0.0.1.evil.com` (which is a NAME, not an
/// address, and must never be classified as loopback just because it
/// starts with one).
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u16 = p.parse().ok()?;
        if v > 255 {
            return None;
        }
        out[i] = v as u8;
    }
    Some(out)
}

// ---------------------------------------------------------------------
// Ollama reachability probe (Auto mode only). Cheap, on demand, bounded --
// never a background poll (rule 5). Split into a pure decision
// (`ollama_ready_from_tags_response`, `model_present_in_tags`) and a thin
// network wrapper (`probe_ollama_ready`), the same split
// `provider/common.rs` already uses for `post_json`/`post_json_with`: the
// decision is unit-tested without a socket, the network wrapper is not
// (rule 8 -- checked by hand, see issue #19's closing comment / issue
// #166 for the named manual check).
// ---------------------------------------------------------------------

/// Bounded short timeout for the probe: loopback is either answering in a
/// few milliseconds or not answering at all (no DNS, no WAN latency), so
/// this only needs to be long enough to not misclassify a briefly slow
/// local server, never long enough to be felt as added latency.
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// Reads `models[].name` (falling back to `models[].model`) out of an
/// Ollama `/api/tags` response body and reports whether `model` is among
/// them. Malformed JSON, a missing `models` key, or an empty list all
/// degrade to `false` -- never an error, since the caller's only use for
/// this is a yes/no gate on whether to try Local first.
pub(crate) fn model_present_in_tags(body: &str, model: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let Some(models) = value.get("models").and_then(|v| v.as_array()) else {
        return false;
    };
    models.iter().any(|entry| {
        let name = entry.get("name").and_then(|v| v.as_str());
        let model_field = entry.get("model").and_then(|v| v.as_str());
        name == Some(model) || model_field == Some(model)
    })
}

/// The pure decision behind [`probe_ollama_ready`]: given the result of
/// fetching `/api/tags` (already reduced to `Result<body, _>` so the
/// network error type doesn't leak in here), is Ollama ready? A transport
/// failure (not running, refused, timed out) degrades to `false`, same as
/// a response that parses but doesn't list the model.
pub(crate) fn ollama_ready_from_tags_response<E>(result: Result<String, E>, model: &str) -> bool {
    match result {
        Ok(body) => model_present_in_tags(&body, model),
        Err(_) => false,
    }
}

/// The real probe: `GET {base_url}/api/tags`, bounded by [`PROBE_TIMEOUT`],
/// through `provider::common`'s guarded transport (so it is refused the
/// same way any other request would be if Offline mode were somehow
/// active when this runs -- it never should be, since Auto mode never
/// calls this, but the guard costs nothing extra to also cover it).
/// Called only when [`should_probe_ollama`] already said yes, i.e. only
/// when Ollama is actually configured -- see that function's doc for the
/// latency guarantee this preserves.
pub fn probe_ollama_ready(base_url: &str, model: &str) -> bool {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let result = crate::provider::common::get_text_with_timeout(&url, PROBE_TIMEOUT, "ollama-probe")
        .map_err(|e| e.to_string());
    ollama_ready_from_tags_response(result, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Mode ---------------------------------------------------------

    #[test]
    fn default_mode_is_auto() {
        assert_eq!(Mode::default(), Mode::Auto);
    }

    #[test]
    fn mode_labels_are_capitalized_and_distinct() {
        let labels = [Mode::Cloud.label(), Mode::Local.label(), Mode::Auto.label(), Mode::Offline.label()];
        assert_eq!(labels, ["Cloud", "Local", "Auto", "Offline"]);
    }

    #[test]
    fn mode_serializes_lowercase_and_round_trips() {
        for (m, expected) in [
            (Mode::Cloud, "\"cloud\""),
            (Mode::Local, "\"local\""),
            (Mode::Auto, "\"auto\""),
            (Mode::Offline, "\"offline\""),
        ] {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(json, expected);
            let back: Mode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
    }

    // -- process-wide atomic (the only tests in this module that touch it) --

    #[test]
    fn set_current_then_current_round_trips_every_variant() {
        let _guard = MODE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for m in [Mode::Cloud, Mode::Local, Mode::Auto, Mode::Offline] {
            set_current(m);
            assert_eq!(current(), m);
        }
        set_current(Mode::Auto); // restore the default for any test after this one
    }

    #[test]
    fn is_offline_now_true_only_for_offline() {
        let _guard = MODE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for (m, expect_offline) in [
            (Mode::Cloud, false),
            (Mode::Local, false),
            (Mode::Auto, false),
            (Mode::Offline, true),
        ] {
            set_current(m);
            assert_eq!(is_offline_now(), expect_offline, "failed for {m:?}");
        }
        set_current(Mode::Auto);
    }

    // -- select_providers (table tests) --------------------------------

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cloud_mode_excludes_ollama_regardless_of_readiness() {
        let configured = names(&["openai", "anthropic", "ollama"]);
        for ready in [true, false] {
            assert_eq!(
                select_providers(Mode::Cloud, &configured, ready),
                names(&["openai", "anthropic"]),
                "ready={ready}"
            );
        }
    }

    #[test]
    fn cloud_mode_preserves_configured_order() {
        let configured = names(&["anthropic", "openai"]);
        assert_eq!(select_providers(Mode::Cloud, &configured, false), names(&["anthropic", "openai"]));
    }

    #[test]
    fn local_mode_is_ollama_only_regardless_of_readiness() {
        let configured = names(&["openai", "anthropic", "ollama"]);
        for ready in [true, false] {
            assert_eq!(select_providers(Mode::Local, &configured, ready), names(&["ollama"]), "ready={ready}");
        }
    }

    #[test]
    fn offline_mode_selects_the_same_as_local() {
        let configured = names(&["openai", "anthropic", "ollama"]);
        for ready in [true, false] {
            assert_eq!(
                select_providers(Mode::Offline, &configured, ready),
                select_providers(Mode::Local, &configured, ready),
                "ready={ready}"
            );
        }
    }

    #[test]
    fn local_and_offline_are_empty_when_ollama_is_not_configured() {
        let configured = names(&["openai", "anthropic"]);
        assert_eq!(select_providers(Mode::Local, &configured, true), Vec::<String>::new());
        assert_eq!(select_providers(Mode::Offline, &configured, true), Vec::<String>::new());
    }

    #[test]
    fn auto_mode_puts_local_first_when_ready() {
        let configured = names(&["openai", "anthropic", "ollama"]);
        assert_eq!(
            select_providers(Mode::Auto, &configured, true),
            names(&["ollama", "openai", "anthropic"])
        );
    }

    #[test]
    fn auto_mode_is_cloud_only_when_not_ready() {
        let configured = names(&["openai", "anthropic", "ollama"]);
        assert_eq!(select_providers(Mode::Auto, &configured, false), names(&["openai", "anthropic"]));
    }

    #[test]
    fn auto_mode_is_cloud_only_when_ollama_not_configured_even_if_ready_is_somehow_true() {
        let configured = names(&["openai", "anthropic"]);
        assert_eq!(select_providers(Mode::Auto, &configured, true), names(&["openai", "anthropic"]));
    }

    #[test]
    fn every_mode_on_empty_configuration_is_empty() {
        for mode in [Mode::Cloud, Mode::Local, Mode::Auto, Mode::Offline] {
            assert_eq!(select_providers(mode, &[], true), Vec::<String>::new(), "failed for {mode:?}");
            assert_eq!(select_providers(mode, &[], false), Vec::<String>::new(), "failed for {mode:?}");
        }
    }

    #[test]
    fn unrecognized_provider_names_are_treated_as_cloud_for_selection_purposes() {
        // select_providers only sorts into "local" (is_local_provider_name)
        // vs "everything else" -- an unknown name (e.g. a future provider
        // this function hasn't been taught about yet, or a typo) rides
        // along as "cloud" rather than vanishing silently. `build_chain`
        // separately drops names it doesn't recognize when constructing
        // providers, which is where an actually-unknown name has no effect.
        let configured = names(&["mystery-provider", "ollama"]);
        assert_eq!(select_providers(Mode::Cloud, &configured, false), names(&["mystery-provider"]));
    }

    // -- should_probe_ollama --------------------------------------------

    #[test]
    fn should_probe_only_when_ollama_is_named_and_has_a_base_url() {
        assert!(should_probe_ollama(&names(&["openai", "ollama"]), "http://127.0.0.1:11434"));
        assert!(!should_probe_ollama(&names(&["openai"]), "http://127.0.0.1:11434"), "ollama not opted in");
        assert!(!should_probe_ollama(&names(&["ollama"]), ""), "blank base_url");
        assert!(!should_probe_ollama(&names(&["ollama"]), "   "), "whitespace-only base_url");
    }

    // -- classify_host: IPv4 -----------------------------------------------

    #[test]
    fn ipv4_loopback_range_is_loopback() {
        for host in ["http://127.0.0.1/", "http://127.0.0.2/", "http://127.255.255.255/", "http://127.1.2.3/"] {
            assert_eq!(classify_host(host), HostClass::Loopback, "failed for {host}");
        }
    }

    #[test]
    fn ipv4_outside_loopback_range_is_not_loopback() {
        for host in ["http://126.0.0.1/", "http://128.0.0.1/", "http://10.0.0.1/", "http://8.8.8.8/"] {
            assert_eq!(classify_host(host), HostClass::NotLoopback, "failed for {host}");
        }
    }

    #[test]
    fn classify_host_respects_ports() {
        assert_eq!(classify_host("http://127.0.0.1:11434/api/chat"), HostClass::Loopback);
        assert_eq!(classify_host("http://evil.com:11434/"), HostClass::NotLoopback);
    }

    #[test]
    fn classify_host_is_case_insensitive() {
        assert_eq!(classify_host("HTTP://127.0.0.1:11434/"), HostClass::Loopback);
        assert_eq!(classify_host("http://LOCALHOST:11434/"), HostClass::Localhost);
    }

    // -- classify_host: localhost -------------------------------------------

    #[test]
    fn localhost_is_its_own_class_not_loopback() {
        assert_eq!(classify_host("http://localhost:11434/"), HostClass::Localhost);
        assert_ne!(HostClass::Localhost, HostClass::Loopback);
    }

    // -- classify_host: hostname tricks --------------------------------------

    #[test]
    fn a_hostname_that_merely_starts_with_the_loopback_address_is_not_loopback() {
        assert_eq!(classify_host("http://127.0.0.1.evil.com/"), HostClass::NotLoopback);
    }

    #[test]
    fn userinfo_claiming_to_be_loopback_does_not_fool_the_real_host() {
        assert_eq!(classify_host("http://127.0.0.1@evil.com/"), HostClass::NotLoopback);
    }

    #[test]
    fn userinfo_before_a_genuinely_loopback_host_is_still_loopback() {
        assert_eq!(classify_host("http://evil.com@127.0.0.1/"), HostClass::Loopback);
    }

    // -- classify_host: IPv6 -------------------------------------------------

    #[test]
    fn ipv6_loopback_bracketed_forms_are_loopback() {
        for host in ["http://[::1]/", "http://[::1]:11434/", "HTTP://[::1]:11434/api"] {
            assert_eq!(classify_host(host), HostClass::Loopback, "failed for {host}");
        }
    }

    #[test]
    fn ipv4_mapped_ipv6_loopback_is_loopback_decided_and_documented() {
        // ::ffff:127.0.0.1 genuinely denotes the IPv4 loopback address
        // under the IPv4-mapped IPv6 convention -- recognizing it does not
        // widen what's allowed, it just recognizes another spelling of the
        // same allowed address. See `classify_host`'s doc comment.
        for host in ["http://[::ffff:127.0.0.1]/", "http://[::FFFF:127.0.0.1]:11434/"] {
            assert_eq!(classify_host(host), HostClass::Loopback, "failed for {host}");
        }
    }

    #[test]
    fn ipv4_mapped_ipv6_non_loopback_is_not_loopback() {
        assert_eq!(classify_host("http://[::ffff:8.8.8.8]/"), HostClass::NotLoopback);
    }

    #[test]
    fn other_ipv6_loopback_spellings_are_conservatively_not_recognized() {
        // Decided and documented in `classify_host`'s doc comment: only
        // `::1` and `::ffff:a.b.c.d` are recognized. This fails closed
        // (refused), not open, so it is safe -- just narrower than a full
        // RFC 4291 implementation would be.
        assert_eq!(classify_host("http://[0:0:0:0:0:0:0:1]/"), HostClass::NotLoopback);
    }

    #[test]
    fn unparseable_or_empty_host_fails_closed() {
        for host in ["", "http://", "http:///path", "not a url at all"] {
            assert_eq!(classify_host(host), HostClass::NotLoopback, "failed for {host:?}");
        }
    }

    // -- model_present_in_tags / ollama_ready_from_tags_response -----------

    #[test]
    fn model_present_in_tags_matches_by_name() {
        let body = r#"{"models":[{"name":"gemma3:4b","model":"gemma3:4b"},{"name":"qwen3:14b"}]}"#;
        assert!(model_present_in_tags(body, "gemma3:4b"));
        assert!(model_present_in_tags(body, "qwen3:14b"));
        assert!(!model_present_in_tags(body, "llama3.1:8b"));
    }

    #[test]
    fn model_present_in_tags_handles_missing_models_key() {
        assert!(!model_present_in_tags(r#"{"other":1}"#, "gemma3:4b"));
    }

    #[test]
    fn model_present_in_tags_handles_empty_list() {
        assert!(!model_present_in_tags(r#"{"models":[]}"#, "gemma3:4b"));
    }

    #[test]
    fn model_present_in_tags_handles_malformed_json() {
        assert!(!model_present_in_tags("not json", "gemma3:4b"));
    }

    #[test]
    fn ollama_ready_from_tags_response_false_on_transport_error() {
        let result: Result<String, String> = Err("connection refused".to_string());
        assert!(!ollama_ready_from_tags_response(result, "gemma3:4b"));
    }

    #[test]
    fn ollama_ready_from_tags_response_true_when_model_listed() {
        let result: Result<String, String> = Ok(r#"{"models":[{"name":"gemma3:4b"}]}"#.to_string());
        assert!(ollama_ready_from_tags_response(result, "gemma3:4b"));
    }
}
