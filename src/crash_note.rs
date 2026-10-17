//! Issue #227: a crash note, so a panic is never a silent disappearance.
//!
//! The release profile keeps `panic = "abort"`, so a panic on any thread
//! ends the process with no chance to show a card. A panic hook, installed
//! first thing in `main`, runs before the abort and writes a short note to
//! `%LOCALAPPDATA%\Wingman\crash-note.txt`. The next start reads it once,
//! deletes it and shows a card saying Wingman closed unexpectedly, with the
//! place in the code and the panic message ([`take_note`], [`card_text`]).
//!
//! # What the note may hold
//!
//! Version, thread name, a unix timestamp, the source location and, only
//! when the panic message is a string literal, that message. Never a
//! screenshot, OCR text, a key or the clipboard: the hook reads nothing of
//! the sort. The one place user data could sneak in is a *formatted* panic
//! message (`unwrap()` on an `Err` formats the error, and an error can quote
//! a selection, OCR text or a model reply), so [`message_of`] drops every
//! payload that is not a `&'static str` and the note carries the location
//! alone. What is kept still goes through [`sanitize`]: cut first (so the
//! quadratic profile redaction stays cheap), key-shaped tokens redacted,
//! account name stripped, one line, at most [`MAX_MESSAGE_CHARS`].
//!
//! # Debug builds
//!
//! With `panic = "unwind"` (dev and test builds) the process survives a
//! worker panic, so the hook is not installed there and no note is written:
//! a note would claim a crash that did not happen. `app::catch_worker` turns
//! such a panic into an ordinary error card instead.
//!
//! Nothing here polls or runs while idle: the hook only runs when a panic
//! does, and the note is read once at startup.

use std::path::{Path, PathBuf};

/// Longest panic message kept, in characters.
pub const MAX_MESSAGE_CHARS: usize = 160;

/// Input is cut to this many characters before any redaction pass runs.
const MAX_INPUT_CHARS: usize = 1024;

/// Longest location kept, in characters.
const MAX_LOCATION_CHARS: usize = 120;

/// Longest thread name kept, in characters.
const MAX_THREAD_CHARS: usize = 40;

/// What the hook knows about a panic, before sanitising.
pub struct PanicFacts<'a> {
    pub thread: &'a str,
    pub location: &'a str,
    pub message: &'a str,
    pub version: &'a str,
    pub unix_secs: u64,
}

/// `%LOCALAPPDATA%\Wingman\crash-note.txt`. Machine-scoped like the egress
/// log. AGENTS.md rule 9: under `#[cfg(test)]` this is a process-scoped file
/// in `%TEMP%`, never the real path.
pub fn note_path() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!(
            "wingman-test-crash-note-{}.txt",
            std::process::id()
        )))
    }
    #[cfg(not(test))]
    {
        Ok(crate::known_folder::local_app_data()?
            .join("Wingman")
            .join("crash-note.txt"))
    }
}

/// Folds `text` onto one line, redacts key-shaped tokens and the account
/// name, and cuts it to `max` characters (with an ellipsis when cut). Pure.
pub fn sanitize(text: &str, profile: &str, max: usize) -> String {
    let bounded: String = text.chars().take(MAX_INPUT_CHARS).collect();
    let one_line = bounded.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted = crate::egress::redact_opaque_tokens(&one_line);
    let redacted = crate::diagnostics::redact_user_profile(&redacted, profile);
    let (cut, was_cut) = crate::provider::truncate_chars(&redacted, max);
    if was_cut {
        format!("{cut}...")
    } else {
        cut
    }
}

/// The note's text: one `key: value` line each, every value sanitised.
pub fn format_note(facts: &PanicFacts, profile: &str) -> String {
    let mut note = format!(
        "version: {}\nthread: {}\ntime_unix: {}\nlocation: {}\n",
        sanitize(facts.version, profile, 20),
        sanitize(facts.thread, profile, MAX_THREAD_CHARS),
        facts.unix_secs,
        sanitize(facts.location, profile, MAX_LOCATION_CHARS),
    );
    let message = sanitize(facts.message, profile, MAX_MESSAGE_CHARS);
    if !message.is_empty() {
        note.push_str(&format!("message: {message}\n"));
    }
    note
}

/// The panic message, but only when the payload is a `&'static str`. A
/// `String` payload is a formatted message and may carry user data, so it
/// yields an empty string (see the module docs).
pub fn message_of(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|s| s.to_string())
        .unwrap_or_default()
}

thread_local! {
    static LAST_LOCATION: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Remembers where the most recent panic on this thread happened, so
/// `app::catch_worker` can name it in the error card in builds that unwind.
pub fn note_location(location: &str) {
    LAST_LOCATION.with(|l| *l.borrow_mut() = location.to_string());
}

/// The location [`note_location`] last saw on this thread, then forgotten.
pub fn take_location() -> String {
    LAST_LOCATION.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

/// The value after `key: ` in a note, if that line exists.
fn field<'a>(note: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}: ");
    note.lines()
        .find_map(|l| l.trim_end_matches('\r').strip_prefix(prefix.as_str()))
}

/// Headline and detail for the "closed unexpectedly" card. Pure. No em
/// dashes (AGENTS.md rule 11).
pub fn card_text(note: &str) -> (String, String) {
    let location = field(note, "location").unwrap_or("unknown");
    let message = field(note, "message").unwrap_or("");
    if location == STARTUP_LOCATION {
        let mut detail = "Wingman could not finish starting and closed.".to_string();
        if !message.is_empty() {
            detail.push_str(&format!("\n\nReason: {message}"));
        }
        detail.push_str("\n\nThis message shows once. Start Wingman again from the Start menu.");
        return ("Wingman could not start last time".to_string(), detail);
    }
    let mut detail = format!("Wingman stopped because of an internal error in {location}.");
    if !message.is_empty() {
        detail.push_str(&format!("\n\nError: {message}"));
    }
    detail.push_str(
        "\n\nUse Copy details on this card or Copy diagnostics (tray menu, Help and \
         diagnostics) to report it. \
         This message shows once.",
    );
    ("Wingman closed unexpectedly last time".to_string(), detail)
}

/// The `location` of a note written by [`record_startup_failure`].
const STARTUP_LOCATION: &str = "startup";

/// #1019: the note for a start that failed before a card could be shown. Pure;
/// the error text goes through [`sanitize`] like a panic message.
pub fn startup_failure_note(error: &str, profile: &str, unix_secs: u64) -> String {
    format_note(
        &PanicFacts {
            thread: "main",
            location: STARTUP_LOCATION,
            message: error,
            version: env!("CARGO_PKG_VERSION"),
            unix_secs,
        },
        profile,
    )
}

/// #1019: writes [`startup_failure_note`] for the next start to show as a
/// card. Best effort: a release build has no console, so the note is the only
/// trace of why the process vanished.
pub fn record_startup_failure(error: &str) {
    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let profile = crate::known_folder::user_profile()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if let Ok(path) = note_path() {
        let _ = write_note(&path, &startup_failure_note(error, &profile, unix_secs));
    }
}

/// Writes `note` to `path`, creating the folder. Best effort for the hook.
///
/// Written to a temp file in the same folder and renamed over `path`, so two
/// threads panicking together never leave a half-written note.
pub fn write_note(path: &Path, note: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tid: String = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.{tid}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, note)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Reads the note at `path` and clears it, so the card shows once. If the
/// file cannot be deleted it is truncated to empty instead; the note is
/// returned if either worked, so a crash is never silently swallowed. `None`
/// when there is no note, it is empty, or it could be neither deleted nor
/// emptied (it would otherwise show on every start).
pub fn take_note(path: &Path) -> Option<String> {
    let note = std::fs::read_to_string(path).ok()?;
    if std::fs::remove_file(path).is_err() && std::fs::File::create(path).is_err() {
        return None;
    }
    if note.trim().is_empty() {
        return None;
    }
    Some(note)
}

/// Installs the panic hook. Call first thing in `main`. Every panic records
/// its location ([`note_location`]); only a build that aborts on panic also
/// writes the note (see the module docs).
pub fn install_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        note_location(&location);
        if !cfg!(panic = "abort") {
            previous(info);
            return;
        }
        let message = message_of(info.payload());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let profile = crate::known_folder::user_profile()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let note = format_note(
            &PanicFacts {
                thread: &thread,
                location: &location,
                message: &message,
                version: env!("CARGO_PKG_VERSION"),
                unix_secs,
            },
            &profile,
        );
        if let Ok(path) = note_path() {
            let _ = write_note(&path, &note);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(message: &'a str, location: &'a str) -> PanicFacts<'a> {
        PanicFacts {
            thread: "worker",
            location,
            message,
            version: "1.2.3",
            unix_secs: 1_760_000_000,
        }
    }

    #[test]
    fn note_has_every_field() {
        let note = format_note(&facts("boom", "src\\app.rs:10:5"), "");
        assert_eq!(
            note,
            "version: 1.2.3\nthread: worker\ntime_unix: 1760000000\n\
             location: src\\app.rs:10:5\nmessage: boom\n"
        );
    }

    #[test]
    fn message_is_cut_and_folded_onto_one_line() {
        let long = format!("first\nsecond {}", "x ".repeat(200));
        let out = sanitize(&long, "", MAX_MESSAGE_CHARS);
        assert!(!out.contains('\n'));
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS + 3);
        assert!(out.ends_with("..."));
        assert!(out.starts_with("first second"));
    }

    #[test]
    fn short_message_is_untouched() {
        assert_eq!(
            sanitize("index out of bounds", "", 160),
            "index out of bounds"
        );
    }

    #[test]
    fn key_shaped_tokens_are_redacted() {
        let key = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        let out = sanitize(&format!("called unwrap on Err: bad key {key}"), "", 160);
        assert!(!out.contains("AbCdEfGhIjKl"), "{out}");
        assert!(!out.contains("0123456789"), "{out}");
    }

    #[test]
    fn account_name_is_removed_from_paths() {
        let out = sanitize(
            "C:\\Users\\jdoe\\.cargo\\registry\\src\\x.rs:1:1",
            "C:\\Users\\jdoe",
            160,
        );
        assert!(!out.contains("jdoe"), "{out}");
        assert!(out.contains("%USERPROFILE%"), "{out}");
    }

    #[test]
    fn a_note_with_hostile_values_stays_one_line_per_field() {
        let note = format_note(&facts("a\nversion: fake\nb", "loc\r\nx"), "");
        assert_eq!(note.lines().count(), 5);
    }

    #[test]
    fn card_text_names_the_location_and_message() {
        let note = format_note(&facts("boom", "src\\app.rs:10:5"), "");
        let (headline, detail) = card_text(&note);
        assert_eq!(headline, "Wingman closed unexpectedly last time");
        assert!(detail.contains("src\\app.rs:10:5"));
        assert!(detail.contains("Error: boom"));
        assert!(detail.contains("Copy diagnostics"));
        assert!(!detail.contains('\u{2014}'));
    }

    // #1019: a start that fails before any window can show a card leaves a
    // note, so the next start can say why.
    #[test]
    fn a_startup_failure_note_reads_as_a_startup_card() {
        let note = startup_failure_note("creating the notification card: no window", "", 7);
        let (headline, detail) = card_text(&note);
        assert_eq!(headline, "Wingman could not start last time");
        assert!(
            detail.contains("creating the notification card: no window"),
            "{detail}"
        );
        assert!(!detail.contains("internal error"), "{detail}");
        assert!(!detail.contains('\u{2014}'));
    }

    #[test]
    fn a_startup_failure_note_hides_the_account_name() {
        let note = startup_failure_note("cannot open C:\\Users\\jane\\x", "C:\\Users\\jane", 7);
        assert!(!note.contains("jane"), "{note}");
    }

    #[test]
    fn card_text_survives_a_damaged_note() {
        let (_, detail) = card_text("garbage");
        assert!(detail.contains("unknown"));
        assert!(!detail.contains("Error:"));
    }

    #[test]
    fn card_text_reads_crlf_notes() {
        let note = format_note(&facts("boom", "a.rs:1:1"), "").replace('\n', "\r\n");
        let (_, detail) = card_text(&note);
        assert!(detail.contains("a.rs:1:1."), "{detail}");
        assert!(detail.contains("Error: boom\n"), "{detail}");
    }

    fn temp_note(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wingman-test-crash-{name}-{}", std::process::id()))
    }

    #[test]
    fn only_a_literal_message_is_kept() {
        let literal: Box<dyn std::any::Any + Send> = Box::new("index out of bounds");
        let formatted: Box<dyn std::any::Any + Send> =
            Box::new(String::from("SECRET selection text"));
        assert_eq!(message_of(literal.as_ref()), "index out of bounds");
        assert_eq!(message_of(formatted.as_ref()), "");
    }

    #[test]
    fn a_note_without_a_message_still_names_the_location() {
        let note = format_note(&facts("", "src\\app.rs:10:5"), "");
        assert!(note.contains("location: src\\app.rs:10:5"));
        assert!(!note.contains("message:"));
        let (_, detail) = card_text(&note);
        assert!(detail.contains("src\\app.rs:10:5"));
        assert!(!detail.contains("Error:"));
    }

    #[test]
    fn huge_input_is_bounded_before_redaction() {
        let huge = "C:\\Users\\jdoe ".repeat(200_000);
        let out = sanitize(&huge, "C:\\Users\\jdoe", MAX_MESSAGE_CHARS);
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS + 3);
    }

    #[test]
    fn location_is_remembered_once_per_thread() {
        note_location("src\\a.rs:1:2");
        assert_eq!(take_location(), "src\\a.rs:1:2");
        assert_eq!(take_location(), "");
    }

    #[test]
    fn rewriting_a_note_replaces_it_and_leaves_no_temp_files() {
        let dir = temp_note("atomic");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("note.txt");
        write_note(&path, "first").unwrap();
        write_note(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A note that cannot be deleted (another process holds it without
    /// delete sharing) is emptied instead, and the card still shows once.
    #[test]
    fn an_undeletable_note_is_emptied_and_still_shown() {
        use std::os::windows::fs::OpenOptionsExt;
        let path = temp_note("locked");
        write_note(&path, "version: 1\nlocation: a.rs:1:1\n").unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        assert!(std::fs::remove_file(&path).is_err(), "precondition: locked");
        let taken = take_note(&path);
        drop(holder);
        assert_eq!(taken.as_deref(), Some("version: 1\nlocation: a.rs:1:1\n"));
        assert_eq!(take_note(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_note_is_read_once_then_gone() {
        let path = temp_note("once");
        let _ = std::fs::remove_file(&path);
        write_note(&path, "version: 1\nlocation: a.rs:1:1\n").unwrap();
        assert_eq!(
            take_note(&path).as_deref(),
            Some("version: 1\nlocation: a.rs:1:1\n")
        );
        assert!(!path.exists());
        assert_eq!(take_note(&path), None);
    }

    #[test]
    fn no_note_means_no_card() {
        assert_eq!(take_note(&temp_note("missing")), None);
    }

    #[test]
    fn an_empty_note_is_cleared_and_ignored() {
        let path = temp_note("empty");
        write_note(&path, "  \n").unwrap();
        assert_eq!(take_note(&path), None);
        assert!(!path.exists());
    }

    #[test]
    fn write_note_creates_the_folder() {
        let dir = temp_note("dir");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("note.txt");
        write_note(&path, "x").unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_builds_never_use_the_production_path() {
        let p = note_path().unwrap();
        assert!(p.starts_with(std::env::temp_dir()));
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("wingman-test-"));
    }
}
