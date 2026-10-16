//! Share one action as a single `.toml` file (#117): export, and a guarded
//! import.
//!
//! An imported file is untrusted input. It goes through the same checks as a
//! hand-written `actions.toml` entry ([`super::validate_action`]: the executor
//! must exist, the proposal must be real, `replace_text` needs a selection),
//! plus the rules that only matter for a stranger's file:
//!
//! - Decision: a size cap is checked before parsing, and every text field has
//!   a length cap, so a hostile file cannot hang or flood the app.
//! - Decision: the file holds exactly one `[[actions]]` entry and nothing else
//!   (no `disabled_groups`, no `replace`, no unknown key).
//! - Decision: an imported action never carries a hotkey and is always
//!   enabled. A shared file must not grab a key chord on someone's machine.
//! - Decision: every executor except `none` (`replace_text`, `fill_form`,
//!   `calendar_add`, and also `clipboard` and `image_clipboard`, which the app
//!   would otherwise auto-run as read-only) always confirms. An imported
//!   `confirm = false` is turned on and the preview says so.
//! - Decision: nothing runs on import, and nothing is overwritten. An id that
//!   is already used (a built-in or one in the user's file) gets the next free
//!   `-2`, `-3`, ... suffix. The user sees the final id in the preview.
//! - Decision: the new entry is appended to the user's `actions.toml` as text,
//!   re-serialised from the checked action (never the foreign text), so the
//!   user's hand edits and comments stay as they were. The combined file is
//!   parsed again before anything is written; if that fails, nothing is.
//!
//! No Win32 here: the dialogs live in `ui::file_dialog` and `app.rs`.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{Action, ActionOverride, Origin, Resolved};

/// Largest file the importer will read, in bytes. A real action is well under
/// 4 KiB; this leaves room for a long prompt and nothing more.
pub const MAX_FILE_BYTES: u64 = 16 * 1024;
const MAX_ID_CHARS: usize = 64;
const MAX_NAME_CHARS: usize = 80;
const MAX_GROUP_CHARS: usize = 40;
const MAX_PROMPT_CHARS: usize = 8000;
/// Decision: a prompt up to this long fits on the confirmation card in full
/// (the card is narrow and shows at most ten wrapped lines per field), so the
/// user reads all of it before saying yes. A longer one (up to
/// [`MAX_PROMPT_CHARS`]) is still imported, but turned off, and the user is
/// told to read it in actions.toml and turn it on. The count is conservative.
pub const MAX_SHOWN_PROMPT_CHARS: usize = 300;
const TURNED_OFF_NOTE: &str = "Added turned off. Read the prompt in actions.toml, then turn it on.";

/// Whether an action with this prompt would import turned off.
pub fn imports_turned_off(action: &Action) -> bool {
    action.prompt.chars().count() > MAX_SHOWN_PROMPT_CHARS
}
const MAX_INPUTS: usize = 12;
/// The highest suffix tried when an id is taken.
const MAX_RENAME_SUFFIX: u32 = 99;

/// The root of a shared action file: one `[[actions]]` entry and no other key.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareFile {
    actions: Vec<ActionOverride>,
}

#[derive(Serialize)]
struct ShareOut<'a> {
    actions: [&'a Action; 1],
}

/// A checked action from a shared file, with plain-words notes about what
/// was changed on the way in.
#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    pub action: Action,
    pub notes: Vec<String>,
}

/// Where the action will land and under which id.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub action: Action,
    /// The id the file carried, when it had to be changed.
    pub renamed_from: Option<String>,
    pub notes: Vec<String>,
    /// The file as it was when this was prepared; `commit` refuses if the
    /// file is different by then.
    base: String,
    new_contents: String,
}

/// The action as a shareable file. The hotkey is left out (see the module
/// doc), everything else is written as it is.
pub fn export_toml(action: &Action) -> Result<String> {
    let mut shared = action.clone();
    shared.hotkey = None;
    let body = toml::to_string(&ShareOut { actions: [&shared] })
        .context("could not write the action as TOML")?;
    Ok(format!(
        "# A Wingman action. To add it: tray menu > Help and diagnostics >\n\
         # Import action from file. You are shown what it does before it is added.\n\
         {body}"
    ))
}

/// The actions the user wrote themselves, as (id, name), for the export menu.
/// Built-ins ship with the app, so there is nothing to share.
pub fn export_candidates(actions: &[Resolved]) -> Vec<(String, String)> {
    actions
        .iter()
        .filter(|r| r.origin == Origin::User)
        .map(|r| (r.action.id.clone(), r.action.name.clone()))
        .collect()
}

/// Reads a shared file, refusing one bigger than [`MAX_FILE_BYTES`] without
/// reading past the cap.
pub fn read_import_file(path: &Path) -> Result<String> {
    let file =
        fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("could not read {}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        anyhow::bail!(
            "that file is bigger than {} KiB, which is too big for an action",
            MAX_FILE_BYTES / 1024
        );
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("that file is not UTF-8 text"))
}

/// Unicode "format" characters (category Cf) that change how text is drawn
/// without showing: the right-to-left override U+202E, zero-width marks and
/// the like. `char` has no category lookup, so the ranges are listed.
fn is_format_char(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F
        | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x1D173..=0x1D17A
        | 0xE0001 | 0xE0020..=0xE007F)
}

fn has_control(s: &str, allow_newlines: bool) -> bool {
    s.chars().any(|c| {
        is_format_char(c)
            || (c.is_control() && !(allow_newlines && matches!(c, '\n' | '\r' | '\t')))
    })
}

fn id_is_plain(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_ID_CHARS
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Parses and checks one shared file. Never panics: every failure is an `Err`
/// with a sentence the user can read.
pub fn parse_import(text: &str) -> Result<Import> {
    if text.len() as u64 > MAX_FILE_BYTES {
        anyhow::bail!(
            "that file is bigger than {} KiB, which is too big for an action",
            MAX_FILE_BYTES / 1024
        );
    }
    let file: ShareFile = toml::from_str(text)
        .map_err(|e| anyhow::anyhow!("that file is not a valid action file: {}", e.message()))?;
    let mut entries = file.actions;
    if entries.len() != 1 {
        anyhow::bail!(
            "an action file holds exactly one action, this one holds {}",
            entries.len()
        );
    }
    let entry = entries.remove(0);
    if entry.replace.is_some() {
        anyhow::bail!("an action file cannot use \"replace\"");
    }
    let mut action = entry.into_action()?;
    let mut notes = Vec::new();

    if !id_is_plain(&action.id) {
        anyhow::bail!("the id must be 1 to {MAX_ID_CHARS} characters of a-z, 0-9, \"-\" and \"_\"");
    }
    action.name = action.name.trim().to_string();
    if action.name.is_empty()
        || action.name.chars().count() > MAX_NAME_CHARS
        || has_control(&action.name, false)
    {
        anyhow::bail!("the name must be 1 to {MAX_NAME_CHARS} characters on one line");
    }
    if let Some(group) = &action.group {
        if group.trim().is_empty()
            || group.chars().count() > MAX_GROUP_CHARS
            || has_control(group, false)
        {
            anyhow::bail!("the group must be 1 to {MAX_GROUP_CHARS} characters on one line");
        }
    }
    if action.prompt.chars().count() > MAX_PROMPT_CHARS || has_control(&action.prompt, true) {
        anyhow::bail!(
            "the prompt is longer than {MAX_PROMPT_CHARS} characters or has odd characters"
        );
    }
    if action.inputs.len() > MAX_INPUTS {
        anyhow::bail!("the action lists more than {MAX_INPUTS} inputs");
    }

    // Only "none" does nothing. Every other executor (the clipboard ones
    // included, which the app treats as read-only and would auto-run) must
    // ask first. This runs before validation, which refuses `confirm = false`
    // on a writing executor (#732): an import turns it on instead.
    if action.executor != "none" && !action.confirm {
        action.confirm = true;
        notes.push(
            "It acts on your computer (it changes something or uses the clipboard), so it always asks first. Confirm was turned on."
                .into(),
        );
    }

    super::validate_action(&action)?;

    if action.hotkey.take().is_some() {
        notes.push("Its hotkey was left out.".to_string());
    }
    action.enabled = !imports_turned_off(&action);
    if !action.enabled {
        notes.push(TURNED_OFF_NOTE.to_string());
    }
    Ok(Import { action, notes })
}

/// The first id that is free: `id` itself, else `id-2`, `id-3`, ...
pub fn unique_id(id: &str, taken: &[String]) -> Option<String> {
    if !taken.iter().any(|t| t == id) {
        return Some(id.to_string());
    }
    // Keep the result inside the id length cap.
    let room = MAX_ID_CHARS.saturating_sub(3);
    let stem: String = id.chars().take(room).collect();
    (2..=MAX_RENAME_SUFFIX)
        .map(|n| format!("{stem}-{n}"))
        .find(|candidate| !taken.iter().any(|t| t == candidate))
}

/// Works out the final id and the new file contents against the user's file
/// (which may not exist yet). Writes nothing.
pub fn prepare(user_path: &Path, import: Import) -> Result<Prepared> {
    let existing = match fs::read_to_string(user_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(anyhow::Error::new(e).context("could not read your actions.toml")),
    };
    let parsed: super::UserActionsFile = toml::from_str(&existing).map_err(|e| {
        anyhow::anyhow!(
            "your actions.toml does not parse ({}), so nothing was added to it",
            e.message()
        )
    })?;
    let mut taken: Vec<String> = super::builtin_actions().into_iter().map(|a| a.id).collect();
    taken.extend(parsed.actions.iter().map(|a| a.id.clone()));

    let Import { mut action, notes } = import;
    let original = action.id.clone();
    action.id = unique_id(&original, &taken).ok_or_else(|| {
        anyhow::anyhow!("the id \"{original}\" is already used and no free variant was found")
    })?;
    let renamed_from = (action.id != original).then_some(original);

    let entry = toml::to_string(&ShareOut { actions: [&action] })
        .context("could not write the action as TOML")?;
    let existing_text = existing.clone();
    let mut new_contents = existing;
    if !new_contents.is_empty() && !new_contents.ends_with('\n') {
        new_contents.push('\n');
    }
    if !new_contents.is_empty() {
        new_contents.push('\n');
    }
    new_contents.push_str(&entry);

    // The combined file must still load, and must hold the new id exactly once.
    let check: super::UserActionsFile = toml::from_str(&new_contents)
        .map_err(|_| anyhow::anyhow!("the action could not be added to your actions.toml as it is written, so nothing was changed"))?;
    if check.actions.iter().filter(|a| a.id == action.id).count() != 1 {
        anyhow::bail!("the action could not be added to your actions.toml, so nothing was changed");
    }
    Ok(Prepared {
        action,
        renamed_from,
        notes,
        base: existing_text,
        new_contents,
    })
}

impl Prepared {
    /// Writes the prepared file: a temp file next to it, then a rename, so a
    /// crash leaves the old file whole. Refuses if `actions.toml` is not the
    /// file this was prepared against (an edit made while the user was
    /// deciding must not be lost); nothing is written then.
    pub fn commit(&self, user_path: &Path) -> Result<()> {
        let now = match fs::read_to_string(user_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(anyhow::Error::new(e).context("could not read your actions.toml")),
        };
        if now != self.base {
            anyhow::bail!(
                "your actions.toml changed while you were deciding, so nothing was added. Import the file again"
            );
        }
        if let Some(dir) = user_path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        let tmp = user_path.with_extension("toml.new");
        fs::write(&tmp, &self.new_contents)
            .with_context(|| format!("could not write {}", tmp.display()))?;
        fs::rename(&tmp, user_path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            anyhow::Error::new(e).context("could not replace your actions.toml")
        })
    }

    /// The preview card's fields, in the order the card shows them. The notes
    /// (what was changed, and that nothing runs yet) come first so they are
    /// never pushed off the card; every line break is folded to a space so a
    /// value cannot fake extra lines.
    pub fn preview_value(&self) -> serde_json::Value {
        let a = &self.action;
        let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut notes = vec!["It will not run until you choose it.".to_string()];
        if let Some(old) = &self.renamed_from {
            notes.push(format!(
                "The id \"{old}\" is already used, so it is added as \"{}\".",
                a.id
            ));
        }
        notes.extend(self.notes.iter().cloned());
        let inputs = a
            .inputs
            .iter()
            .map(|i| format!("{i:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", ");
        let ask = if a.confirm { "asks first" } else { "no ask" };
        serde_json::json!({
            "notes": notes.join(" "),
            "name": one_line(&a.name),
            "id": a.id,
            "executor": format!("{} ({ask})", a.executor),
            "looks_at": inputs,
            "prompt": one_line(&a.prompt),
        })
    }
}

/// The schema for [`Prepared::preview_value`]: every row is read-only text, so
/// nothing on the card can be edited into the file.
pub fn preview_schema() -> serde_json::Value {
    let text = |label: &str, multiline: bool| serde_json::json!({"type": "string", "label": label, "multiline": multiline});
    serde_json::json!({
        "type": "object",
        "properties": {
            "notes": text("Notes", true),
            "name": text("Name", false),
            "id": text("Id", false),
            "executor": text("Executor", false),
            "looks_at": text("Looks at", false),
            "prompt": text("Prompt", true),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let unique = format!(
            "wingman-share-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("actions.toml")
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }

    const GOOD: &str = r#"
[[actions]]
id = "tidy-notes"
name = "Tidy notes"
group = "Writing"
inputs = ["selection"]
proposal = "text_answer"
executor = "replace_text"
confirm = true
prompt = "Tidy these notes."
"#;

    fn with_line(extra: &str) -> String {
        format!("{GOOD}{extra}\n")
    }

    #[test]
    fn a_good_file_imports() {
        let i = parse_import(GOOD).unwrap();
        assert_eq!(i.action.id, "tidy-notes");
        assert!(i.notes.is_empty());
    }

    #[test]
    fn export_then_import_round_trips() {
        let mut a = parse_import(GOOD).unwrap().action;
        a.hotkey = Some(crate::actions::ActionHotkey {
            vk: 0x41,
            ctrl: true,
            ..Default::default()
        });
        let text = export_toml(&a).unwrap();
        assert!(!text.contains("hotkey"), "the hotkey is not shared: {text}");
        let back = parse_import(&text).unwrap().action;
        a.hotkey = None;
        assert_eq!(back, a);
    }

    #[test]
    fn a_multi_line_prompt_round_trips() {
        let mut a = parse_import(GOOD).unwrap().action;
        a.prompt = "Line one.\nLine \"two\".\n\tTabbed.".to_string();
        let back = parse_import(&export_toml(&a).unwrap()).unwrap().action;
        assert_eq!(back.prompt, a.prompt);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        for bad in [
            "",
            "not toml [[[",
            "actions = 5",
            "[[actions]]\nid = 3",
            "\u{0}\u{0}",
        ] {
            assert!(parse_import(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn more_than_one_action_or_none_is_refused() {
        assert!(parse_import(&format!("{GOOD}{GOOD}")).is_err());
        assert!(parse_import("actions = []").is_err());
    }

    #[test]
    fn extra_keys_replace_and_disabled_groups_are_refused() {
        assert!(parse_import(&with_line("replace = true")).is_err());
        assert!(parse_import(&with_line("surprise = 1")).is_err());
        assert!(parse_import(&format!("disabled_groups = [\"Study\"]\n{GOOD}")).is_err());
    }

    #[test]
    fn a_missing_field_is_refused() {
        let text = GOOD.replace("prompt = \"Tidy these notes.\"", "");
        assert!(parse_import(&text).is_err());
    }

    #[test]
    fn an_unknown_executor_or_proposal_is_refused() {
        assert!(parse_import(&GOOD.replace("replace_text", "rm_rf")).is_err());
        assert!(parse_import(&GOOD.replace("text_answer", "no_such_schema")).is_err());
    }

    #[test]
    fn replace_text_without_a_selection_is_refused() {
        assert!(parse_import(&GOOD.replace("selection", "screen")).is_err());
    }

    #[test]
    fn a_write_executor_cannot_turn_confirm_off() {
        let i = parse_import(&GOOD.replace("confirm = true", "confirm = false")).unwrap();
        assert!(i.action.confirm);
        assert!(i.notes.iter().any(|n| n.contains("Confirm was turned on")));
    }

    #[test]
    fn clipboard_executors_cannot_turn_confirm_off() {
        for exec in ["clipboard", "image_clipboard"] {
            let text = GOOD
                .replace("replace_text", exec)
                .replace("selection", "screen")
                .replace("confirm = true", "confirm = false");
            let i = parse_import(&text).unwrap_or_else(|e| panic!("{exec}: {e:#}"));
            assert!(i.action.confirm, "{exec}");
            assert!(i.notes.iter().any(|n| n.contains("Confirm was turned on")));
        }
    }

    #[test]
    fn the_none_executor_may_keep_confirm_off() {
        let text = GOOD
            .replace("replace_text", "none")
            .replace("confirm = true", "confirm = false");
        let i = parse_import(&text).unwrap();
        assert!(!i.action.confirm);
    }

    #[test]
    fn the_hotkey_is_dropped_and_disabled_is_turned_on() {
        let text = with_line("enabled = false\nhotkey = { vk = 65, ctrl = true }");
        let i = parse_import(&text).unwrap();
        assert!(i.action.hotkey.is_none());
        assert!(i.action.enabled);
        assert!(i.notes.iter().any(|n| n.contains("hotkey")));
    }

    #[test]
    fn size_and_length_caps_hold() {
        let long = "x".repeat(MAX_PROMPT_CHARS + 1);
        assert!(parse_import(&GOOD.replace("Tidy these notes.", &long)).is_err());
        let huge = "# x\n".repeat(MAX_FILE_BYTES as usize);
        assert!(parse_import(&format!("{huge}{GOOD}")).is_err());
        let long_name = "n".repeat(MAX_NAME_CHARS + 1);
        assert!(parse_import(&GOOD.replace("Tidy notes", &long_name)).is_err());
    }

    #[test]
    fn odd_ids_names_and_control_characters_are_refused() {
        assert!(parse_import(&GOOD.replace("tidy-notes", "Tidy Notes")).is_err());
        assert!(parse_import(&GOOD.replace("tidy-notes", "../evil")).is_err());
        assert!(parse_import(&GOOD.replace("Tidy notes", "Tidy\\nnotes")).is_err());
        assert!(parse_import(&GOOD.replace("Tidy these notes.", "a\\u0007b")).is_err());
    }

    #[test]
    fn a_big_file_is_refused_before_it_is_read_whole() {
        let path = scratch("big");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'a'; (MAX_FILE_BYTES + 10) as usize]).unwrap();
        assert!(read_import_file(&path).is_err());
        fs::write(&path, GOOD).unwrap();
        assert!(read_import_file(&path).is_ok());
        fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(read_import_file(&path).is_err());
        cleanup(&path);
    }

    #[test]
    fn a_taken_id_gets_a_suffix_and_nothing_is_overwritten() {
        let taken = vec!["a".to_string(), "a-2".to_string()];
        assert_eq!(unique_id("a", &taken).as_deref(), Some("a-3"));
        assert_eq!(unique_id("b", &taken).as_deref(), Some("b"));
        let all: Vec<String> = std::iter::once("a".to_string())
            .chain((2..=MAX_RENAME_SUFFIX).map(|n| format!("a-{n}")))
            .collect();
        assert_eq!(unique_id("a", &all), None);
    }

    #[test]
    fn a_builtin_id_is_renamed_not_replaced() {
        let path = scratch("builtin");
        let text = GOOD.replace("tidy-notes", crate::actions::DEFAULT_ACTION_ID);
        let p = prepare(&path, parse_import(&text).unwrap()).unwrap();
        assert_eq!(
            p.action.id,
            format!("{}-2", crate::actions::DEFAULT_ACTION_ID)
        );
        assert_eq!(
            p.renamed_from.as_deref(),
            Some(crate::actions::DEFAULT_ACTION_ID)
        );
        assert!(p.preview_value()["notes"]
            .as_str()
            .unwrap()
            .contains("already used"));
        cleanup(&path);
    }

    #[test]
    fn adding_appends_and_keeps_the_users_file_byte_for_byte() {
        let path = scratch("append");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mine = "# my notes\ndisabled_groups = [\"Study\"]\n\n[[actions]]\nid = \"mine\"\nname = \"Mine\"\ninputs = [\"screen\"]\nproposal = \"text_answer\"\nexecutor = \"none\"\nconfirm = false\nprompt = \"Hi\" # keep\n";
        fs::write(&path, mine).unwrap();
        let p = prepare(&path, parse_import(GOOD).unwrap()).unwrap();
        p.commit(&path).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(mine), "{after}");
        let report = crate::actions::load_actions_report_from(&path).unwrap();
        assert!(report.actions.iter().any(|r| r.action.id == "tidy-notes"));
        assert!(!path.with_extension("toml.new").exists());
        cleanup(&path);
    }

    #[test]
    fn adding_twice_gives_two_actions_with_different_ids() {
        let path = scratch("twice");
        for _ in 0..2 {
            prepare(&path, parse_import(GOOD).unwrap())
                .unwrap()
                .commit(&path)
                .unwrap();
        }
        let report = crate::actions::load_actions_report_from(&path).unwrap();
        let ids: Vec<_> = report
            .actions
            .iter()
            .map(|r| r.action.id.as_str())
            .collect();
        assert!(
            ids.contains(&"tidy-notes") && ids.contains(&"tidy-notes-2"),
            "{ids:?}"
        );
        cleanup(&path);
    }

    #[test]
    fn a_broken_user_file_is_left_alone() {
        let path = scratch("broken");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "this is [[ not toml").unwrap();
        assert!(prepare(&path, parse_import(GOOD).unwrap()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "this is [[ not toml");
        cleanup(&path);
    }

    #[test]
    fn an_inline_actions_array_that_cannot_be_appended_to_is_left_alone() {
        let path = scratch("inline");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mine = "actions = []\n";
        fs::write(&path, mine).unwrap();
        assert!(prepare(&path, parse_import(GOOD).unwrap()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), mine);
        cleanup(&path);
    }

    #[test]
    fn the_preview_names_the_action_executor_and_prompt() {
        let path = scratch("preview");
        let p = prepare(&path, parse_import(GOOD).unwrap()).unwrap();
        let v = p.preview_value();
        assert_eq!(v["name"], "Tidy notes");
        assert_eq!(v["id"], "tidy-notes");
        assert!(v["executor"].as_str().unwrap().contains("replace_text"));
        assert_eq!(v["prompt"], "Tidy these notes.");
        // Every key the card shows has a row in the schema, notes first.
        let schema = preview_schema();
        let keys: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "notes");
        for k in v.as_object().unwrap().keys() {
            assert!(schema["properties"].get(k).is_some(), "{k}");
        }
        cleanup(&path);
    }

    #[test]
    fn the_preview_folds_newlines_and_leads_with_the_notes() {
        let path = scratch("fold");
        let text = GOOD
            .replace("confirm = true", "confirm = false")
            .replace("Tidy these notes.", "one\\n\\n\\n\\ntwo");
        let p = prepare(&path, parse_import(&text).unwrap()).unwrap();
        let v = p.preview_value();
        assert_eq!(v["prompt"], "one two");
        let notes = v["notes"].as_str().unwrap();
        assert!(notes.starts_with("It will not run until you choose it."));
        assert!(notes.contains("Confirm was turned on"));
        cleanup(&path);
    }

    #[test]
    fn a_prompt_too_long_to_show_whole_imports_turned_off() {
        let fits = "x".repeat(MAX_SHOWN_PROMPT_CHARS);
        let i = parse_import(&GOOD.replace("Tidy these notes.", &fits)).unwrap();
        assert!(i.action.enabled && !imports_turned_off(&i.action));
        let long = "x".repeat(MAX_SHOWN_PROMPT_CHARS + 1);
        let i = parse_import(&GOOD.replace("Tidy these notes.", &long)).unwrap();
        assert!(!i.action.enabled && imports_turned_off(&i.action));
        assert!(i.notes.iter().any(|n| n.contains("Added turned off")));
        let path = scratch("off");
        let p = prepare(&path, i).unwrap();
        assert!(p.preview_value()["notes"]
            .as_str()
            .unwrap()
            .contains("Added turned off. Read the prompt in actions.toml, then turn it on."));
        p.commit(&path).unwrap();
        let report = crate::actions::load_actions_report_from(&path).unwrap();
        assert!(!report.actions.iter().any(|r| r.action.id == "tidy-notes"));
        let too_long = "x".repeat(MAX_PROMPT_CHARS + 1);
        assert!(parse_import(&GOOD.replace("Tidy these notes.", &too_long)).is_err());
        cleanup(&path);
    }

    #[test]
    fn invisible_format_characters_are_refused() {
        for bad in ["\\u202e", "\\u200b", "\\ufeff", "\\u2066"] {
            assert!(
                parse_import(&GOOD.replace("Tidy notes", &format!("Tidy{bad}notes"))).is_err(),
                "name {bad}"
            );
            assert!(
                parse_import(&GOOD.replace("Tidy these notes.", &format!("a{bad}b"))).is_err(),
                "prompt {bad}"
            );
            assert!(
                parse_import(&GOOD.replace("\"Writing\"", &format!("\"W{bad}\""))).is_err(),
                "group {bad}"
            );
        }
        assert!(parse_import(&GOOD.replace("tidy-notes", "tidy\\u202e")).is_err());
    }

    #[test]
    fn an_edit_made_while_the_user_decides_is_not_lost() {
        let path = scratch("stale");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "# before\n").unwrap();
        let p = prepare(&path, parse_import(GOOD).unwrap()).unwrap();
        fs::write(&path, "# before\n# edited meanwhile\n").unwrap();
        assert!(p.commit(&path).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# before\n# edited meanwhile\n"
        );
        assert!(!path.with_extension("toml.new").exists());
        cleanup(&path);
    }

    #[test]
    fn a_file_that_appears_while_the_user_decides_is_not_overwritten() {
        let path = scratch("appears");
        let p = prepare(&path, parse_import(GOOD).unwrap()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "# new file\n").unwrap();
        assert!(p.commit(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "# new file\n");
        cleanup(&path);
    }

    #[test]
    fn only_user_written_actions_can_be_exported() {
        let path = scratch("cands");
        prepare(&path, parse_import(GOOD).unwrap())
            .unwrap()
            .commit(&path)
            .unwrap();
        let report = crate::actions::load_actions_report_from(&path).unwrap();
        let c = export_candidates(&report.actions);
        assert_eq!(
            c,
            vec![("tidy-notes".to_string(), "Tidy notes".to_string())]
        );
        cleanup(&path);
    }
}
