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

/// The longest side, in pixels, the detector is ever given. `rqrr` has no
/// time limit of its own: on a noisy 4K frame it ran for about 24 minutes in
/// the test profile and overflowed in a debug build (#528). Larger frames are
/// shrunk by a whole-number factor first, which bounds the pixel count. The
/// shrink alone does not bound the run time: MEASURED 2026-10-15, a native
/// 1920x1080 frame of finder-like squares still took seconds and could panic
/// inside `rqrr`, so [`MAX_FINDER_CANDIDATES`] is the second limit. A code
/// smaller than a few pixels per module after shrinking is reported as not
/// found.
const MAX_SIDE: usize = 1920;

/// The most finder-like shapes (see [`finder_candidates`]) a frame may hold
/// before the detector is not run at all. MEASURED 2026-10-15 (release, native
/// 1920x1080, finder squares scattered at random): 20 squares took 44 ms, 100
/// took 236 ms, 300 took 2.9 s, 600 took 6.1 s, and 1500 made `rqrr` itself panic
/// (`geometry.rs`: `x <= i32::MAX`), which aborts a release build. A real
/// screen holds three per code, plus a few false hits inside the data. Since
/// the count was made to match `rqrr`'s own row test (#537) a real code counts
/// about 10 (MEASURED 2026-10-16: 6 codes on one 1920x1080 screen counted 60),
/// so 150 covers about 15 codes on one screen.
const MAX_FINDER_CANDIDATES: usize = 150;

/// `grey` as `rqrr` sees it: dark or not, decided against a running local
/// average of the surrounding pixels (a snake scan, a window of one eighth of
/// the width, 5% under the average), not at a fixed level. This repeats the
/// thresholding in `rqrr`'s `PreparedImage::prepare` (MIT OR Apache-2.0, see
/// `THIRD_PARTY_NOTICES.md`) because `rqrr` does not expose its binarised
/// image, and the shape count below is only safe if it reads the same dark and
/// light pixels `rqrr` reads (#537). Pure.
fn binarize_like_rqrr(grey: &Grey) -> Vec<bool> {
    let (w, h) = (grey.w, grey.h);
    let mut dark = vec![false; w * h];
    let mut row_average = vec![0usize; w];
    let (mut avg_v, mut avg_u) = (0usize, 0usize);
    let window = (w / 8).max(1);
    for y in 0..h {
        let row = &grey.px[y * w..(y + 1) * w];
        row_average.fill(0);
        for x in 0..w {
            let (v, u) = if y % 2 == 0 {
                (w - 1 - x, x)
            } else {
                (x, w - 1 - x)
            };
            avg_v = avg_v * (window - 1) / window + usize::from(row[v]);
            avg_u = avg_u * (window - 1) / window + usize::from(row[u]);
            row_average[v] += avg_v;
            row_average[u] += avg_u;
        }
        for x in 0..w {
            dark[y * w + x] = usize::from(row[x]) < row_average[x] * 95 / (200 * window);
        }
    }
    dark
}

/// Whether five consecutive runs (first one dark) pass `rqrr`'s row test for a
/// finder pattern: every run within three quarters of the average of the four
/// outer runs, around 1:1:3:1:1. Deliberately the same arithmetic as `rqrr`'s
/// `LineScanner::test_for_capstone`, which is much looser than an exact ratio.
/// Returns the pattern's width.
fn rqrr_row_hit(win: &[(usize, usize, bool)]) -> Option<usize> {
    const CHECK: [usize; 5] = [1, 1, 3, 1, 1];
    let len = |i: usize| win[i].1;
    let avg = (len(0) + len(1) + len(3) + len(4)) / 4;
    let err = avg * 3 / 4;
    (0..5)
        .all(|i| len(i) + err >= CHECK[i] * avg && len(i) <= CHECK[i] * avg + err)
        .then(|| win.iter().map(|r| r.1).sum())
}

/// How many finder-pattern-like shapes `grey` holds, counting up to `cap + 1`
/// and then stopping. It must never count fewer than `rqrr` will find (#537):
/// `rqrr` is not safe to run on a frame that holds hundreds. So it reads the
/// same binarised pixels ([`binarize_like_rqrr`]) with the same row test
/// ([`rqrr_row_hit`]), and counts every row hit, not only the ones `rqrr` goes
/// on to confirm. Hits in consecutive rows at about the same place are one
/// shape, because `rqrr` paints a found shape over so its lower rows do not hit
/// again. A shape may span a single row. Over-counting only refuses a very
/// busy frame; under-counting lets `rqrr` panic. Not a detector. `rqrr` scans
/// rows only, so this does too.
fn finder_candidates(grey: &Grey, cap: usize) -> usize {
    let dark = binarize_like_rqrr(grey);
    // (centre x, unit, first row, last row)
    let mut open: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut count = 0usize;
    for y in 0..grey.h {
        let row = &dark[y * grey.w..(y + 1) * grey.w];
        // Runs of equal darkness in this row: (start, length, dark).
        let mut runs: Vec<(usize, usize, bool)> = Vec::new();
        for (x, &d) in row.iter().enumerate() {
            match runs.last_mut() {
                Some(r) if r.2 == d => r.1 += 1,
                _ => runs.push((x, 1, d)),
            }
        }
        for win in runs.windows(5) {
            if !win[0].2 {
                continue;
            }
            let Some(total) = rqrr_row_hit(win) else {
                continue;
            };
            let unit = (total / 7).max(1);
            let cx = win[0].0 + total / 2;
            match open
                .iter_mut()
                .find(|o| o.0.abs_diff(cx) <= unit.max(o.1) && y - o.3 <= 1)
            {
                Some(o) => {
                    o.0 = cx;
                    o.3 = y;
                }
                None => open.push((cx, unit, y, y)),
            }
        }
        // Shapes whose last hit is older than the previous row are finished.
        open.retain(|o| {
            if y - o.3 <= 1 {
                return true;
            }
            count += 1;
            false
        });
        if count + open.len() > cap {
            return count + open.len();
        }
    }
    count + open.len()
}

/// A greyscale image: `px.len() == w * h`.
struct Grey {
    px: Vec<u8>,
    w: usize,
    h: usize,
}

/// `rgba` as greyscale, shrunk by the smallest whole-number factor (box
/// average) that brings both sides to at most `max_side`. Pure. The caller
/// has already checked that `rgba` is exactly `w * h * 4` bytes.
fn bounded_grey(rgba: &[u8], w: usize, h: usize, max_side: usize) -> Grey {
    let factor = w.max(h).div_ceil(max_side).max(1);
    let (ow, oh) = (w.div_ceil(factor), h.div_ceil(factor));
    let mut px = Vec::with_capacity(ow * oh);
    for oy in 0..oh {
        for ox in 0..ow {
            let (x1, y1) = (((ox + 1) * factor).min(w), ((oy + 1) * factor).min(h));
            let (mut sum, mut n) = (0u32, 0u32);
            for y in oy * factor..y1 {
                for x in ox * factor..x1 {
                    let i = (y * w + x) * 4;
                    // Rec. 601 luma in integers: no float work per pixel.
                    sum += (u32::from(rgba[i]) * 299
                        + u32::from(rgba[i + 1]) * 587
                        + u32::from(rgba[i + 2]) * 114)
                        / 1000;
                    n += 1;
                }
            }
            px.push((sum / n.max(1)) as u8);
        }
    }
    Grey { px, w: ow, h: oh }
}

/// Find and decode every QR code in an RGBA image. Pure CPU work, so it runs
/// on the worker thread. The detector only ever sees an image of at most
/// [`MAX_SIDE`] pixels on a side, so the time is bounded.
pub fn scan(rgba: &[u8], width: u32, height: u32) -> Scan {
    let (w, h) = (width as usize, height as usize);
    // A short or empty buffer must never index out of range (the worker
    // thread may not panic: AGENTS.md rule 7).
    if w == 0 || h == 0 || w.checked_mul(h).and_then(|n| n.checked_mul(4)) != Some(rgba.len()) {
        return Scan::default();
    }
    let grey = bounded_grey(rgba, w, h, MAX_SIDE);
    if finder_candidates(&grey, MAX_FINDER_CANDIDATES) > MAX_FINDER_CANDIDATES {
        // Too busy to be worth decoding, and past the point where `rqrr` is
        // safe to run. Reported as a code that could not be read.
        return Scan {
            texts: Vec::new(),
            unreadable: 1,
        };
    }
    let mut img =
        rqrr::PreparedImage::prepare_from_greyscale(grey.w, grey.h, |x, y| grey.px[y * grey.w + x]);
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

/// The authority part (`user@host:port`) of an `http`/`https` link, the way a
/// browser would cut it out. Browsers drop tabs and newlines anywhere in a
/// URL, ignore spaces and control characters at its ends, accept any run of
/// `/` and `\` after `http:` or `https:`, and end the authority at the first
/// `/`, `\`, `?` or `#`. `None` for anything that is not an `http` or
/// `https` link.
fn authority_of(text: &str) -> Option<String> {
    let cleaned: String = text
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let cleaned = cleaned.trim_matches(|c: char| c <= ' ');
    let lower = cleaned.to_ascii_lowercase();
    let rest = if lower.starts_with("https:") {
        &cleaned["https:".len()..]
    } else if lower.starts_with("http:") {
        &cleaned["http:".len()..]
    } else {
        return None;
    };
    let rest = rest.trim_start_matches(['/', '\\']);
    Some(
        rest.split(['/', '\\', '?', '#'])
            .next()
            .unwrap_or("")
            .to_string(),
    )
}

/// One host label in its ASCII form: unchanged if it is already ASCII,
/// otherwise Punycode (RFC 3492) with the `xn--` prefix, which is what the
/// address really is on the wire and exposes look-alike letters
/// (`xn--pple-43d` is not `apple`).
fn ascii_label(label: &str) -> String {
    if label.is_ascii() {
        return label.to_string();
    }
    const BASE: u32 = 36;
    const TMIN: u32 = 1;
    const TMAX: u32 = 26;
    fn digit(d: u32) -> char {
        if d < 26 {
            (b'a' + d as u8) as char
        } else {
            (b'0' + (d - 26) as u8) as char
        }
    }
    fn adapt(delta: u32, points: u32, first: bool) -> u32 {
        let mut delta = if first { delta / 700 } else { delta / 2 };
        delta += delta / points;
        let mut k = 0;
        while delta > ((BASE - TMIN) * TMAX) / 2 {
            delta /= BASE - TMIN;
            k += BASE;
        }
        k + (BASE - TMIN + 1) * delta / (delta + 38)
    }
    let encode = || -> Option<String> {
        let input: Vec<u32> = label.chars().map(u32::from).collect();
        let mut out: String = label.chars().filter(char::is_ascii).collect();
        let basic = out.len() as u32;
        let mut handled = basic;
        if basic > 0 {
            out.push('-');
        }
        let (mut n, mut delta, mut bias) = (128u32, 0u32, 72u32);
        while (handled as usize) < input.len() {
            let m = input.iter().copied().filter(|&c| c >= n).min()?;
            delta = delta.checked_add((m - n).checked_mul(handled + 1)?)?;
            n = m;
            for &c in &input {
                if c < n {
                    delta = delta.checked_add(1)?;
                }
                if c == n {
                    let (mut q, mut k) = (delta, BASE);
                    loop {
                        let t = if k <= bias {
                            TMIN
                        } else if k >= bias + TMAX {
                            TMAX
                        } else {
                            k - bias
                        };
                        if q < t {
                            break;
                        }
                        out.push(digit(t + (q - t) % (BASE - t)));
                        q = (q - t) / (BASE - t);
                        k += BASE;
                    }
                    out.push(digit(q));
                    bias = adapt(delta, handled + 1, handled == basic);
                    delta = 0;
                    handled += 1;
                }
            }
            delta += 1;
            n += 1;
        }
        Some(format!("xn--{out}"))
    };
    encode().unwrap_or_else(|| "xn--(unreadable)".to_string())
}

/// The host of an `http`/`https` URL as a browser would take it (see
/// [`authority_of`]), with any `user:pass@` prefix removed, any port dropped,
/// letters lower-cased and any non-ASCII label shown in its Punycode form.
/// `None` for anything that is not a plain web link (Wi-Fi payloads,
/// `mailto:`, plain text, `javascript:`).
pub fn url_host(text: &str) -> Option<String> {
    let authority = authority_of(text)?;
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        // IPv6 literal: keep the brackets, drop the port after `]`.
        let end = stripped.find(']')?;
        &host_port[..end + 2]
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let host = host.to_lowercase();
    Some(
        host.split('.')
            .map(ascii_label)
            .collect::<Vec<_>>()
            .join("."),
    )
}

/// Whether the URL's authority carries a `user@` part. Browsers ignore it
/// for routing, and attackers use it to make `https://bank.com@evil.example`
/// read like a bank link.
fn has_userinfo(text: &str) -> bool {
    authority_of(text).is_some_and(|a| a.contains('@'))
}

/// Whether a payload starts like `scheme:something` (`javascript:...`,
/// `file:///...`, `ms-settings:...`, `WIFI:...`) as opposed to plain text.
fn looks_like_scheme(text: &str) -> bool {
    let text = text.trim_start();
    let mut chars = text.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let mut rest = chars.as_str();
    let scheme_len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')))
        .unwrap_or(rest.len());
    rest = &rest[scheme_len..];
    rest.strip_prefix(':')
        .and_then(|after| after.chars().next())
        .is_some_and(|c| !c.is_whitespace())
}

/// Characters that can reorder or hide what a person reads, or forge a line:
/// shown as a visible `\u{..}` mark instead of being drawn.
fn is_invisible_or_reordering(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'
                | '\u{2066}'..='\u{2069}'
                | '\u{061C}'
                | '\u{FEFF}'
        )
}

/// `text` made safe to draw on the card: newlines and tabs as `\n`, `\r`,
/// `\t`, other control, invisible and direction-changing characters as
/// `\u{..}`. The clipboard keeps the exact text; only the card is escaped, so
/// a payload cannot forge a "Site:" line or flip the order of what is shown.
fn escape_for_card(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if is_invisible_or_reordering(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn shown(text: &str) -> String {
    let text = escape_for_card(text);
    if text.chars().count() <= MAX_SHOWN_CHARS {
        text
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
        if host.split('.').any(|label| label.starts_with("xn--")) {
            out.push_str(
                " It uses non-English letters, which can imitate English ones. \
                 Check the address carefully.",
            );
        }
        if has_userinfo(text) {
            out.push_str(" The address has a name before the site, which can disguise it.");
        }
    } else if looks_like_scheme(text) {
        out.push_str("\nNot a web link. Not opened.");
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
            screen: None,
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
        // A browser reads extra slashes after the scheme as one.
        assert_eq!(url_host("https:///path"), Some("path".into()));
    }

    #[test]
    fn url_host_is_none_for_anything_that_is_not_a_web_link() {
        for text in [
            "just some text",
            "WIFI:T:WPA;S:cafe;P:secret;;",
            "mailto:a@b.c",
            "javascript:alert(1)",
            "https://",
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

    // -- how a browser reads the address ------------------------------------

    #[test]
    fn a_backslash_ends_the_authority_like_a_slash() {
        // A browser sends this to evil.example, not bank.com.
        assert_eq!(
            url_host("https://evil.example\\@bank.com/"),
            Some("evil.example".into())
        );
        assert!(!has_userinfo("https://evil.example\\@bank.com/"));
        assert_eq!(
            url_host("http://bank.com\\@evil.example"),
            Some("bank.com".into())
        );
        assert_eq!(
            url_host("https:\\\\evil.example/x"),
            Some("evil.example".into())
        );
        let card = card_for(&Scan {
            texts: vec!["https://evil.example\\@bank.com/".into()],
            unreadable: 0,
        });
        assert!(
            card.detail.contains("Site: evil.example."),
            "{}",
            card.detail
        );
        assert!(!card.detail.contains("disguise"), "{}", card.detail);
    }

    #[test]
    fn tabs_and_newlines_inside_an_address_do_not_hide_the_site() {
        assert_eq!(
            url_host("https://evil.exa\nmple/\t"),
            Some("evil.example".into())
        );
    }

    // -- what the card draws -------------------------------------------------

    #[test]
    fn a_newline_in_the_payload_cannot_forge_a_site_line() {
        let payload = "https://evil.example/\nSite: bank.com. Not opened.";
        let card = card_for(&Scan {
            texts: vec![payload.into()],
            unreadable: 0,
        });
        let site_lines: Vec<&str> = card
            .detail
            .lines()
            .filter(|l| l.starts_with("Site:"))
            .collect();
        assert_eq!(site_lines.len(), 1, "{}", card.detail);
        assert!(site_lines[0].starts_with("Site: evil.example."));
        // Plain text too: it stays on one line.
        let plain = card_for(&Scan {
            texts: vec!["hi\nSite: bank.com. Not opened.".into()],
            unreadable: 0,
        });
        assert_eq!(plain.detail.lines().count(), 1, "{}", plain.detail);
        // The clipboard keeps the exact text.
        let scan = Scan {
            texts: vec![payload.into()],
            unreadable: 0,
        };
        assert_eq!(clipboard_text(&scan).as_deref(), Some(payload));
    }

    #[test]
    fn direction_and_invisible_characters_are_shown_as_marks() {
        let payload = "pay\u{202E}moc.knab\u{202C} \u{2066}x\u{2069}\u{200E}\u{200F}\u{200B}";
        let card = card_for(&Scan {
            texts: vec![payload.into()],
            unreadable: 0,
        });
        for c in [
            '\u{202E}', '\u{202C}', '\u{2066}', '\u{2069}', '\u{200E}', '\u{200F}', '\u{200B}',
        ] {
            assert!(!card.detail.contains(c), "{c:?} drawn raw: {}", card.detail);
        }
        assert!(card.detail.contains("\\u{202e}"), "{}", card.detail);
    }

    #[test]
    fn a_look_alike_host_is_shown_in_punycode_and_flagged() {
        // The first letter is Cyrillic U+0430, not Latin "a".
        assert_eq!(
            url_host("https://\u{0430}pple.com/"),
            Some("xn--pple-43d.com".into())
        );
        assert_eq!(
            url_host("https://B\u{00FC}cher.example"),
            Some("xn--bcher-kva.example".into())
        );
        let card = card_for(&Scan {
            texts: vec!["https://\u{0430}pple.com/".into()],
            unreadable: 0,
        });
        assert!(
            card.detail.contains("Site: xn--pple-43d.com."),
            "{}",
            card.detail
        );
        assert!(
            card.detail.contains("non-English letters"),
            "{}",
            card.detail
        );
        let plain = card_for(&Scan {
            texts: vec!["https://apple.com/".into()],
            unreadable: 0,
        });
        assert!(!plain.detail.contains("non-English"), "{}", plain.detail);
    }

    #[test]
    fn other_schemes_are_called_not_a_web_link() {
        for text in [
            "javascript:alert(1)",
            "file:///C:/Windows/System32/calc.exe",
            "ms-settings:network",
            "WIFI:T:WPA;S:cafe;P:secret;;",
        ] {
            let card = card_for(&Scan {
                texts: vec![text.into()],
                unreadable: 0,
            });
            assert!(
                card.detail.ends_with("\nNot a web link. Not opened."),
                "{text}: {}",
                card.detail
            );
        }
        for text in ["hello", "Note: buy milk", "12:30"] {
            let card = card_for(&Scan {
                texts: vec![text.into()],
                unreadable: 0,
            });
            assert_eq!(card.detail, text);
        }
    }

    // -- robustness ----------------------------------------------------------

    /// A 4K frame full of noise and finder-pattern-like squares, the worst
    /// plausible input for the detector. It must come back without panicking
    /// (the release build aborts on panic). Prints the time as a measurement.
    /// Fast because the frame is shrunk and counted before detection (#528): well under a second in the
    /// test profile.
    #[test]
    fn scan_survives_a_4k_frame_of_finder_pattern_noise() {
        let (w, h) = (3840usize, 2160usize);
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut rgba = vec![255u8; w * h * 4];
        for px in rgba.chunks_exact_mut(4) {
            let v = if next() % 1000 < 5 { 0 } else { 255 };
            px[..3].fill(v);
        }
        for _ in 0..300 {
            let (ox, oy) = (next() as usize % (w - 40), next() as usize % (h - 40));
            for y in 0..35usize {
                for x in 0..35usize {
                    let (mx, my) = (x / 5, y / 5);
                    let ring = mx.min(6 - mx).min(my.min(6 - my));
                    let dark = ring != 1;
                    let i = ((oy + y) * w + ox + x) * 4;
                    rgba[i..i + 3].fill(if dark { 0 } else { 255 });
                }
            }
        }
        let started = std::time::Instant::now();
        let found = scan(&rgba, w as u32, h as u32);
        eprintln!(
            "MEASURED: scan of 3840x2160 noise took {:?} (test profile), {} texts, {} unreadable",
            started.elapsed(),
            found.texts.len(),
            found.unreadable
        );
        for text in &found.texts {
            assert!(!card_for(&Scan {
                texts: vec![text.clone()],
                unreadable: 0
            })
            .detail
            .is_empty());
        }
    }

    #[test]
    fn the_detector_never_sees_more_than_the_bound() {
        // Dimensions only: 4K, 8K, an odd size that does not divide evenly,
        // and a frame exactly at the bound.
        for (w, h) in [
            (3840usize, 2160usize),
            (7680, 4320),
            (3841, 2161),
            (1920, 1080),
        ] {
            let rgba = vec![0u8; w * h * 4];
            let g = bounded_grey(&rgba, w, h, MAX_SIDE);
            assert!(
                g.w <= MAX_SIDE && g.h <= MAX_SIDE,
                "{w}x{h} -> {}x{}",
                g.w,
                g.h
            );
            assert_eq!(g.px.len(), g.w * g.h);
        }
        // Frames at or under the bound are not touched.
        let g = bounded_grey(&vec![0u8; 800 * 600 * 4], 800, 600, MAX_SIDE);
        assert_eq!((g.w, g.h), (800, 600));
    }

    #[test]
    fn shrinking_keeps_the_picture_and_a_code_still_reads_on_a_4k_screen() {
        let mut rgba = vec![255u8; 8];
        rgba[..4].copy_from_slice(&[0, 0, 0, 255]);
        // 2x1 frame untouched: first pixel black, second white.
        let g = bounded_grey(&rgba, 2, 1, MAX_SIDE);
        assert_eq!(g.px, vec![0, 255]);
        let shot = canvas(3840, 2160, &[("https://example.com/4k", 1000, 800)]);
        let found = scan(&shot.rgba, shot.width, shot.height);
        assert_eq!(found.texts, vec!["https://example.com/4k".to_string()]);
    }

    #[test]
    fn averaging_is_exact_with_a_factor_of_two_and_a_partial_edge_block() {
        // 5x3 frame, max side 2 -> factor 3, output 2x1. Grey values (red
        // channel only, so luma = v * 299 / 1000 is not used: use grey pixels
        // v,v,v where luma = v exactly).
        let vals: [[u8; 5]; 3] = [
            [0, 30, 60, 90, 120],
            [10, 40, 70, 100, 130],
            [20, 50, 80, 110, 140],
        ];
        let mut rgba = Vec::new();
        for row in vals {
            for v in row {
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        // Luma of (v,v,v) is (v*299 + v*587 + v*114)/1000 = v exactly.
        let g = bounded_grey(&rgba, 5, 3, 2);
        assert_eq!((g.w, g.h), (2, 1));
        // Left block: columns 0..3 (9 pixels): sum 0+30+60+10+40+70+20+50+80 = 360 -> 40.
        // Right block: columns 3..5 (6 pixels, partial): 90+120+100+130+110+140 = 690 -> 115.
        assert_eq!(g.px, vec![40, 115]);
        // Factor exactly 2 on an even frame.
        let rgba: Vec<u8> = [0u8, 100, 200, 100]
            .iter()
            .flat_map(|&v| [v, v, v, 255])
            .collect();
        let g = bounded_grey(&rgba, 2, 2, 1);
        assert_eq!((g.w, g.h, g.px), (1, 1, vec![100]));
    }

    /// Finder-pattern-like noise at a native size, no shrinking involved.
    fn noise_frame(w: usize, h: usize, squares: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut rgba = vec![255u8; w * h * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[..3].fill(if next() % 1000 < 5 { 0 } else { 255 });
        }
        if w > 40 && h > 40 {
            for _ in 0..squares {
                let (ox, oy) = (next() as usize % (w - 40), next() as usize % (h - 40));
                for y in 0..35usize {
                    for x in 0..35usize {
                        let (mx, my) = (x / 5, y / 5);
                        let dark = mx.min(6 - mx).min(my.min(6 - my)) != 1;
                        let i = ((oy + y) * w + ox + x) * 4;
                        rgba[i..i + 3].fill(if dark { 0 } else { 255 });
                    }
                }
            }
        }
        rgba
    }

    #[test]
    fn finder_count_sees_real_codes_and_ignores_plain_noise() {
        let one = canvas(400, 400, &[("https://example.com", 50, 50)]);
        let g = bounded_grey(&one.rgba, 400, 400, MAX_SIDE);
        // A real code counts a few more than its three finders (false hits in
        // the data), far under the cap.
        let n = finder_candidates(&g, 1000);
        assert!((3..=20).contains(&n), "{n}");
        let blank = bounded_grey(&vec![255u8; 300 * 300 * 4], 300, 300, MAX_SIDE);
        assert_eq!(finder_candidates(&blank, 1000), 0);
        // Counting stops soon after the cap.
        let many = noise_frame(1920, 1080, 600);
        let g = bounded_grey(&many, 1920, 1080, MAX_SIDE);
        assert!(finder_candidates(&g, MAX_FINDER_CANDIDATES) > MAX_FINDER_CANDIDATES);
    }

    #[test]
    fn a_frame_with_too_many_finder_shapes_is_refused_not_scanned() {
        // 1500 shapes made rqrr panic (release aborts); 600 took 6 s.
        for squares in [300usize, 600, 1500] {
            let rgba = noise_frame(1920, 1080, squares);
            let started = std::time::Instant::now();
            let found = scan(&rgba, 1920, 1080);
            let took = started.elapsed();
            eprintln!("MEASURED: {squares} finder squares at 1920x1080 refused in {took:?}");
            assert!(found.texts.is_empty());
            assert_eq!(found.unreadable, 1);
            assert!(took.as_secs() < 2, "{took:?}");
        }
    }

    /// `n` finder-like squares on a plain light frame, laid out on a grid so
    /// none overlap. Each is a dark ring of `ring` pixels, a light gap of
    /// `gap`, and a dark stone of `stone`, so a row through its middle reads
    /// `ring, gap, stone, gap, ring`. Grey levels are `dark` and `light`.
    fn grid_frame(
        (w, h): (usize, usize),
        n: usize,
        (ring, gap, stone): (usize, usize, usize),
        (dark, light): (u8, u8),
    ) -> Vec<u8> {
        let mut rgba = vec![light; w * h * 4];
        let size = 2 * (ring + gap) + stone;
        let pitch = size + 12;
        let cols = (w - 8) / pitch;
        assert!(cols > 0 && n <= cols * ((h - 8) / pitch), "grid too small");
        let mut fill = |x0: usize, y0: usize, side: usize, v: u8| {
            for y in y0..y0 + side {
                for x in x0..x0 + side {
                    let i = (y * w + x) * 4;
                    rgba[i..i + 3].fill(v);
                }
            }
        };
        for k in 0..n {
            let (ox, oy) = (4 + (k % cols) * pitch, 4 + (k / cols) * pitch);
            fill(ox, oy, size, dark);
            fill(ox + ring, oy + ring, size - 2 * ring, light);
            fill(ox + ring + gap, oy + ring + gap, stone, dark);
        }
        rgba
    }

    /// #537: the counter must never count fewer shapes than `rqrr` will find.
    /// `rqrr` accepts a row if each run is within 75% of the average of the
    /// four outer runs, far looser than an exact 1:1:3:1:1, so a ring that is
    /// seven times its gap is still a finder to it.
    #[test]
    fn loose_ratio_finder_shapes_are_counted_like_rqrr_counts_them() {
        let rgba = grid_frame((1920, 1080), 400, (7, 1, 12), (0, 255));
        let g = bounded_grey(&rgba, 1920, 1080, MAX_SIDE);
        let n = finder_candidates(&g, MAX_FINDER_CANDIDATES);
        assert!(n > MAX_FINDER_CANDIDATES, "counted only {n}");
        let started = std::time::Instant::now();
        let found = scan(&rgba, 1920, 1080);
        assert_eq!(found.unreadable, 1, "{found:?}");
        assert!(started.elapsed().as_secs() < 2, "{:?}", started.elapsed());
    }

    /// #537: `rqrr` thresholds against a running local average, not at a fixed
    /// 128, so a pale-grey-on-white code is a code to it.
    #[test]
    fn low_contrast_finder_shapes_are_counted_like_rqrr_counts_them() {
        let rgba = grid_frame((1920, 1080), 400, (5, 5, 15), (150, 220));
        let g = bounded_grey(&rgba, 1920, 1080, MAX_SIDE);
        let n = finder_candidates(&g, MAX_FINDER_CANDIDATES);
        assert!(n > MAX_FINDER_CANDIDATES, "counted only {n}");
        let started = std::time::Instant::now();
        let found = scan(&rgba, 1920, 1080);
        assert_eq!(found.unreadable, 1, "{found:?}");
        assert!(started.elapsed().as_secs() < 2, "{:?}", started.elapsed());
    }

    /// The count is now conservative, so check it did not tip into refusing a
    /// screen that really holds a handful of codes (#537).
    #[test]
    fn a_screen_with_six_real_codes_is_not_refused() {
        let codes: Vec<(String, usize, usize)> = (0..6)
            .map(|i| {
                (
                    format!("https://example.com/{i}"),
                    20 + (i % 3) * 600,
                    20 + (i / 3) * 400,
                )
            })
            .collect();
        let refs: Vec<(&str, usize, usize)> =
            codes.iter().map(|(t, x, y)| (t.as_str(), *x, *y)).collect();
        let shot = canvas(1920, 1080, &refs);
        let g = bounded_grey(&shot.rgba, 1920, 1080, MAX_SIDE);
        let n = finder_candidates(&g, 1000);
        eprintln!("MEASURED: 6 real codes at 1920x1080 count {n} shapes");
        assert!(n <= MAX_FINDER_CANDIDATES / 2, "{n}");
        let found = scan(&shot.rgba, shot.width, shot.height);
        assert_eq!(found.texts.len(), 6, "{found:?}");
    }

    #[test]
    fn a_busy_but_plausible_frame_is_still_scanned_in_time() {
        let rgba = noise_frame(1920, 1080, 20);
        let started = std::time::Instant::now();
        let found = scan(&rgba, 1920, 1080);
        let took = started.elapsed();
        eprintln!("MEASURED: 20 finder squares at 1920x1080 scanned in {took:?}");
        assert_ne!(found.unreadable, 1, "refused a frame under the cap");
        assert!(took.as_secs() < 5, "{took:?}");
    }

    #[test]
    fn native_extreme_aspect_frames_do_not_panic_or_crawl() {
        for (w, h) in [(1usize, 5000usize), (5000, 1), (5000, 3)] {
            let rgba = noise_frame(w, h, 0);
            let started = std::time::Instant::now();
            let _ = scan(&rgba, w as u32, h as u32);
            let took = started.elapsed();
            eprintln!("MEASURED: native {w}x{h} scan took {took:?}");
            assert!(took.as_secs() < 5, "{w}x{h}: {took:?}");
        }
    }

    #[test]
    fn tiny_frames_do_not_panic() {
        for (w, h) in [(1u32, 1u32), (3, 3), (7, 7), (21, 21), (1, 500), (500, 1)] {
            let rgba = vec![0u8; (w * h * 4) as usize];
            let _ = scan(&rgba, w, h);
            let rgba = vec![255u8; (w * h * 4) as usize];
            let _ = scan(&rgba, w, h);
        }
    }
}
