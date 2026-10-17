//! #373: "Report a problem" on an error card. Builds a link to a new GitHub
//! issue with the title and body filled in, and the app opens it in the
//! browser. Wingman sends nothing itself: the user reads the form on GitHub
//! and decides whether to submit it.
//!
//! What goes in is a short fixed vocabulary: the version, the mode, the kind
//! of each provider in the chain, and a coarse class of the error. The error
//! text itself is never copied, so nothing from the screen, a model reply, a
//! key, a URL or a file path can reach the link. The user can paste the
//! scrubbed details ("Copy details") into the form if they want to.

/// The page the link opens.
const NEW_ISSUE_URL: &str = "https://github.com/Raaif-Yousuf/Wingman/issues/new";

/// The longest link built, in bytes. GitHub accepts far more; browsers and
/// shells are the tighter limit.
const MAX_URL_LEN: usize = 1800;

/// A coarse class of failure, picked from a fixed list. Never carries text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Offline,
    Timeout,
    Auth,
    RateLimit,
    Server,
    Network,
    Refusal,
    Parse,
    Other,
}

impl ErrorClass {
    pub fn label(self) -> &'static str {
        match self {
            ErrorClass::Offline => "offline mode",
            ErrorClass::Timeout => "timeout",
            ErrorClass::Auth => "key rejected",
            ErrorClass::RateLimit => "rate limit",
            ErrorClass::Server => "server error",
            ErrorClass::Network => "network",
            ErrorClass::Refusal => "model refused",
            ErrorClass::Parse => "unreadable reply",
            ErrorClass::Other => "other",
        }
    }
}

/// Picks the class of an error chain. Only used to choose a label; none of
/// `chain` is copied out.
pub fn classify(chain: &str) -> ErrorClass {
    let c = chain.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| c.contains(w));
    if has(&["offline mode", "offline guard"]) {
        ErrorClass::Offline
    } else if has(&["refus"]) {
        ErrorClass::Refusal
    } else if has(&["401", "403", "unauthorized", "forbidden", "api key"]) {
        ErrorClass::Auth
    } else if has(&["429", "rate limit", "too many requests"]) {
        ErrorClass::RateLimit
    } else if has(&["timed out", "timeout"]) {
        ErrorClass::Timeout
    } else if has(&["500", "502", "503", "504", "service unavailable"]) {
        ErrorClass::Server
    } else if has(&["parse", "json", "schema", "malformed"]) {
        ErrorClass::Parse
    } else if has(&["dns", "connect", "lookup", "network", "unreachable"]) {
        ErrorClass::Network
    } else {
        ErrorClass::Other
    }
}

/// The facts the link carries. Built by the caller from config and mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportInput {
    pub version: String,
    pub mode: String,
    /// Kinds only: `openai`, `anthropic`, `gemini`, `ollama`, `compat`.
    pub provider_kinds: Vec<String>,
    pub class: ErrorClass,
}

impl ReportInput {
    /// Reduces what the app knows to the fixed vocabulary: the mode's label,
    /// the kind of each known provider in `order`, and the class of `chain`.
    pub fn from_parts(mode_label: &str, order: &[String], chain: &str) -> Self {
        ReportInput {
            version: env!("CARGO_PKG_VERSION").to_string(),
            mode: mode_label.to_string(),
            provider_kinds: order
                .iter()
                .filter_map(|n| provider_kind(n))
                .map(String::from)
                .collect(),
            class: classify(chain),
        }
    }
}

/// The kind of one `providers.order` entry, or `None` for a name that is not
/// one of the known kinds. A `compat:<label>` entry is just `compat`: the
/// label is the user's own text.
pub fn provider_kind(order_name: &str) -> Option<&'static str> {
    match order_name {
        "openai" => Some("openai"),
        "anthropic" => Some("anthropic"),
        "gemini" => Some("gemini"),
        "ollama" => Some("ollama"),
        n if n.starts_with("compat:") => Some("compat"),
        _ => None,
    }
}

/// Percent-encodes `s` and stops before the encoded form passes `budget`
/// bytes, so a character or an escape is never cut in half.
pub fn encode_capped(s: &str, budget: usize) -> String {
    let mut out = String::new();
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        let mut piece = String::new();
        for &b in ch.encode_utf8(&mut buf).as_bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                piece.push(b as char);
            } else {
                piece.push_str(&format!("%{b:02X}"));
            }
        }
        if out.len() + piece.len() > budget {
            break;
        }
        out.push_str(&piece);
    }
    out
}

/// The link to open. Always starts with [`NEW_ISSUE_URL`] and is never longer
/// than [`MAX_URL_LEN`].
pub fn build_url(input: &ReportInput) -> String {
    let title = format!("Problem: {}", input.class.label());
    let kinds = if input.provider_kinds.is_empty() {
        "none".to_string()
    } else {
        input.provider_kinds.join(", ")
    };
    let body = format!(
        "What happened:


What I expected:


---
Version: {}
Mode: {}
Providers: {}
Error class: {}
",
        input.version,
        input.mode,
        kinds,
        input.class.label(),
    );
    let head = format!("{NEW_ISSUE_URL}?title=");
    let title = encode_capped(&title, 200);
    let mid = "&body=";
    let room = MAX_URL_LEN.saturating_sub(head.len() + title.len() + mid.len());
    format!("{head}{title}{mid}{}", encode_capped(&body, room))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ReportInput {
        ReportInput {
            version: "0.2.0".into(),
            mode: "Auto".into(),
            provider_kinds: vec!["anthropic".into(), "ollama".into()],
            class: ErrorClass::Timeout,
        }
    }

    fn decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                assert!(i + 3 <= b.len(), "cut escape");
                let h = std::str::from_utf8(&b[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(h, 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("encoded text must decode to whole characters")
    }

    #[test]
    fn url_points_at_the_new_issue_page_with_title_and_body() {
        let url = build_url(&input());
        assert!(url.starts_with("https://github.com/Raaif-Yousuf/Wingman/issues/new?title="));
        assert!(url.contains("&body="));
        let d = decode(&url);
        assert!(d.contains("0.2.0"));
        assert!(d.contains("Auto"));
        assert!(d.contains("anthropic, ollama"));
        assert!(d.contains("timeout"));
    }

    #[test]
    fn url_is_plain_ascii_with_no_raw_spaces_or_newlines() {
        let url = build_url(&input());
        assert!(url.is_ascii());
        assert!(!url.contains(' ') && !url.contains('\n') && !url.contains('\r'));
    }

    #[test]
    fn classify_picks_a_class_from_the_chain() {
        assert_eq!(classify("HTTP 401: invalid api key"), ErrorClass::Auth);
        assert_eq!(
            classify("HTTP 429: too many requests"),
            ErrorClass::RateLimit
        );
        assert_eq!(classify("request timed out after 60s"), ErrorClass::Timeout);
        assert_eq!(classify("HTTP 503 service unavailable"), ErrorClass::Server);
        assert_eq!(
            classify("Offline mode blocked host api.openai.com"),
            ErrorClass::Offline
        );
        assert_eq!(classify("stop_reason: refusal"), ErrorClass::Refusal);
        assert_eq!(
            classify("could not parse the reply as JSON"),
            ErrorClass::Parse
        );
        assert_eq!(
            classify("dns error: failed to lookup address"),
            ErrorClass::Network
        );
        assert_eq!(classify("something odd"), ErrorClass::Other);
        assert_eq!(classify(""), ErrorClass::Other);
    }

    #[test]
    fn provider_kind_drops_the_users_compat_label_and_unknown_names() {
        assert_eq!(provider_kind("openai"), Some("openai"));
        assert_eq!(provider_kind("ollama"), Some("ollama"));
        assert_eq!(provider_kind("compat:my-secret-lab-box"), Some("compat"));
        assert_eq!(provider_kind("bogus"), None);
    }

    #[test]
    fn planted_secrets_paths_and_screen_text_never_reach_the_url() {
        // The chain carries everything that must stay out. Only the class
        // may come through it.
        let chain = "HTTP 401 from https://user:hunter2@api.example.com/v1?key=sk-ant-api03-PLANTEDSECRET1234567890 \
                     at C:\\Users\\planteduser\\AppData\\Roaming\\Wingman\\config.toml; \
                     model said: PLANTED_MODEL_OUTPUT; screen showed: PLANTED_SCREEN_TEXT";
        let mut i = input();
        i.class = classify(chain);
        i.provider_kinds = ["compat:planted-label", "openai"]
            .iter()
            .filter_map(|n| provider_kind(n))
            .map(String::from)
            .collect();
        let url = build_url(&i);
        let d = decode(&url);
        assert!(d.contains("key rejected"), "the class must come through");
        for bad in [
            "hunter2",
            "sk-ant",
            "PLANTEDSECRET",
            "planteduser",
            "config.toml",
            "PLANTED_MODEL_OUTPUT",
            "PLANTED_SCREEN_TEXT",
            "planted-label",
            "api.example.com",
        ] {
            assert!(!url.contains(bad) && !d.contains(bad), "leaked {bad}");
        }
    }

    #[test]
    fn from_parts_keeps_only_the_fixed_vocabulary() {
        let order = vec![
            "compat:planted-label".to_string(),
            "bogus".to_string(),
            "ollama".to_string(),
        ];
        let i = ReportInput::from_parts("Local", &order, "HTTP 429 for PLANTED");
        assert_eq!(i.provider_kinds, vec!["compat", "ollama"]);
        assert_eq!(i.class, ErrorClass::RateLimit);
        assert_eq!(i.mode, "Local");
        assert_eq!(i.version, env!("CARGO_PKG_VERSION"));
        assert!(!decode(&build_url(&i)).contains("PLANTED"));
    }

    #[test]
    fn encode_capped_encodes_reserved_and_non_ascii_bytes() {
        assert_eq!(encode_capped("a b&c=d#e", 100), "a%20b%26c%3Dd%23e");
        assert_eq!(encode_capped("\u{e9}", 100), "%C3%A9");
        assert_eq!(encode_capped("AZaz09-._~", 100), "AZaz09-._~");
    }

    #[test]
    fn encode_capped_never_splits_a_character_or_an_escape() {
        // An e-acute is 6 bytes encoded; a budget of 5 must give nothing of it.
        assert_eq!(encode_capped("\u{e9}", 5), "");
        assert_eq!(encode_capped("a\u{e9}", 4), "a");
        for budget in 0..40 {
            let out = encode_capped("a\u{1F600}b\u{e9}\n x", budget);
            assert!(out.len() <= budget);
            decode(&out);
        }
        assert_eq!(encode_capped("anything", 0), "");
    }

    #[test]
    fn a_huge_input_is_capped_and_does_not_panic() {
        let i = ReportInput {
            version: "9".repeat(50_000),
            mode: "\u{1F600}".repeat(5_000),
            provider_kinds: vec!["openai".to_string(); 5_000],
            class: ErrorClass::Other,
        };
        let url = build_url(&i);
        assert!(url.len() <= MAX_URL_LEN);
        assert!(url.starts_with(NEW_ISSUE_URL));
        decode(&url);
    }

    #[test]
    fn labels_have_no_em_dash() {
        for c in [
            ErrorClass::Offline,
            ErrorClass::Timeout,
            ErrorClass::Auth,
            ErrorClass::RateLimit,
            ErrorClass::Server,
            ErrorClass::Network,
            ErrorClass::Refusal,
            ErrorClass::Parse,
            ErrorClass::Other,
        ] {
            assert!(!c.label().contains('\u{2014}'));
        }
        assert!(!build_url(&input()).contains("%E2%80%94"));
    }
}
