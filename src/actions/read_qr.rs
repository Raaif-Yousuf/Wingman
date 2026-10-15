//! "Read QR code from screen" (#313): the second model-free built-in action.
//! Screen -> `rqrr` decode -> the `"clipboard"` executor -> a result card. No
//! provider, no schema, no network call of any kind, so it works in Offline
//! mode and with no provider configured, exactly like
//! [`super::extract_text`] (which this module copies the shape of; see its
//! module doc for why nothing here takes a `Providers` or `Mode`).
//!
//! Every QR code on the active monitor is decoded. One code: its text is
//! copied and shown on the card. Several: all texts are copied, one per
//! line, and listed. None: a card saying so, and the clipboard is left alone.
//!
//! A link inside a QR code is shown, never opened: opening a URL a stranger
//! put on screen is a classic phishing vector, and Wingman does not act
//! without the user asking. The card spells out the real host (the part
//! after any `user@`, which is the trick `https://bank.com@evil.example`
//! relies on) so the user can judge it before they go anywhere.

use anyhow::{Context, Result};
use serde_json::json;

use crate::capture::RawShot;
use crate::executors::Executor;
use crate::provider::Answer;
use crate::ui::confirm::{auto_confirm_read_only, Proposal};

/// The longest stretch of decoded text shown on the card. The clipboard
/// always gets the full text; this only keeps a huge payload (a vCard, a
/// pasted essay) from overflowing the card.
const MAX_SHOWN_CHARS: usize = 500;

/// What a scan of one screenshot found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scan {
    /// Decoded texts in the order the detector found them, identical codes
    /// collapsed to one (a poster that repeats one code is still one code).
    pub texts: Vec<String>,
    /// Codes that were located but could not be decoded (too small, blurry,
    /// damaged beyond error correction).
    pub unreadable: usize,
}

/// Find and decode every QR code in an RGBA image. Pure CPU work, so it runs
/// on the worker thread; a 4K screenshot takes a fraction of a second.
pub fn scan(rgba: &[u8], width: u32, height: u32) -> Scan {
    let (w, h) = (width as usize, height as usize);
    // A short or empty buffer must never index out of range (the worker
    // thread may not panic: AGENTS.md rule 7).
    if w == 0 || h == 0 || w.checked_mul(h).and_then(|n| n.checked_mul(4)) != Some(rgba.len()) {
        return Scan::default();
    }
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| {
        let i = (y * w + x) * 4;
        // Rec. 601 luma in integers: no float work per pixel.
        ((u32::from(rgba[i]) * 299 + u32::from(rgba[i + 1]) * 587 + u32::from(rgba[i + 2]) * 114)
            / 1000) as u8
    });
    let mut out = Scan::default();
    for grid in img.detect_grids() {
        match grid.decode() {
            Ok((_meta, text)) => {
                if !out.texts.contains(&text) {
                    out.texts.push(text);
                }
            }
            Err(_) => out.unreadable += 1,
        }
    }
    out
}

/// The host of an `http`/`https` URL, as a browser would take it: after the
/// scheme, before the first `/`, `?` or `#`, with any `user:pass@` prefix
/// removed and any port dropped. `None` for anything that is not a plain
/// web link (Wi-Fi payloads, `mailto:`, plain text, `javascript:`).
pub fn url_host(text: &str) -> Option<String> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &text["https://".len()..]
    } else if lower.starts_with("http://") {
        &text["http://".len()..]
    } else {
        return None;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        // IPv6 literal: keep the brackets, drop the port after `]`.
        let end = stripped.find(']')?;
        &host_port[..end + 2]
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Whether the URL's authority carries a `user@` part. Browsers ignore it
/// for routing, and attackers use it to make `https://bank.com@evil.example`
/// read like a bank link.
fn has_userinfo(text: &str) -> bool {
    let rest = text
        .trim()
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or_default();
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .contains('@')
}

fn shown(text: &str) -> String {
    if text.chars().count() <= MAX_SHOWN_CHARS {
        text.to_string()
    } else {
        let cut: String = text.chars().take(MAX_SHOWN_CHARS).collect();
        format!("{cut}... (shortened here, the whole text is on the clipboard)")
    }
}

/// One code's lines on the card: the text, then, for a web link, where it
/// goes and that it was not opened.
fn describe(text: &str) -> String {
    let mut out = shown(text);
    if let Some(host) = url_host(text) {
        out.push_str(&format!("\nSite: {host}. Not opened."));
        if has_userinfo(text) {
            out.push_str(" The address has a name before the site, which can disguise it.");
        }
    }
    out
}

/// What goes on the clipboard: the one text, or all of them one per line.
pub fn clipboard_text(scan: &Scan) -> Option<String> {
    if scan.texts.is_empty() {
        None
    } else {
        Some(scan.texts.join("\n"))
    }
}

/// The result card for a scan. Pure, so every outcome is unit-tested.
pub fn card_for(scan: &Scan) -> Answer {
    let (headline, detail) = match scan.texts.as_slice() {
        [] if scan.unreadable > 0 => (
            "Found a QR code but couldn't read it".to_string(),
            "Zoom in on the code or make it bigger on screen, then try again.".to_string(),
        ),
        [] => (
            "No QR code found on screen".to_string(),
            "Make sure the whole code is visible on this monitor.".to_string(),
        ),
        [only] => ("Copied the QR code text".to_string(), describe(only)),
        many => (
            format!("Copied {} QR codes", many.len()),
            many.iter()
                .enumerate()
                .map(|(i, t)| format!("{}. {}", i + 1, describe(t)))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    };
    Answer {
        headline,
        detail,
        difficulty: None,
    }
}

/// Scan `raw`, and if any code was decoded copy its text through `executor`
/// (must be `Effect::ReadOnly`, enforced by [`auto_confirm_read_only`]: this
/// only writes the system clipboard). Nothing found leaves the clipboard
/// untouched.
pub fn run_pipeline(raw: &RawShot, executor: &dyn Executor) -> Result<Answer> {
    let found = scan(&raw.rgba, raw.width, raw.height);
    if let Some(text) = clipboard_text(&found) {
        let proposal = Proposal::new(json!({ "text": text }));
        let confirmed = auto_confirm_read_only(executor, proposal)
            .context("read QR code: executor is not read-only")?;
        executor
            .execute(confirmed)
            .context("Couldn't copy to the clipboard")?;
    }
    Ok(card_for(&found))
}

/// The real, non-test pipeline: decode, then copy via the `"clipboard"`
/// executor resolved by name. Runs on a worker thread.
pub fn read_and_copy(raw: &RawShot) -> Result<Answer> {
    let executor = crate::executors::registry::resolve("clipboard")
        .context("read QR code: couldn't resolve the clipboard executor")?;
    run_pipeline(raw, executor.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executors::{Effect, Undo};
    use qrcode::{Color, QrCode};
    use std::cell::RefCell;

    const QUIET: usize = 4; // quiet-zone modules around a code
    const SCALE: usize = 6; // pixels per module

    /// A white RGBA canvas with `codes` drawn on it at the given pixel
    /// offsets. Generated here so no binary fixture is committed.
    fn canvas(width: usize, height: usize, codes: &[(&str, usize, usize)]) -> RawShot {
        let mut rgba = vec![255u8; width * height * 4];
        for (text, ox, oy) in codes {
            let code = QrCode::new(text.as_bytes()).expect("encodable");
            let n = code.width();
            for my in 0..n {
                for mx in 0..n {
                    if code[(mx, my)] != Color::Dark {
                        continue;
                    }
                    for dy in 0..SCALE {
                        for dx in 0..SCALE {
                            let x = ox + (mx + QUIET) * SCALE + dx;
                            let y = oy + (my + QUIET) * SCALE + dy;
                            let i = (y * width + x) * 4;
                            rgba[i..i + 3].copy_from_slice(&[0, 0, 0]);
                        }
                    }
                }
            }
        }
        RawShot {
            rgba,
            width: width as u32,
            height: height as u32,
        }
    }

    #[derive(Default)]
    struct RecordingExecutor {
        copied: RefCell<Option<String>>,
    }
    // Single-threaded test double, same promise `extract_text`'s tests make.
    unsafe impl Sync for RecordingExecutor {}
    impl Executor for RecordingExecutor {
        fn name(&self) -> &'static str {
            "recording"
        }
        fn effect(&self) -> Effect {
            Effect::ReadOnly
        }
        fn execute(
            &self,
            confirmed: crate::ui::confirm::Confirmed<serde_json::Value>,
        ) -> Result<Undo> {
            let value = confirmed.into_value();
            *self.copied.borrow_mut() =
                value.get("text").and_then(|v| v.as_str()).map(String::from);
            Ok(Undo::none("test"))
        }
    }

    // -- decode ------------------------------------------------------------

    #[test]
    fn decodes_one_code_exactly() {
        let shot = canvas(400, 400, &[("https://example.com", 50, 50)]);
        let found = scan(&shot.rgba, shot.width, shot.height);
        assert_eq!(found.texts, vec!["https://example.com".to_string()]);
        assert_eq!(found.unreadable, 0);
    }

    #[test]
    fn decodes_every_code_on_the_screen() {
        let shot = canvas(
            900,
            400,
            &[
                ("first code", 20, 20),
                ("https://example.org/two", 450, 100),
            ],
        );
        let found = scan(&shot.rgba, shot.width, shot.height);
        assert_eq!(found.texts.len(), 2, "{found:?}");
        assert!(found.texts.contains(&"first code".to_string()));
        assert!(found.texts.contains(&"https://example.org/two".to_string()));
    }

    #[test]
    fn the_same_code_twice_is_reported_once() {
        let shot = canvas(900, 400, &[("same", 20, 20), ("same", 450, 100)]);
        let found = scan(&shot.rgba, shot.width, shot.height);
        assert_eq!(found.texts, vec!["same".to_string()]);
    }

    #[test]
    fn a_blank_screen_has_no_codes() {
        let shot = canvas(300, 300, &[]);
        assert_eq!(scan(&shot.rgba, shot.width, shot.height), Scan::default());
    }

    #[test]
    fn malformed_buffers_do_not_panic() {
        assert_eq!(scan(&[], 0, 0), Scan::default());
        assert_eq!(scan(&[0; 7], 2, 2), Scan::default());
    }

    // -- url_host ------------------------------------------------------------

    #[test]
    fn url_host_takes_the_real_site() {
        assert_eq!(url_host("https://example.com"), Some("example.com".into()));
        assert_eq!(
            url_host("HTTP://Example.COM:8080/a/b?q=1#x"),
            Some("example.com".into())
        );
        // The disguise: everything before `@` is a user name, not the site.
        assert_eq!(
            url_host("https://bank.com@evil.example/login"),
            Some("evil.example".into())
        );
        assert_eq!(url_host("http://[::1]:80/x"), Some("[::1]".into()));
    }

    #[test]
    fn url_host_is_none_for_anything_that_is_not_a_web_link() {
        for text in [
            "just some text",
            "WIFI:T:WPA;S:cafe;P:secret;;",
            "mailto:a@b.c",
            "javascript:alert(1)",
            "https://",
            "https:///path",
            "https://has space.com",
        ] {
            assert_eq!(url_host(text), None, "{text}");
        }
    }

    // -- the card ------------------------------------------------------------

    #[test]
    fn card_for_a_url_shows_it_and_its_host_and_says_it_was_not_opened() {
        let card = card_for(&Scan {
            texts: vec!["https://example.com".into()],
            unreadable: 0,
        });
        assert_eq!(card.headline, "Copied the QR code text");
        assert!(card.detail.starts_with("https://example.com"));
        assert!(card.detail.contains("Site: example.com. Not opened."));
    }

    #[test]
    fn card_warns_when_the_address_has_a_name_before_the_site() {
        let card = card_for(&Scan {
            texts: vec!["https://bank.com@evil.example/".into()],
            unreadable: 0,
        });
        assert!(
            card.detail.contains("Site: evil.example."),
            "{}",
            card.detail
        );
        assert!(card.detail.contains("disguise"), "{}", card.detail);
    }

    #[test]
    fn card_for_plain_text_shows_the_text_and_no_site_line() {
        let card = card_for(&Scan {
            texts: vec!["hello".into()],
            unreadable: 0,
        });
        assert_eq!(card.detail, "hello");
    }

    #[test]
    fn card_for_several_codes_numbers_them() {
        let card = card_for(&Scan {
            texts: vec!["a".into(), "https://example.com".into()],
            unreadable: 0,
        });
        assert_eq!(card.headline, "Copied 2 QR codes");
        assert!(card.detail.starts_with("1. a\n2. https://example.com"));
    }

    #[test]
    fn card_for_nothing_found_and_for_unreadable_are_different_and_honest() {
        let none = card_for(&Scan::default());
        assert_eq!(none.headline, "No QR code found on screen");
        let bad = card_for(&Scan {
            texts: vec![],
            unreadable: 1,
        });
        assert_eq!(bad.headline, "Found a QR code but couldn't read it");
    }

    #[test]
    fn a_huge_payload_is_shortened_on_the_card_only() {
        let long = "x".repeat(2000);
        let scan = Scan {
            texts: vec![long.clone()],
            unreadable: 0,
        };
        assert!(card_for(&scan).detail.chars().count() < 700);
        assert_eq!(clipboard_text(&scan), Some(long));
    }

    #[test]
    fn card_text_has_no_em_dashes() {
        let scans = [
            Scan::default(),
            Scan {
                texts: vec![],
                unreadable: 2,
            },
            Scan {
                texts: vec!["https://bank.com@evil.example/".into(), "b".into()],
                unreadable: 0,
            },
        ];
        for s in &scans {
            let c = card_for(s);
            assert!(!c.headline.contains('\u{2014}'), "{}", c.headline);
            assert!(!c.detail.contains('\u{2014}'), "{}", c.detail);
        }
    }

    // -- pipeline ------------------------------------------------------------

    #[test]
    fn pipeline_copies_exactly_the_decoded_text() {
        let shot = canvas(400, 400, &[("https://example.com", 50, 50)]);
        let exec = RecordingExecutor::default();
        let card = run_pipeline(&shot, &exec).expect("must succeed");
        assert_eq!(exec.copied.borrow().as_deref(), Some("https://example.com"));
        assert!(card.detail.contains("https://example.com"));
    }

    #[test]
    fn pipeline_copies_all_codes_one_per_line() {
        let shot = canvas(900, 400, &[("one", 20, 20), ("two", 450, 100)]);
        let exec = RecordingExecutor::default();
        run_pipeline(&shot, &exec).expect("must succeed");
        let copied = exec.copied.borrow().clone().expect("copied");
        let mut lines: Vec<&str> = copied.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, vec!["one", "two"]);
    }

    #[test]
    fn pipeline_leaves_the_clipboard_alone_when_nothing_is_found() {
        let shot = canvas(300, 300, &[]);
        let exec = RecordingExecutor::default();
        let card = run_pipeline(&shot, &exec).expect("must succeed");
        assert_eq!(card.headline, "No QR code found on screen");
        assert!(exec.copied.borrow().is_none());
    }

    #[test]
    fn pipeline_takes_no_providers_or_mode_parameter() {
        fn assert_shape<F: Fn(&RawShot, &dyn Executor) -> Result<Answer>>(_f: F) {}
        assert_shape(run_pipeline);
    }
}
