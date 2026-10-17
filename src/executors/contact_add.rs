//! The `"contact_add"` executor (#71): consumes a confirmed `contact_card`
//! proposal (name, email, phone, org, title, address), writes it as a vCard
//! `.vcf` file in the temp folder, and opens it so Windows offers to add it
//! to the address book.
//!
//! **Version: vCard 3.0 (RFC 2426).** The machine this was built on has
//! `.vcf` registered to classic Outlook (`Outlook.File.vcf.15`), and
//! Outlook and the Windows People/Contacts apps read 2.1 and 3.0 reliably
//! while 4.0 support is partial. THEORY (unverified): that Outlook also
//! reads the UTF-8 in a 3.0 file without a `CHARSET` parameter, so a name
//! like "Zoë" survives; the check is a manual one (see the PR).
//!
//! Escaping, per RFC 2426 section 4: backslash, comma, semicolon and a line
//! break (as the two characters `\n`) in every value, applied to each
//! component of a structured value (`N`, `ORG`, `ADR`) on its own, so a
//! comma or semicolon in a company name never splits a field. Lines are
//! folded at 75 octets without cutting a multi-byte character. Control
//! characters are dropped, so no model output can start a new property.
//!
//! Nothing here panics on model output: every field is read as an optional
//! string, trimmed, and cut on a character boundary.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::connectors::{
    cleanup_stale_ics_files, escape_text, fold_line, generate_uid, is_hidden_format,
    sanitize_filename, temp_dir_for, Opener, ShellOpener,
};
use crate::ui::confirm::Confirmed;

use super::{Effect, Executor, Undo};

/// Longest value kept for one field, in characters. A signature field is
/// never this long; the cap only bounds a runaway model answer.
const MAX_FIELD_CHARS: usize = 400;

/// The six fields of a `contact_card` proposal, cleaned: trimmed, control
/// characters removed, line breaks normalised to `\n`, cut to
/// [`MAX_FIELD_CHARS`]. An absent or non-string field is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contact {
    pub name: String,
    pub email: String,
    pub phone: String,
    pub org: String,
    pub title: String,
    pub address: String,
}

/// A one-line field: control characters dropped, every run of whitespace
/// (a line break included) one space, trimmed, cut to [`MAX_FIELD_CHARS`].
fn clean_line(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .filter(|c| {
            (!c.is_control() || c.is_whitespace()) && *c != '\u{feff}' && !is_hidden_format(*c)
        })
        .collect();
    cut(kept.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// A multi-line field (the address): line breaks of every kind become `\n`,
/// other control characters are dropped, each line is trimmed, empty lines
/// are removed.
fn clean_block(raw: &str) -> String {
    let normalised: String = raw
        .chars()
        .filter_map(|c| match c {
            '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}' => Some('\n'),
            '\t' => Some(' '),
            '\u{feff}' => None,
            c if c.is_control() || is_hidden_format(c) => None,
            c => Some(c),
        })
        .collect();
    let lines: Vec<&str> = normalised
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    cut(lines.join("\n"))
}

/// At most [`MAX_FIELD_CHARS`] characters, cut on a character boundary.
fn cut(s: String) -> String {
    match s.char_indices().nth(MAX_FIELD_CHARS) {
        Some((i, _)) => s[..i].trim_end().to_string(),
        None => s,
    }
}

fn field(value: &Value, key: &str, clean: fn(&str) -> String) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(clean)
        .unwrap_or_default()
}

/// Reads a `contact_card` proposal. Never fails on a missing or oddly typed
/// field (that field is empty). Fails when there is nothing worth saving: no
/// name, no email and no phone (AGENTS.md rule 7: an error card, not an empty
/// file handed to the address book).
pub fn parse_contact(value: &Value) -> Result<Contact> {
    let contact = Contact {
        name: field(value, "name", clean_line),
        email: field(value, "email", clean_line),
        phone: field(value, "phone", clean_line),
        org: field(value, "org", clean_line),
        title: field(value, "title", clean_line),
        address: field(value, "address", clean_block),
    };
    if contact.name.is_empty() && contact.email.is_empty() && contact.phone.is_empty() {
        return Err(anyhow!(
            "contact_add: the card has no name, email or phone, so there is nothing to save"
        ));
    }
    Ok(contact)
}

/// Words that end a company name, so "Smith and Sons Ltd" is not a person.
const COMPANY_SUFFIXES: [&str; 14] = [
    "inc", "inc.", "llc", "ltd", "ltd.", "corp", "corp.", "co", "co.", "gmbh", "plc", "llp", "ag",
    "company",
];

/// `(family, given, suffix, display)` for a name.
///
/// - A business (the name is the org, or ends in a company suffix such as
///   "Corp" or "Ltd") goes whole into the family slot, never split in two.
/// - "Smith, Jane" (one word before the comma) is family-first: family Smith,
///   given Jane, displayed "Jane Smith".
/// - Any other comma is a trailing credential: "Jane Smith, MD" is family
///   Smith, given Jane, suffix MD, and the display name is kept exactly.
/// - Otherwise the last word is the family name and the rest the given names.
///   A single word is the family name.
fn split_name(name: &str, org: &str) -> (String, String, String, String) {
    let whole = || {
        (
            name.to_string(),
            String::new(),
            String::new(),
            name.to_string(),
        )
    };
    let last_word = name.rsplit(' ').next().unwrap_or("").to_lowercase();
    if name.eq_ignore_ascii_case(org) || COMPANY_SUFFIXES.contains(&last_word.as_str()) {
        return whole();
    }
    let (person, suffix) = match name.split_once(',') {
        Some((before, after)) => {
            let (before, after) = (before.trim(), after.trim());
            if !before.is_empty() && !after.is_empty() && !before.contains(' ') {
                return (
                    before.to_string(),
                    after.to_string(),
                    String::new(),
                    format!("{after} {before}"),
                );
            }
            (before, after)
        }
        None => (name, ""),
    };
    match person.rsplit_once(' ') {
        Some((given, family)) => (
            family.to_string(),
            given.to_string(),
            suffix.to_string(),
            name.to_string(),
        ),
        None => whole(),
    }
}

/// The vCard 3.0 text for `contact`, CRLF line endings, folded. `uid` is the
/// card's `UID`.
pub fn render_vcard(contact: &Contact, uid: &str) -> String {
    let (family, given, suffix, display) = split_name(&contact.name, &contact.org);
    // FN is required: with no name, fall back to the email, then the phone.
    let display = [display, contact.email.clone(), contact.phone.clone()]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_default();

    let mut props = vec![
        "BEGIN:VCARD".to_string(),
        "VERSION:3.0".to_string(),
        format!("UID:{}", escape_text(uid)),
        format!(
            "N:{};{};;;{}",
            escape_text(&family),
            escape_text(&given),
            escape_text(&suffix)
        ),
        format!("FN:{}", escape_text(&display)),
    ];
    if !contact.org.is_empty() {
        props.push(format!("ORG:{}", escape_text(&contact.org)));
    }
    if !contact.title.is_empty() {
        props.push(format!("TITLE:{}", escape_text(&contact.title)));
    }
    if !contact.phone.is_empty() {
        props.push(format!("TEL;TYPE=VOICE:{}", escape_text(&contact.phone)));
    }
    if !contact.email.is_empty() {
        props.push(format!(
            "EMAIL;TYPE=INTERNET:{}",
            escape_text(&contact.email)
        ));
    }
    if !contact.address.is_empty() {
        // The street component carries the whole address: one block of text
        // cannot be split into street, city and code reliably.
        props.push(format!(
            "ADR;TYPE=WORK:;;{};;;;",
            escape_text(&contact.address)
        ));
    }
    props.push("END:VCARD".to_string());

    let mut out = String::new();
    for line in props {
        out.push_str(&fold_line(&line));
        out.push_str("\r\n");
    }
    out
}

/// Writes the card to a temp `.vcf` and opens it. `O` is injectable so tests
/// never launch a real handler, and the folder is injectable so tests never
/// touch the real `%TEMP%\Wingman` (AGENTS.md rule 9).
pub struct ContactAddExecutor<O: Opener = ShellOpener> {
    temp_dir: PathBuf,
    opener: O,
}

impl ContactAddExecutor<ShellOpener> {
    /// Production constructor: `%TEMP%\Wingman` (or the portable root's
    /// `tmp`), the real shell opener.
    pub fn new() -> Self {
        Self {
            temp_dir: temp_dir_for(crate::portable::root().as_deref(), std::env::temp_dir()),
            opener: ShellOpener,
        }
    }
}

impl Default for ContactAddExecutor<ShellOpener> {
    fn default() -> Self {
        Self::new()
    }
}

impl<O: Opener> ContactAddExecutor<O> {
    #[cfg(test)]
    fn with_temp_dir_and_opener(temp_dir: PathBuf, opener: O) -> Self {
        Self { temp_dir, opener }
    }
}

impl<O: Opener> Executor for ContactAddExecutor<O> {
    fn name(&self) -> &'static str {
        "contact_add"
    }

    fn effect(&self) -> Effect {
        Effect::Writes
    }

    fn execute(&self, confirmed: Confirmed<Value>) -> Result<Undo> {
        // The one place this executor reads the raw `Value`.
        let contact = parse_contact(&confirmed.into_value())?;
        let uid = generate_uid();
        let content = render_vcard(&contact, &uid);

        std::fs::create_dir_all(&self.temp_dir)
            .with_context(|| format!("could not create {}", self.temp_dir.display()))?;
        // Best-effort: old calendar and contact files in this folder go.
        cleanup_stale_ics_files(&self.temp_dir);
        let path = self
            .temp_dir
            .join(format!("contact-{}.vcf", sanitize_filename(&uid)));
        std::fs::write(&path, content.as_bytes())
            .with_context(|| format!("could not write {}", path.display()))?;

        // A failed open still leaves the written file: the summary names it.
        let opened_note = if self.opener.open(&path).is_ok() {
            String::new()
        } else {
            format!(
                ", but it could not be opened automatically; open {} yourself",
                path.display()
            )
        };
        let who = [&contact.name, &contact.email, &contact.phone]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_default();
        // Same honesty as the calendar: undo can only remove the file this
        // wrote, not a contact the address book made from it.
        let summary = format!(
            "Saved \"{who}\" as a contact card at {}{opened_note}. Undo removes this file; removing the contact from your address book is a manual step.",
            path.display()
        );
        Ok(Undo::recording(summary, move || {
            std::fs::remove_file(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
            Ok(())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use crate::ui::confirm::Proposal;

    #[derive(Default)]
    struct RecordingOpener {
        opened: Mutex<Vec<PathBuf>>,
        fail: bool,
    }

    impl Opener for RecordingOpener {
        fn open(&self, path: &Path) -> Result<()> {
            self.opened.lock().unwrap().push(path.to_path_buf());
            if self.fail {
                anyhow::bail!("no handler");
            }
            Ok(())
        }
    }

    fn test_temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wingman-contact-add-test-{}-{n}-{label}",
            std::process::id()
        ))
    }

    fn confirmed(value: Value) -> Confirmed<Value> {
        crate::ui::confirm::confirm(Proposal::new(value), crate::ui::confirm::user_confirmed())
    }

    fn sample() -> Value {
        serde_json::json!({
            "name": "Jane Q. Smith",
            "email": "jane.smith@example.com",
            "phone": "+1 (555) 010-4477",
            "org": "Acme Corp",
            "title": "Head of Sales",
            "address": "12 Main St, Springfield, IL 62701"
        })
    }

    fn lines(vcard: &str) -> Vec<&str> {
        vcard.split("\r\n").collect()
    }

    // -- parse_contact ---------------------------------------------------

    #[test]
    fn parses_a_full_card() {
        let c = parse_contact(&sample()).unwrap();
        assert_eq!(c.name, "Jane Q. Smith");
        assert_eq!(c.email, "jane.smith@example.com");
        assert_eq!(c.phone, "+1 (555) 010-4477");
        assert_eq!(c.org, "Acme Corp");
        assert_eq!(c.title, "Head of Sales");
        assert_eq!(c.address, "12 Main St, Springfield, IL 62701");
    }

    #[test]
    fn refuses_a_card_with_no_name_email_or_phone() {
        for v in [
            serde_json::json!({}),
            serde_json::json!({"name": "", "email": " ", "phone": "\n", "org": "Acme"}),
            serde_json::json!({"org": "Acme", "title": "CEO", "address": "1 Road"}),
            serde_json::json!("not an object"),
            serde_json::json!(null),
        ] {
            let err = parse_contact(&v).expect_err("nothing to save must be refused");
            let msg = err.to_string();
            assert!(msg.contains("name"), "names what is missing: {msg}");
            assert!(!msg.contains('\u{2014}'), "no em dash: {msg}");
        }
    }

    #[test]
    fn a_name_alone_an_email_alone_or_a_phone_alone_is_enough() {
        for v in [
            serde_json::json!({"name": "Jane"}),
            serde_json::json!({"email": "a@b.co"}),
            serde_json::json!({"phone": "555 0100"}),
        ] {
            assert!(parse_contact(&v).is_ok(), "{v}");
        }
    }

    #[test]
    fn wrong_typed_fields_are_empty_not_a_panic() {
        let v = serde_json::json!({"name": "Jane", "email": 5, "phone": ["x"], "org": null});
        let c = parse_contact(&v).unwrap();
        assert_eq!(c.name, "Jane");
        assert_eq!(c.email, "");
        assert_eq!(c.phone, "");
        assert_eq!(c.org, "");
    }

    #[test]
    fn control_characters_are_dropped_and_line_breaks_normalised() {
        let v = serde_json::json!({
            "name": "Jane\u{0}\u{7} Smith\u{feff}",
            "address": "1 Road\r\nSuite 2\rTown\u{2028}Zip"
        });
        let c = parse_contact(&v).unwrap();
        assert_eq!(c.name, "Jane Smith");
        assert_eq!(c.address, "1 Road\nSuite 2\nTown\nZip");
    }

    #[test]
    fn a_runaway_field_is_cut_on_a_character_boundary() {
        let v = serde_json::json!({"name": "\u{e9}".repeat(MAX_FIELD_CHARS + 50)});
        let c = parse_contact(&v).unwrap();
        assert_eq!(c.name.chars().count(), MAX_FIELD_CHARS);
    }

    // -- render_vcard ----------------------------------------------------

    #[test]
    fn renders_a_full_card_exactly() {
        let c = parse_contact(&sample()).unwrap();
        let out = render_vcard(&c, "uid-1@wingman.local");
        let expected = [
            "BEGIN:VCARD",
            "VERSION:3.0",
            "UID:uid-1@wingman.local",
            "N:Smith;Jane Q.;;;",
            "FN:Jane Q. Smith",
            "ORG:Acme Corp",
            "TITLE:Head of Sales",
            "TEL;TYPE=VOICE:+1 (555) 010-4477",
            "EMAIL;TYPE=INTERNET:jane.smith@example.com",
            "ADR;TYPE=WORK:;;12 Main St\\, Springfield\\, IL 62701;;;;",
            "END:VCARD",
            "",
        ]
        .join("\r\n");
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_fields_write_no_property() {
        let c = Contact {
            name: "Jane Smith".into(),
            ..Contact::default()
        };
        let out = render_vcard(&c, "u");
        for prop in ["ORG", "TITLE", "TEL", "EMAIL", "ADR"] {
            assert!(
                !lines(&out).iter().any(|l| l.starts_with(prop)),
                "{prop} written for an empty field: {out}"
            );
        }
        assert!(out.contains("FN:Jane Smith\r\n"));
    }

    #[test]
    fn commas_semicolons_backslashes_and_newlines_are_escaped_per_component() {
        let c = Contact {
            name: "Smith, Jane".into(),
            org: "Acme, Inc.; Sales\\East".into(),
            title: "VP, Ops".into(),
            address: "1 Road\nSuite 2; Wing, B".into(),
            email: "a@b.co".into(),
            ..Contact::default()
        };
        let out = render_vcard(&c, "u");
        // "Family, Given" is read as family and given.
        assert!(out.contains("\r\nN:Smith;Jane;;;\r\n"), "{out}");
        assert!(out.contains("\r\nFN:Jane Smith\r\n"), "{out}");
        assert!(
            out.contains("\r\nORG:Acme\\, Inc.\\; Sales\\\\East\r\n"),
            "{out}"
        );
        assert!(out.contains("\r\nTITLE:VP\\, Ops\r\n"), "{out}");
        assert!(
            out.contains("\r\nADR;TYPE=WORK:;;1 Road\\nSuite 2\\; Wing\\, B;;;;\r\n"),
            "{out}"
        );
        // No raw line break survives inside a value: every physical line
        // starts a known property, or is a fold, or the BEGIN/END frame.
        for l in lines(&out) {
            assert!(
                l.is_empty()
                    || l.starts_with(' ')
                    || [
                        "BEGIN:", "VERSION:", "UID:", "N:", "FN:", "ORG:", "TITLE:", "TEL;",
                        "EMAIL;", "ADR;", "END:"
                    ]
                    .iter()
                    .any(|p| l.starts_with(p)),
                "unexpected line {l:?} in {out}"
            );
        }
    }

    #[test]
    fn a_value_cannot_inject_a_property() {
        let c = Contact {
            name: "Jane".into(),
            title: "CEO\nEMAIL:evil@x.co\nEND:VCARD".into(),
            ..Contact::default()
        };
        let out = render_vcard(&c, "u");
        assert_eq!(
            lines(&out).iter().filter(|l| **l == "END:VCARD").count(),
            1,
            "{out}"
        );
        assert!(!lines(&out).iter().any(|l| l.starts_with("EMAIL")), "{out}");
    }

    #[test]
    fn non_ascii_is_kept_and_a_long_line_folds_without_cutting_a_character() {
        let c = Contact {
            name: "Zo\u{eb} M\u{fc}ller".into(),
            address: "\u{4e2d}\u{6587}".repeat(60),
            ..Contact::default()
        };
        let out = render_vcard(&c, "u");
        assert!(out.contains("FN:Zo\u{eb} M\u{fc}ller"), "{out}");
        let mut unfolded = String::new();
        for l in lines(&out) {
            assert!(l.len() <= 75, "physical line over 75 octets: {}", l.len());
            match l.strip_prefix(' ') {
                Some(rest) => unfolded.push_str(rest),
                None => {
                    unfolded.push('\n');
                    unfolded.push_str(l);
                }
            }
        }
        assert!(
            unfolded.contains(&format!(
                "ADR;TYPE=WORK:;;{};;;;",
                "\u{4e2d}\u{6587}".repeat(60)
            )),
            "unfolding must give back the whole address"
        );
    }

    fn card_for(name: &str, org: &str) -> String {
        let c = Contact {
            name: name.into(),
            org: org.into(),
            ..Contact::default()
        };
        render_vcard(&c, "u")
    }

    #[test]
    fn a_trailing_credential_stays_in_the_name_and_is_not_swapped_to_the_front() {
        let out = card_for("Jane Smith, MD", "");
        assert!(out.contains("\r\nFN:Jane Smith\\, MD\r\n"), "{out}");
        assert!(out.contains("\r\nN:Smith;Jane;;;MD\r\n"), "{out}");
        let out = card_for("John Doe, PhD", "");
        assert!(out.contains("\r\nFN:John Doe\\, PhD\r\n"), "{out}");
        assert!(out.contains("\r\nN:Doe;John;;;PhD\r\n"), "{out}");
    }

    #[test]
    fn a_business_name_is_not_split_into_a_person() {
        // Same as the org, and a name ending in a company suffix.
        for (name, org) in [
            ("Acme Corp", "Acme Corp"),
            ("acme corp", "ACME CORP"),
            ("Acme Corp", ""),
            ("Smith and Sons Ltd", ""),
        ] {
            let out = card_for(name, org);
            assert!(
                out.contains(&format!("\r\nN:{name};;;;\r\n")),
                "{name}: {out}"
            );
            assert!(out.contains(&format!("\r\nFN:{name}\r\n")), "{name}: {out}");
        }
    }

    #[test]
    fn format_characters_that_reorder_or_hide_text_are_dropped_but_joiners_stay() {
        let v = serde_json::json!({
            "name": "Jane\u{202e}Smith\u{200b}\u{2066}x\u{2069}\u{200e}",
            "title": "a\u{200d}b\u{200c}c",
            "address": "1\u{202a} Road\u{2067}\n2\u{200b}"
        });
        let c = parse_contact(&v).unwrap();
        assert_eq!(c.name, "JaneSmithx");
        assert_eq!(c.title, "a\u{200d}b\u{200c}c");
        assert_eq!(c.address, "1 Road\n2");
    }

    #[test]
    fn a_single_word_name_is_still_a_valid_card() {
        let c = Contact {
            name: "Madonna".into(),
            ..Contact::default()
        };
        let out = render_vcard(&c, "u");
        assert!(out.contains("\r\nN:Madonna;;;;\r\n"), "{out}");
        assert!(out.contains("\r\nFN:Madonna\r\n"), "{out}");
    }

    #[test]
    fn no_name_uses_the_email_then_the_phone_for_the_required_fn() {
        let c = Contact {
            email: "a@b.co".into(),
            phone: "555".into(),
            ..Contact::default()
        };
        assert!(render_vcard(&c, "u").contains("\r\nFN:a@b.co\r\n"));
        let c = Contact {
            phone: "555".into(),
            ..Contact::default()
        };
        assert!(render_vcard(&c, "u").contains("\r\nFN:555\r\n"));
    }

    #[test]
    fn the_card_is_one_vcard_with_crlf_endings_and_a_trailing_break() {
        let out = render_vcard(&parse_contact(&sample()).unwrap(), "u");
        assert!(out.starts_with("BEGIN:VCARD\r\nVERSION:3.0\r\n"));
        assert!(out.ends_with("END:VCARD\r\n"));
        assert!(
            !out.replace("\r\n", "").contains('\n'),
            "bare LF in {out:?}"
        );
    }

    // -- the executor ------------------------------------------------------

    #[test]
    fn contact_add_is_a_writing_executor() {
        let ex = ContactAddExecutor::with_temp_dir_and_opener(
            test_temp_dir("effect"),
            RecordingOpener::default(),
        );
        assert_eq!(ex.effect(), Effect::Writes);
        assert_eq!(ex.name(), "contact_add");
    }

    #[test]
    fn execute_writes_the_vcf_opens_it_and_undo_removes_it() {
        let dir = test_temp_dir("write");
        let ex =
            ContactAddExecutor::with_temp_dir_and_opener(dir.clone(), RecordingOpener::default());
        let undo = ex.execute(confirmed(sample())).unwrap();

        let opened = ex.opener.opened.lock().unwrap().clone();
        assert_eq!(opened.len(), 1, "the file is handed to the shell once");
        let path = &opened[0];
        assert!(path.starts_with(&dir));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("vcf"));
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("FN:Jane Q. Smith"), "{text}");
        assert!(undo.summary.contains("Jane Q. Smith"), "{}", undo.summary);
        assert!(!undo.summary.contains('\u{2014}'));
        assert!(undo.is_restorable());

        undo.undo().unwrap();
        assert!(!path.exists(), "undo removes the file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_open_still_succeeds_and_names_the_file() {
        let dir = test_temp_dir("noopen");
        let ex = ContactAddExecutor::with_temp_dir_and_opener(
            dir.clone(),
            RecordingOpener {
                fail: true,
                ..RecordingOpener::default()
            },
        );
        let undo = ex.execute(confirmed(sample())).unwrap();
        assert!(
            undo.summary.contains("could not be opened"),
            "{}",
            undo.summary
        );
        assert!(undo.summary.contains(".vcf"), "{}", undo.summary);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn execute_refuses_an_empty_card_and_writes_nothing() {
        let dir = test_temp_dir("empty");
        let ex =
            ContactAddExecutor::with_temp_dir_and_opener(dir.clone(), RecordingOpener::default());
        let err = ex
            .execute(confirmed(serde_json::json!({"org": "Acme"})))
            .err()
            .expect("an empty card must be refused");
        assert!(err.to_string().contains("name"), "{err}");
        assert!(ex.opener.opened.lock().unwrap().is_empty());
        assert!(!dir.exists(), "nothing is created for a refused card");
    }
}
