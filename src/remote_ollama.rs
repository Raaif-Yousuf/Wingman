//! #806: a warning, shown once, when Ollama is not on this PC.
//!
//! Decision: a remote Ollama `base_url` (a home server, another machine on
//! the network) stays allowed in Local mode, because it is still Ollama. But
//! the screenshot then leaves this PC, so the first press that would send to
//! such an address shows a card saying so instead, and remembers it. Every
//! press after that goes through. An address on this PC (127.0.0.1, `::1`,
//! `localhost`) never shows anything.
//!
//! The check is pure string parsing and never resolves DNS (a request must
//! not wait on a lookup just to be warned about it). A hostname other than
//! `localhost` cannot be shown to be this PC without a lookup, so it counts
//! as another machine: the safe side is to warn.
//!
//! What is remembered is the address that was warned about, in a small file
//! next to the crash note, plus a copy in memory for the running session so
//! a file that cannot be written never traps the user in a loop. Changing
//! the address warns again.

use std::path::PathBuf;
use std::sync::Mutex;

/// Whether `url`'s host is this PC: a loopback address or `localhost`.
/// Everything else (a LAN address, a hostname, an address that cannot be
/// parsed) is not. Pure; never resolves DNS.
pub fn is_on_this_pc(url: &str) -> bool {
    matches!(
        crate::mode::classify_host(url),
        crate::mode::HostClass::Loopback | crate::mode::HostClass::Localhost
    )
}

/// The host to name on the warning card, or `None` when `url` is on this PC
/// or has no host at all (such a request fails on its own, with its own
/// card). Lowercase, without scheme, port or path.
pub fn remote_address(url: &str) -> Option<String> {
    let url = url.trim();
    if is_on_this_pc(url) {
        return None;
    }
    crate::mode::extract_host(url)
}

/// The address to warn about for this press, or `None` when no warning is
/// due: Ollama is on this PC, or `acknowledged` already names this address.
/// Pure.
pub fn warning_due(url: &str, acknowledged: Option<&str>) -> Option<String> {
    let address = remote_address(url)?;
    if acknowledged.is_some_and(|a| a.trim().eq_ignore_ascii_case(&address)) {
        return None;
    }
    Some(address)
}

/// Headline and detail for the warning card. No em dash (rule 11).
pub fn card_text(address: &str) -> (String, String) {
    (
        "Ollama is not on this PC".to_string(),
        format!(
            "Your Ollama address points to {address}. Your screenshot and any selected text \
             will be sent there. This message shows once for this address. Try \
             again to go on, or open Settings to change it."
        ),
    )
}

/// Whether this press can reach Ollama at all: never in Offline mode (the
/// offline guard refuses a request to a non-loopback address, so there is no
/// send to warn about), otherwise when `ollama` is in the selected chain.
/// In Auto mode the chain assumes Ollama is up; whether it really is needs a
/// network probe, so a press that ends up on a cloud provider can still show
/// the card for an address that was not used. Pure.
pub fn notice_applies(mode: crate::mode::Mode, selected: &[String]) -> bool {
    mode != crate::mode::Mode::Offline && selected.iter().any(|n| n == "ollama")
}

/// The address the running session already warned about, so a file that
/// cannot be written does not warn on every press.
static SESSION_ACK: Mutex<Option<String>> = Mutex::new(None);

/// `%LOCALAPPDATA%\Wingman\remote-ollama-ack.txt`. AGENTS.md rule 9: under
/// `#[cfg(test)]` a process-scoped file in `%TEMP%`, never the real path.
fn ack_path() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!(
            "wingman-test-remote-ollama-ack-{}.txt",
            std::process::id()
        )))
    }
    #[cfg(not(test))]
    {
        Ok(crate::known_folder::local_app_data()?
            .join("Wingman")
            .join("remote-ollama-ack.txt"))
    }
}

/// The address last warned about: this session's, else the file's. A file
/// that is missing or unreadable reads as "nothing warned yet", so the
/// warning is shown rather than skipped.
fn acknowledged() -> Option<String> {
    if let Ok(guard) = SESSION_ACK.lock() {
        if let Some(a) = guard.as_ref() {
            return Some(a.clone());
        }
    }
    let path = ack_path().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Remembers that `address` was warned about, in memory and on disk. The
/// disk write is best effort: the in-memory copy still holds for this run.
fn remember(address: &str) {
    if let Ok(mut guard) = SESSION_ACK.lock() {
        *guard = Some(address.to_string());
    }
    if let Ok(path) = ack_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, address);
    }
}

/// For one press that would ask Ollama at `base_url`: the card to show
/// instead of sending, the first time for this address. The address is
/// remembered before this returns, so the next press goes through.
pub fn take_notice(base_url: &str) -> Option<(String, String)> {
    // Only an address that is not on this PC touches the file at all.
    remote_address(base_url)?;
    let address = warning_due(base_url, acknowledged().as_deref())?;
    remember(&address);
    Some(card_text(&address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_on_this_pc_is_never_remote() {
        for url in [
            "http://127.0.0.1:11434",
            "http://127.0.0.1:11434/api/chat",
            "http://127.1.2.3:11434",
            "http://[::1]:11434",
            "http://localhost:11434",
            "http://LOCALHOST:11434",
        ] {
            assert!(is_on_this_pc(url), "{url}");
            assert_eq!(remote_address(url), None, "{url}");
            assert_eq!(warning_due(url, None), None, "{url}");
        }
    }

    #[test]
    fn a_lan_address_or_a_hostname_is_remote() {
        for (url, host) in [
            ("http://192.168.1.20:11434", "192.168.1.20"),
            ("http://10.0.0.5:11434/", "10.0.0.5"),
            ("http://homeserver:11434", "homeserver"),
            ("http://Gaming-PC.local:11434", "gaming-pc.local"),
            ("https://ollama.example.com", "ollama.example.com"),
        ] {
            assert!(!is_on_this_pc(url), "{url}");
            assert_eq!(remote_address(url).as_deref(), Some(host), "{url}");
        }
    }

    #[test]
    fn a_loopback_lookalike_is_remote() {
        // The host here is evil.com; the loopback address is only userinfo.
        assert!(!is_on_this_pc("http://127.0.0.1@evil.com:11434"));
        assert_eq!(
            remote_address("http://127.0.0.1@evil.com:11434").as_deref(),
            Some("evil.com")
        );
    }

    #[test]
    fn an_address_with_no_host_gives_no_warning_and_does_not_panic() {
        for url in ["", "   ", "http://", "http:///path", "://", "http://@"] {
            assert_eq!(remote_address(url), None, "{url:?}");
            assert_eq!(warning_due(url, None), None, "{url:?}");
        }
    }

    #[test]
    fn the_warning_is_due_until_that_address_was_acknowledged() {
        let url = "http://192.168.1.20:11434";
        assert_eq!(warning_due(url, None).as_deref(), Some("192.168.1.20"));
        assert_eq!(warning_due(url, Some("")), warning_due(url, None));
        assert_eq!(warning_due(url, Some("192.168.1.20")), None);
        assert_eq!(warning_due(url, Some(" 192.168.1.20 ")), None);
        // A different address warns again.
        assert_eq!(
            warning_due("http://192.168.1.21:11434", Some("192.168.1.20")).as_deref(),
            Some("192.168.1.21")
        );
    }

    #[test]
    fn the_notice_never_applies_offline_or_without_ollama_in_the_chain() {
        use crate::mode::Mode;
        let with = vec!["ollama".to_string(), "openai".to_string()];
        let without = vec!["openai".to_string()];
        assert!(notice_applies(Mode::Local, &with));
        assert!(notice_applies(Mode::Auto, &with));
        assert!(!notice_applies(Mode::Offline, &with));
        assert!(!notice_applies(Mode::Local, &without));
        assert!(!notice_applies(Mode::Local, &[]));
    }

    #[test]
    fn the_card_names_the_address_and_has_no_em_dash() {
        let (headline, detail) = card_text("homeserver");
        assert!(detail.contains("homeserver"), "{detail}");
        assert!(detail.contains("screenshot"), "{detail}");
        assert!(detail.contains("once"), "{detail}");
        // #945: the card can come from the key, the tray or the options card, so
        // it must not tell the user which one to use.
        assert!(!detail.to_lowercase().contains("key"), "{detail}");
        assert!(detail.contains("Try again"), "{detail}");
        assert!(!headline.contains('\u{2014}') && !detail.contains('\u{2014}'));
    }

    /// The whole flow against the test-only file: the first notice for an
    /// address shows, the next does not, a different address shows again,
    /// and 127.0.0.1 never shows. One test, because the session copy and
    /// the file are process-wide.
    #[test]
    fn a_notice_shows_once_per_address_and_never_for_loopback() {
        let _ = std::fs::remove_file(ack_path().unwrap());
        *SESSION_ACK.lock().unwrap() = None;

        assert_eq!(take_notice("http://127.0.0.1:11434"), None);

        let first = take_notice("http://192.168.1.20:11434");
        assert!(first.is_some());
        assert_eq!(take_notice("http://192.168.1.20:11434"), None);

        // Remembered across a restart too: clear the session copy, keep the file.
        *SESSION_ACK.lock().unwrap() = None;
        assert_eq!(take_notice("http://192.168.1.20:11434"), None);

        assert!(take_notice("http://192.168.1.21:11434").is_some());
        assert_eq!(take_notice("http://127.0.0.1:11434"), None);

        let _ = std::fs::remove_file(ack_path().unwrap());
        *SESSION_ACK.lock().unwrap() = None;
    }
}
