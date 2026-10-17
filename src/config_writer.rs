//! Field-level writes to `config.toml` (#870, slice 0 of the Settings pages
//! spec, `docs/superpowers/specs/2026-10-16-settings-pages-and-instant-apply-design.md`).
//!
//! `Config::save_to` re-serialises the whole typed `Config`, so every save drops
//! the user's comments, unknown keys and key order, and silently reverts a hand
//! edit made since the file was read. This module changes one field instead: it
//! reads the file as it is on disk now, patches only the touched keys with
//! `toml_edit` (everything else stays byte for byte, including CRLF line
//! endings), and writes the result through the same atomic path `save_to` uses
//! (`config::write_file_atomically`).
//!
//! Three pieces:
//! - [`FieldChange`]: one key to set or remove, as a value rather than a
//!   `Config`. [`diff`] turns "the config before" and "the config after" into the
//!   list of keys that really changed, which is how the Settings window will say
//!   what the user touched.
//! - [`write_changes`]: the fresh read, patch and atomic write, with the rules
//!   below.
//! - [`ConfigWriter`]: one thread that blocks on a channel (no polling) and does
//!   the file and `icacls` work, so the UI thread, which hosts the keyboard
//!   hook, never touches the disk.
//!
//! Rules this module keeps:
//! - A file that does not parse (as TOML, or as a `Config`) is never written.
//!   The error names the line and column and quotes no file text (a value could
//!   be an API key).
//! - No API key ever goes into the file here: a change that would write a
//!   non-empty `api_key` is refused, and [`diff`] never produces one. Keys belong
//!   to Credential Manager.
//! - If the file changes between the read and the rename, the read and patch run
//!   again once on top of the new content; a second change fails the write
//!   rather than overwrite it.
//! - A change that leaves the file unchanged writes nothing.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use crate::config::Config;
use crate::mode::Mode;

/// One key to change in `config.toml`.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldChange {
    /// Set `path` (table names, then the key) to `value`.
    Set {
        path: Vec<String>,
        value: toml::Value,
    },
    /// Remove `path`. Missing is fine.
    Remove { path: Vec<String> },
    /// Set `path` to `value` only if the file still holds `expected` there (`None`:
    /// the key is not set). For a whole value (a list, a table) a window changed
    /// from what it opened with: writing it blind would drop a sibling edit the
    /// user made by hand in the meantime, so a changed file refuses it instead.
    SetIfUnchanged {
        path: Vec<String>,
        expected: Option<toml::Value>,
        value: toml::Value,
    },
}

impl FieldChange {
    /// Set `path` to any serializable value. The tests use it now; the Settings
    /// window's commit step does from #871.
    #[allow(dead_code)]
    pub fn set(path: &[&str], value: impl serde::Serialize) -> Result<Self> {
        Ok(FieldChange::Set {
            path: path.iter().map(|s| s.to_string()).collect(),
            value: toml::Value::try_from(value).context("could not turn the value into TOML")?,
        })
    }

    /// `mode = "..."`, the first field to move to the writer (the tray's Mode
    /// submenu).
    pub fn mode(mode: Mode) -> Self {
        FieldChange::Set {
            path: vec!["mode".to_string()],
            value: toml::Value::try_from(mode).expect("a Mode always serializes"),
        }
    }

    /// The key this change is about.
    pub fn path(&self) -> &[String] {
        match self {
            FieldChange::Set { path, .. }
            | FieldChange::Remove { path }
            | FieldChange::SetIfUnchanged { path, .. } => path,
        }
    }

    /// Patches `doc` and nothing else. Setting a key that is not in the file
    /// adds it (and any missing table above it); a table written inline
    /// (`primary = { vk = 134 }`) is patched in place. The old value's trailing
    /// comment stays with the new one.
    pub fn apply(&self, doc: &mut DocumentMut) -> Result<()> {
        if let FieldChange::SetIfUnchanged { path, value, .. } = self {
            // The comparison is made by the caller (`write_changes_with`), which
            // has the text the doc came from.
            return FieldChange::Set {
                path: path.clone(),
                value: value.clone(),
            }
            .apply(doc);
        }
        let path = self.path();
        let Some((key, parents)) = path.split_last() else {
            bail!("a config change needs a key");
        };
        if let FieldChange::Set { value, .. } = self {
            // The key itself, or anywhere inside a whole table or array being
            // set (`providers`, `providers.compat`): the in-memory config holds
            // hydrated keys, and a whole-value change would carry them in.
            if (key == "api_key" && !is_blank_string(value)) || holds_secret(value) {
                bail!(
                    "an API key is never written to config.toml; it belongs in Credential Manager"
                );
            }
        }
        let create = !matches!(self, FieldChange::Remove { .. });
        let mut table: &mut dyn TableLike = doc.as_table_mut();
        let mut parents = parents.iter().peekable();
        while let Some(name) = parents.next() {
            // A number after a list's name is a position in that list of
            // tables (`providers.compat.1.api_key`): the one entry's field
            // changes and its neighbours and comments stay.
            if let Some(index) = parents.peek().and_then(|n| n.parse::<usize>().ok()) {
                let list = table
                    .get_mut(name)
                    .and_then(Item::as_array_of_tables_mut)
                    .with_context(|| format!("`{name}` in config.toml is not a list of tables"))?;
                parents.next();
                table = list
                    .get_mut(index)
                    .map(|t| t as &mut dyn TableLike)
                    .with_context(|| format!("`{name}` in config.toml has no entry {index}"))?;
                continue;
            }
            if !table.contains_key(name) {
                if !create {
                    return Ok(());
                }
                let mut made = Table::new();
                // Implicit: only `[a.b]` is printed, not an empty `[a]` above it.
                made.set_implicit(true);
                table.insert(name, Item::Table(made));
            }
            table = table
                .get_mut(name)
                .and_then(Item::as_table_like_mut)
                .with_context(|| format!("`{name}` in config.toml is not a table"))?;
        }
        match self {
            FieldChange::Remove { .. } => {
                table.remove(key);
            }
            // Returned early above, as a plain `Set`.
            FieldChange::SetIfUnchanged { .. } => {}
            FieldChange::Set { value, .. } => {
                let mut item = to_item(value);
                match table.get_mut(key) {
                    // Replace the value where it stands. `insert` would replace
                    // the key too, and with it the comment lines above it.
                    Some(slot) => {
                        match (&*slot, &mut item) {
                            (Item::Value(old), Item::Value(new)) => {
                                *new.decor_mut() = old.decor().clone();
                            }
                            // A whole table or list of tables: the new tables
                            // take the old ones' comments and place in the file,
                            // or they would lose the comment above them and move
                            // to the end.
                            (Item::Table(old), Item::Table(new)) => keep_place(old, new),
                            (Item::ArrayOfTables(old), Item::ArrayOfTables(new)) => {
                                let last = old.len().checked_sub(1);
                                for (i, table) in new.iter_mut().enumerate() {
                                    if let Some(old) =
                                        old.get(i).or_else(|| last.and_then(|l| old.get(l)))
                                    {
                                        keep_place(old, table);
                                    }
                                }
                            }
                            _ => {}
                        }
                        let comment = match (&*slot, &item) {
                            (Item::ArrayOfTables(old), Item::Value(_)) => {
                                old.get(0).and_then(|t| t.decor().prefix().cloned())
                            }
                            _ => None,
                        };
                        *slot = item;
                        // A list of tables replaced by an empty list becomes a
                        // plain `key = []`; the comment that stood above the
                        // first table moves to the key.
                        if let Some(prefix) = comment {
                            if let Some(mut k) = table.key_mut(key) {
                                k.leaf_decor_mut().set_prefix(prefix);
                                k.leaf_decor_mut().set_suffix(" ");
                            }
                        }
                    }
                    None => {
                        table.insert(key, item);
                    }
                }
            }
        }
        Ok(())
    }
}

/// `new` takes `old`'s comments and position in the file.
fn keep_place(old: &Table, new: &mut Table) {
    *new.decor_mut() = old.decor().clone();
    new.set_position(old.position());
}

fn is_blank_string(v: &toml::Value) -> bool {
    matches!(v, toml::Value::String(s) if s.is_empty())
}

/// True if `v` is, or contains at any depth, a non-empty `api_key`.
fn holds_secret(v: &toml::Value) -> bool {
    match v {
        toml::Value::Table(t) => t
            .iter()
            .any(|(k, v)| (k == "api_key" && !is_blank_string(v)) || holds_secret(v)),
        toml::Value::Array(a) => a.iter().any(holds_secret),
        _ => false,
    }
}

/// Empties every non-empty `api_key` at any depth, so a diff neither compares
/// nor carries a hydrated key.
fn blank_secrets(v: &mut toml::Value) {
    match v {
        toml::Value::Table(t) => {
            for (k, v) in t.iter_mut() {
                if k == "api_key" && matches!(v, toml::Value::String(s) if !s.is_empty()) {
                    *v = toml::Value::String(String::new());
                } else {
                    blank_secrets(v);
                }
            }
        }
        toml::Value::Array(a) => a.iter_mut().for_each(blank_secrets),
        _ => {}
    }
}

fn to_item(v: &toml::Value) -> Item {
    match v {
        toml::Value::Table(t) => {
            let mut table = Table::new();
            for (k, v) in t {
                table.insert(k, to_item(v));
            }
            Item::Table(table)
        }
        toml::Value::Array(a) if !a.is_empty() && a.iter().all(toml::Value::is_table) => {
            let mut tables = toml_edit::ArrayOfTables::new();
            for v in a {
                if let Item::Table(t) = to_item(v) {
                    tables.push(t);
                }
            }
            Item::ArrayOfTables(tables)
        }
        other => Item::Value(to_value(other)),
    }
}

/// A `f32` field reaches `toml::Value` widened to `f64`, which prints as
/// `1.100000023841858`. If the value is exactly an `f32`, use that `f32`'s
/// shortest form (`1.1`), which reads back as the same `f32`.
fn shortest_float(f: f64) -> f64 {
    let narrow = f as f32;
    if f64::from(narrow) == f {
        if let Ok(short) = narrow.to_string().parse::<f64>() {
            return short;
        }
    }
    f
}

fn to_value(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::from(s.as_str()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(shortest_float(*f)),
        toml::Value::Boolean(b) => Value::from(*b),
        toml::Value::Datetime(d) => Value::from(*d),
        toml::Value::Array(a) => Value::Array(a.iter().map(to_value).collect()),
        toml::Value::Table(t) => {
            let mut inline = toml_edit::InlineTable::new();
            for (k, v) in t {
                inline.insert(k, to_value(v));
            }
            Value::InlineTable(inline)
        }
    }
}

/// The keys that differ between two configs, as the smallest set of changes:
/// a changed leaf, a key present only after (set), a key present only before
/// (remove). Arrays and the like are one leaf. `api_key` is skipped everywhere,
/// and blanked inside any whole table or array that is set: keys go to
/// Credential Manager, never to the file.
pub fn diff(before: &Config, after: &Config) -> Result<Vec<FieldChange>> {
    let mut before = toml::Value::try_from(before).context("could not read the old settings")?;
    let mut after = toml::Value::try_from(after).context("could not read the new settings")?;
    blank_secrets(&mut before);
    blank_secrets(&mut after);
    let mut out = Vec::new();
    walk(&mut Vec::new(), &before, &after, &mut out);
    Ok(out)
}

fn walk(
    path: &mut Vec<String>,
    before: &toml::Value,
    after: &toml::Value,
    out: &mut Vec<FieldChange>,
) {
    match (before, after) {
        (toml::Value::Table(b), toml::Value::Table(a)) => {
            for (key, new) in a {
                path.push(key.clone());
                match b.get(key) {
                    Some(old) => walk(path, old, new, out),
                    None => push_set(path, new, out),
                }
                path.pop();
            }
            for key in b.keys().filter(|k| !a.contains_key(*k)) {
                path.push(key.clone());
                if path.last().map(String::as_str) != Some("api_key") {
                    out.push(FieldChange::Remove { path: path.clone() });
                }
                path.pop();
            }
        }
        (b, a) if b != a => push_set(path, a, out),
        _ => {}
    }
}

fn push_set(path: &[String], value: &toml::Value, out: &mut Vec<FieldChange>) {
    if path.last().map(String::as_str) == Some("api_key") {
        return;
    }
    out.push(FieldChange::Set {
        path: path.to_vec(),
        value: value.clone(),
    });
}

/// The changes that blank, in the file, the keys a load just moved into
/// Credential Manager (#899): `before` is the config as the file read it,
/// `after` the same once the import ran. A provider whose key went from set to
/// empty is one `api_key = ""`; so is each compat entry, addressed by its
/// position in the list, with a check that the entry there still has the same
/// name (a list that changed meanwhile fails the write instead of blanking
/// another entry's key). A key that could not be stored is not in the changes:
/// it stays in the file and is imported on the next load rather than be lost.
/// Comments and unknown keys, inside compat entries too, are untouched.
pub fn blanked_key_changes(before: &Config, after: &Config) -> Result<Vec<FieldChange>> {
    let tb = toml::Value::try_from(before).context("could not read the old settings")?;
    let ta = toml::Value::try_from(after).context("could not read the new settings")?;
    let mut out = Vec::new();
    for provider in ["openai", "anthropic", "gemini"] {
        let path = vec![
            "providers".to_string(),
            provider.to_string(),
            "api_key".to_string(),
        ];
        let was = value_at(&tb, &path).is_some_and(|v| !is_blank_string(v));
        let now = !value_at(&ta, &path).is_some_and(|v| !is_blank_string(v));
        if was && now {
            out.push(FieldChange::Set {
                path,
                value: toml::Value::String(String::new()),
            });
        }
    }
    let list = ["providers".to_string(), "compat".to_string()];
    let (was, now) = (value_at(&tb, &list), value_at(&ta, &list));
    if let (Some(toml::Value::Array(was)), Some(toml::Value::Array(now))) = (was, now) {
        if was.len() != now.len() {
            bail!("the compat list changed while its keys were imported");
        }
        for (i, (was, now)) in was.iter().zip(now).enumerate() {
            let key = |v: &toml::Value| v.get("api_key").is_some_and(|k| !is_blank_string(k));
            if !key(was) || key(now) {
                continue;
            }
            let entry = |field: &str| -> Vec<String> {
                let mut p = list.to_vec();
                p.push(i.to_string());
                p.push(field.to_string());
                p
            };
            if let Some(name) = was.get("name") {
                // Sets the name to what it is: a check that this is the entry.
                out.push(FieldChange::SetIfUnchanged {
                    path: entry("name"),
                    expected: Some(name.clone()),
                    value: name.clone(),
                });
            }
            out.push(FieldChange::Set {
                path: entry("api_key"),
                value: toml::Value::String(String::new()),
            });
        }
    }
    Ok(out)
}

/// Drops every change whose key is changed again later, keeping the later one
/// ("latest wins") and the order of the survivors.
pub fn coalesce(changes: Vec<FieldChange>) -> Vec<FieldChange> {
    let keep: Vec<bool> = (0..changes.len())
        .map(|i| {
            !changes[i + 1..]
                .iter()
                .any(|later| later.path() == changes[i].path())
        })
        .collect();
    changes
        .into_iter()
        .zip(keep)
        .filter_map(|(c, k)| k.then_some(c))
        .collect()
}

/// Writes the jobs that queued up together as one file write (latest value of
/// each key wins) and returns one result per job, in order. If that combined
/// write is refused, each job is tried alone, so one refused job (an API key,
/// say) cannot fail the others; a broken file still fails them all, each with
/// the same sentence.
fn write_batch(path: &Path, jobs: &[&[FieldChange]]) -> Vec<Result<(), String>> {
    let all: Vec<FieldChange> = jobs.iter().flat_map(|j| j.iter().cloned()).collect();
    match write_changes(path, &coalesce(all)) {
        Ok(()) => vec![Ok(()); jobs.len()],
        Err(e) if jobs.len() <= 1 => vec![Err(format!("{e:#}")); jobs.len()],
        Err(_) => jobs
            .iter()
            .map(|j| write_changes(path, &coalesce(j.to_vec())).map_err(|e| format!("{e:#}")))
            .collect(),
    }
}

/// `config` with `changes` applied on top, for changes that could not be written
/// to the file but are live until quit: a reload from disk would otherwise
/// silently revert them. Load-time notes that are not part of the file
/// (`unreadable_secrets`, `keys_left_in_file`, `load_problem`) carry over.
pub fn reapply(config: &Config, changes: &[FieldChange]) -> Result<Config> {
    let mut value = toml::Value::try_from(config).context("could not read the settings")?;
    for change in changes {
        change_value(&mut value, change)?;
    }
    let mut out: Config = value
        .try_into()
        .context("could not apply the unsaved change")?;
    out.unreadable_secrets = config.unreadable_secrets.clone();
    out.keys_left_in_file = config.keys_left_in_file.clone();
    out.load_problem = config.load_problem.clone();
    Ok(out)
}

fn change_value(root: &mut toml::Value, change: &FieldChange) -> Result<()> {
    let Some((key, parents)) = change.path().split_last() else {
        bail!("a config change needs a key");
    };
    let mut at = root;
    for name in parents {
        let table = at
            .as_table_mut()
            .with_context(|| format!("`{name}` is not a table"))?;
        at = table
            .entry(name.clone())
            .or_insert_with(|| toml::Value::Table(Default::default()));
    }
    let table = at
        .as_table_mut()
        .with_context(|| format!("`{key}` is not in a table"))?;
    match change {
        FieldChange::Set { value, .. } | FieldChange::SetIfUnchanged { value, .. } => {
            table.insert(key.clone(), value.clone());
        }
        FieldChange::Remove { .. } => {
            table.remove(key);
        }
    }
    Ok(())
}

/// Makes every whole-value change in `changes` (a list or a table) conditional on
/// the file still holding what `before` held there. `diff` hands a changed list
/// over as one value, so applying it would also replace anything the user added
/// to that list by hand since the window opened; with this the write is refused
/// instead and says so. Leaf changes are left alone: a typed value wins over an
/// older one.
pub fn guard_whole_values(changes: Vec<FieldChange>, before: &Config) -> Result<Vec<FieldChange>> {
    let mut before = toml::Value::try_from(before).context("could not read the old settings")?;
    blank_secrets(&mut before);
    Ok(changes
        .into_iter()
        .map(|change| match change {
            FieldChange::Set { path, value } if value.is_array() || value.is_table() => {
                let expected = value_at(&before, &path).cloned();
                FieldChange::SetIfUnchanged {
                    path,
                    expected,
                    value,
                }
            }
            other => other,
        })
        .collect())
}

/// The settings as `text` (the file's contents) holds them, defaults for every key
/// it does not spell out: what [`guard_whole_values`] must compare against, since
/// the writer compares with the file read the same way. The running config is not
/// that: a tray change not yet written, or a default the file never carried, would
/// make an untouched list look hand-edited.
pub fn baseline_from_text(text: &str) -> Config {
    Config::parse_or_default(text)
}

/// [`baseline_from_text`] for the file at `path`, read once when Settings opens.
/// A missing file is all defaults, as the writer treats it. `None` when it cannot
/// be read: the caller falls back to the running config, and a whole-value change
/// the file then disagrees with is refused (the safe outcome).
pub fn file_baseline(path: &Path) -> Option<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(baseline_from_text(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(baseline_from_text("")),
        Err(_) => None,
    }
}

/// The value at `path`; a number steps into a list by position.
fn value_at<'a>(root: &'a toml::Value, path: &[String]) -> Option<&'a toml::Value> {
    path.iter().try_fold(root, |at, key| match at {
        toml::Value::Array(list) => list.get(key.parse::<usize>().ok()?),
        _ => at.get(key.as_str()),
    })
}

/// Refuses a [`FieldChange::SetIfUnchanged`] whose key no longer holds what the
/// change expected. `on_disk` is the file as `Config` reads it, defaults for
/// missing keys included, so a list the file never spelled out still compares.
fn check_unchanged(on_disk: &toml::Value, change: &FieldChange) -> Result<()> {
    let FieldChange::SetIfUnchanged { path, expected, .. } = change else {
        return Ok(());
    };
    if value_at(on_disk, path) != expected.as_ref() {
        bail!(
            "`{}` was changed in config.toml while Settings was open, so Wingman did not \
             overwrite it. Open Settings again and save once more",
            path.join(".")
        );
    }
    Ok(())
}

/// The file text as `Config` reads it, as a value with every key blanked, for
/// [`check_unchanged`]. `None` when no change needs the comparison, or it could
/// not be made (a guarded change is then refused: the safe outcome).
fn effective_on_disk(text: &str, changes: &[FieldChange]) -> Option<toml::Value> {
    if !changes
        .iter()
        .any(|c| matches!(c, FieldChange::SetIfUnchanged { .. }))
    {
        return None;
    }
    let mut value = toml::Value::try_from(Config::parse_or_default(text)).ok()?;
    blank_secrets(&mut value);
    Some(value)
}

/// Reads `path` fresh, applies `changes`, writes atomically. See the module doc
/// for the rules.
pub fn write_changes(path: &Path, changes: &[FieldChange]) -> Result<()> {
    // No changes (a reload, #901) is no write: not even a file that does not
    // parse is refused for it.
    if changes.is_empty() {
        return Ok(());
    }
    write_changes_with(path, changes, &mut |_| {})
}

/// [`write_changes`] with a hook that runs after each read and patch, before the
/// "did the file change" check, so a test can edit the file at that exact point.
fn write_changes_with(
    path: &Path,
    changes: &[FieldChange],
    between_read_and_write: &mut dyn FnMut(u32),
) -> Result<()> {
    for attempt in 0..2 {
        let seen = crate::config::file_mtime(path);
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            // No file yet: the defaults stand in, and the file starts with the
            // changed keys only.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => bail!("config.toml could not be read, so Wingman did not change it"),
        };
        refuse_unparseable(&text)?;
        let mut doc: DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| anyhow::anyhow!(refusal(&text, e.span())))?;
        let on_disk = effective_on_disk(&text, changes);
        for change in changes {
            match &on_disk {
                Some(on_disk) => check_unchanged(on_disk, change)?,
                None if matches!(change, FieldChange::SetIfUnchanged { .. }) => {
                    bail!(
                        "could not check config.toml before changing `{}`",
                        change.path().join(".")
                    )
                }
                None => {}
            }
            change.apply(&mut doc)?;
        }
        let mut out = match_line_endings(&text, doc.to_string());
        // `toml_edit` drops a leading byte order mark (Notepad writes one); the
        // user's file keeps it.
        if text.starts_with('\u{feff}') && !out.starts_with('\u{feff}') {
            out.insert(0, '\u{feff}');
        }
        // A patch must never produce a file Wingman itself would refuse to load.
        if let Err(e) = toml::from_str::<Config>(&out) {
            bail!(
                "that change would leave config.toml unreadable ({}), so nothing was written",
                crate::config::describe_error_span(&out, e.span())
            );
        }
        between_read_and_write(attempt);
        // The check and the write happen under one hold of the write lock, so
        // another writer cannot slip a write in between them.
        let _one_writer = crate::config::lock_writes();
        if crate::config::file_mtime(path) != seen {
            continue;
        }
        if out == text {
            return Ok(());
        }
        return crate::config::write_file_atomically_held(path, &out, false);
    }
    bail!("config.toml changed while Wingman was saving it, so nothing was written. Try again")
}

fn refusal(text: &str, span: Option<std::ops::Range<usize>>) -> String {
    format!(
        "{}, so Wingman did not change it. Fix the file, then choose Reload settings (tray menu, Help and diagnostics)",
        crate::config::describe_error_span(text, span)
    )
}

/// The file as it stands must be one Wingman can load: not just TOML, but a
/// `Config`. Otherwise a patch could rewrite a file the user is halfway through
/// fixing.
fn refuse_unparseable(text: &str) -> Result<()> {
    if let Err(e) = toml::from_str::<Config>(text) {
        bail!(refusal(text, e.span()));
    }
    Ok(())
}

/// `toml_edit` writes `\n` for any line it adds and for some it keeps. A file
/// that uses `\r\n` (a Windows checkout with `core.autocrlf=true`, Notepad) must
/// stay that way, not turn into a mixed file. So the result gets the original's
/// dominant ending throughout. A file that already mixed the two is normalised
/// to whichever it used more.
fn match_line_endings(original: &str, patched: String) -> String {
    let crlf = original.matches("\r\n").count();
    let lf = original.matches('\n').count();
    let wants_crlf = crlf * 2 > lf;
    if crlf == 0 && !wants_crlf {
        return patched;
    }
    let unix = patched.replace("\r\n", "\n");
    if wants_crlf {
        unix.replace('\n', "\r\n")
    } else {
        unix
    }
}

/// What the writer thread tells the caller when a job is done.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteReply {
    /// What the caller called the job (for its card or status line).
    pub label: String,
    /// `Err` holds a plain sentence.
    pub result: Result<(), String>,
}

/// The file as `Config::load` read it after a write, and the modified time that
/// read saw. The UI thread adopts the time when (and only when) it swaps the
/// config in; see `config::load_deferring_mtime`.
pub struct Loaded {
    pub config: Result<Box<Config>, String>,
    pub mtime: Option<std::time::SystemTime>,
}

/// The UI thread has taken a [`Loaded`] as its config: from now on a whole-file
/// save is judged against what was read then (`Loaded::mtime`).
pub fn adopt_mtime(mtime: Option<std::time::SystemTime>) {
    if let Ok(path) = Config::path() {
        crate::config::note_mtime(&path, mtime);
    }
}

/// What the cards say once a write is done. Different tray changes word them
/// differently.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ok_headline: String,
    pub ok_detail: String,
    pub ok_secs: u32,
    /// Show the OK card as a setup card (it points at Settings and stays).
    pub ok_setup: bool,
    pub fail_headline: String,
    /// Said under the failure, after "It ...".
    pub fail_note: String,
    /// The change comes from the Settings window: it is not kept in `unsaved` when
    /// it fails (the running settings stay as they were), its OK card gives way to
    /// an answer already on screen, and a press of the Wingman key is refused until
    /// its answer has been applied.
    pub from_settings: bool,
    /// The job is the tray's Reload settings: a reload is the file as the truth
    /// again, so a change that failed to save earlier is dropped, and a key the
    /// store could not give back is said.
    pub from_reload: bool,
    /// Say nothing when the write works: something else on screen (the Settings
    /// window's own controls) already shows the change. A failure still ends in a
    /// card.
    pub quiet_ok: bool,
}

impl Outcome {
    /// "<label>" on success; "Using <label>: not saved" and "It will revert when
    /// you quit." on failure.
    pub fn simple(label: &str) -> Self {
        Self {
            ok_headline: label.to_string(),
            ok_detail: String::new(),
            ok_secs: 3,
            ok_setup: false,
            fail_headline: format!("Using {label}: not saved"),
            fail_note: "It will revert when you quit.".to_string(),
            from_settings: false,
            from_reload: false,
            quiet_ok: false,
        }
    }

    /// "Reload settings" from the tray (#901): no field changes, only the read.
    pub fn reload() -> Self {
        Self {
            ok_headline: "Settings reloaded".to_string(),
            fail_headline: "Couldn't reload settings".to_string(),
            fail_note: "Wingman is still using the settings it had.".to_string(),
            from_reload: true,
            ..Self::simple("Settings reloaded")
        }
    }

    /// [`Outcome::simple`] without the card on success (#873): a change made in
    /// the Settings window must not put a card over it.
    pub fn quiet(label: &str) -> Self {
        Self {
            quiet_ok: true,
            ..Self::simple(label)
        }
    }

    /// A save from the Settings window. On failure nothing was applied, and a key
    /// typed there was not stored.
    pub fn settings_save() -> Self {
        Self {
            ok_headline: "Settings saved".to_string(),
            ok_detail: String::new(),
            ok_secs: 3,
            ok_setup: false,
            fail_headline: "Couldn't save settings".to_string(),
            fail_note: "Your changes were not applied and Wingman is still using its previous \
                        settings. A key you typed was not stored, so enter it again after \
                        fixing this."
                .to_string(),
            from_settings: true,
            from_reload: false,
            quiet_ok: false,
        }
    }
}

/// What the app is told when a write finishes: the writer's reply, the changes
/// it was for, how to word the cards, and (on success) the config as
/// `Config::load` now reads it. The load runs on the writer thread, so the
/// Credential Manager reads and any `icacls` call it makes never run on the
/// thread that hosts the keyboard hook.
pub struct Written {
    pub reply: WriteReply,
    pub changes: Vec<FieldChange>,
    pub outcome: Outcome,
    /// `None` when the write failed, or the app is quitting.
    pub loaded: Option<Loaded>,
    /// Set when the file was written but an API key then could not be stored in
    /// Credential Manager (a Settings save with a typed key). A plain sentence.
    pub key_error: Option<String>,
}

/// One write's reply has been dealt with (or could not be posted): takes it off
/// the count of writes in flight and says whether it was the last. Only the last
/// reply may swap in a loaded config; an earlier one was read before a later
/// choice was made and would overwrite it.
pub fn finish(pending: &std::sync::atomic::AtomicU32) -> bool {
    use std::sync::atomic::Ordering;
    let before = pending
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(n.saturating_sub(1))
        })
        .unwrap_or(0);
    before <= 1
}

type Seen = Option<std::time::SystemTime>;

/// Which file mtime the UI thread should adopt after a reply, and which good one
/// to remember for later. `this_ok` is the mtime of this reply's load if it
/// succeeded; `skipped` the newest good one an earlier reply left unadopted
/// (not the last). The last reply adopts its own, or, if it failed, the newest
/// good earlier one: a failed write leaves the file as that load saw it, so
/// Settings saves are not refused for good.
pub fn track_mtime(
    last: bool,
    this_ok: Option<Seen>,
    skipped: Option<Seen>,
) -> (Option<Seen>, Option<Seen>) {
    let newest = this_ok.or(skipped);
    if last {
        (newest, None)
    } else {
        (None, newest)
    }
}

/// Stores the keys a Settings save changed in Credential Manager (see
/// `Config::push_changed_secrets`). `Some` is a plain sentence for the failure.
/// Runs on the writer thread, after the file write has succeeded.
pub fn store_changed_keys(was: &Config, now: &Config) -> Option<String> {
    Config::push_changed_secrets(was, now)
        .err()
        .map(|e| format!("{e:#}"))
}

/// `Config::load` for the handler of [`Written`], with the error as a sentence.
/// Runs on the writer thread.
pub fn load_after_write() -> Loaded {
    let load = || Config::load().map(Box::new).map_err(|e| format!("{e:#}"));
    match Config::path() {
        Ok(path) => {
            let (config, mtime) = crate::config::load_deferring_mtime(&path, load);
            Loaded { config, mtime }
        }
        Err(e) => Loaded {
            config: Err(format!("{e:#}")),
            mtime: None,
        },
    }
}

struct Job {
    changes: Vec<FieldChange>,
    label: String,
    done: Box<dyn FnOnce(WriteReply) + Send>,
}

/// The writer thread's handle. Cheap to share; the thread ends when the last
/// handle is dropped.
pub struct ConfigWriter {
    jobs: mpsc::Sender<Job>,
}

impl ConfigWriter {
    /// Starts the `settings-writer` thread for `path`. It blocks on the channel,
    /// so it costs nothing while idle. Jobs that queue up while it writes are
    /// coalesced into one write (latest value of each key wins) and every job is
    /// told the one result.
    pub fn spawn(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        // If the thread cannot start, `rx` is dropped with the closure and
        // `submit` finds the channel closed, which it reports.
        let _ = std::thread::Builder::new()
            .name("settings-writer".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let mut jobs = vec![first];
                    jobs.extend(rx.try_iter());
                    let sets: Vec<&[FieldChange]> =
                        jobs.iter().map(|j| j.changes.as_slice()).collect();
                    let results = write_batch(&path, &sets);
                    for (job, result) in jobs.into_iter().zip(results) {
                        (job.done)(WriteReply {
                            label: job.label,
                            result,
                        });
                    }
                }
            });
        Self { jobs: tx }
    }

    /// Waits up to `timeout` for everything queued before this call to finish.
    /// True if it did. Jobs run in order, so an empty job's callback firing means
    /// the earlier ones are done.
    pub fn flush(&self, timeout: std::time::Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        self.submit("flush", Vec::new(), move |_| {
            let _ = tx.send(());
        });
        rx.recv_timeout(timeout).is_ok()
    }

    /// Queues `changes`; `done` runs on the writer thread with the result. If the
    /// thread is gone, `done` runs here with an error, so a caller always hears
    /// back.
    pub fn submit(
        &self,
        label: &str,
        changes: Vec<FieldChange>,
        done: impl FnOnce(WriteReply) + Send + 'static,
    ) {
        let job = Job {
            changes,
            label: label.to_string(),
            done: Box::new(done),
        };
        if let Err(mpsc::SendError(job)) = self.jobs.send(job) {
            (job.done)(WriteReply {
                label: job.label,
                result: Err("the settings writer is not running".to_string()),
            });
        }
    }
}

static WRITER: std::sync::OnceLock<ConfigWriter> = std::sync::OnceLock::new();
static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The writer for the real `%APPDATA%\Wingman\config.toml`, started on first use.
/// A failure to find the path is reported but not remembered: the next call
/// looks again.
pub fn global() -> Result<&'static ConfigWriter, String> {
    global_in(&WRITER, Config::path)
}

fn global_in(
    cell: &std::sync::OnceLock<ConfigWriter>,
    path: impl FnOnce() -> Result<PathBuf>,
) -> Result<&ConfigWriter, String> {
    if let Some(writer) = cell.get() {
        return Ok(writer);
    }
    let path = path().map_err(|e| format!("{e:#}"))?;
    Ok(cell.get_or_init(|| ConfigWriter::spawn(path)))
}

/// Waits up to `timeout` for every queued write to finish, then marks the app as
/// quitting (see [`shutting_down`]). Called once, from the window's
/// `WM_DESTROY`. True if there was nothing to wait for or the wait finished.
pub fn flush_global(timeout: std::time::Duration) -> bool {
    SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
    WRITER.get().map_or(true, |w| w.flush(timeout))
}

/// True once [`flush_global`] has run: the app is quitting, so a finished write
/// need not reload the config for a window that is going away.
pub fn shutting_down() -> bool {
    SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A hand-edited file: comments of every kind, an unknown key and table, an
    /// inline table, an array, and values in a non-default order.
    const HAND_EDITED: &str = r#"# My Wingman settings. Hand-edited, please keep.
mode = "auto"   # switch to offline on planes

[hotkeys]
# the Copilot key
primary = { vk = 134, ctrl = false, shift = true, alt = false, win = true }
secondary = { vk = 191, ctrl = true, shift = true, alt = false, win = false }

[providers]
order = ["openai", "anthropic"]  # try OpenAI first

[providers.openai]
model = "gpt-5.5"   # the cheap one
# api_key lives in Credential Manager

[ui]
card_seconds = 12 # I read slowly
# my own notes
favourite_colour = "teal"

[unknown_future_table]
x = 1
"#;

    /// A scratch directory of its own per test (rule 9: never the real
    /// `%APPDATA%\Wingman`).
    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "wingman-config-writer-test-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// #901: a reload sends no changes, so it must neither refuse nor touch a
    /// file that does not parse (the load then reports it as a problem card).
    #[test]
    fn no_changes_write_nothing_and_do_not_refuse_a_broken_file() {
        let path = scratch("no-changes");
        std::fs::write(&path, "mode = [unclosed\r\n").unwrap();
        write_changes(&path, &[]).expect("an empty job is not a write");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "mode = [unclosed\r\n"
        );
        let missing = path.with_file_name("absent.toml");
        write_changes(&missing, &[]).expect("nothing to write");
        assert!(!missing.exists());
        cleanup(&path);
    }

    #[test]
    fn a_reload_outcome_is_marked_and_worded() {
        let o = Outcome::reload();
        assert!(o.from_reload && !o.from_settings && !o.ok_setup);
        assert_eq!(o.ok_headline, "Settings reloaded");
        assert_eq!(o.fail_headline, "Couldn't reload settings");
        assert!(!o.fail_headline.contains("Using"), "{}", o.fail_headline);
        assert!(!o.fail_note.contains("revert"), "{}", o.fail_note);
        assert!(!o.fail_headline.contains('\u{2014}') && !o.fail_note.contains('\u{2014}'));
        assert!(!Outcome::simple("x").from_reload);
        assert!(!Outcome::settings_save().from_reload);
    }

    fn crlf(s: &str) -> String {
        s.replace("\r\n", "\n").replace('\n', "\r\n")
    }

    fn set(path: &[&str], v: impl serde::Serialize) -> FieldChange {
        FieldChange::set(path, v).unwrap()
    }

    /// Writes `text` to a scratch file, applies `changes`, returns what is on
    /// disk afterwards.
    fn patched(tag: &str, text: &str, changes: &[FieldChange]) -> String {
        let path = scratch(tag);
        std::fs::write(&path, text).unwrap();
        write_changes(&path, changes).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        cleanup(&path);
        out
    }

    /// `text` with exactly one substring swapped, which asserts it was there.
    fn replaced(text: &str, from: &str, to: &str) -> String {
        assert!(text.contains(from), "fixture lost {from:?}");
        text.replacen(from, to, 1)
    }

    fn chord(vk: u32, ctrl: bool) -> crate::hotkey::Chord {
        crate::hotkey::Chord {
            vk,
            ctrl,
            shift: false,
            alt: false,
            win: false,
        }
    }

    // -- one field changes, everything else stays byte for byte -------------

    #[test]
    fn changing_one_field_leaves_every_other_byte_alone() {
        let out = patched(
            "one-field",
            HAND_EDITED,
            &[FieldChange::mode(Mode::Offline)],
        );
        assert_eq!(
            out,
            replaced(HAND_EDITED, r#"mode = "auto""#, r#"mode = "offline""#)
        );
    }

    #[test]
    fn the_changed_value_keeps_its_trailing_comment() {
        let out = patched(
            "trailing",
            HAND_EDITED,
            &[set(&["ui", "card_seconds"], 30u32)],
        );
        assert!(out.contains("card_seconds = 30 # I read slowly"), "{out}");
    }

    #[test]
    fn comments_unknown_keys_and_order_survive_several_changes() {
        let out = patched(
            "several",
            HAND_EDITED,
            &[
                FieldChange::mode(Mode::Cloud),
                set(&["ui", "card_seconds"], 5u32),
                set(&["providers", "openai", "model"], "gpt-5.5-mini"),
            ],
        );
        let expected = replaced(
            &replaced(
                &replaced(HAND_EDITED, r#"mode = "auto""#, r#"mode = "cloud""#),
                "card_seconds = 12",
                "card_seconds = 5",
            ),
            r#"model = "gpt-5.5""#,
            r#"model = "gpt-5.5-mini""#,
        );
        assert_eq!(out, expected);
        assert!(out.contains("favourite_colour = \"teal\""));
        assert!(out.contains("[unknown_future_table]"));
    }

    #[test]
    fn a_hand_edit_made_since_the_window_opened_is_not_overwritten() {
        // The writer has no memory of what the file said earlier: it reads the
        // file now. A value the user changed by hand in the meantime survives a
        // change to a different field.
        let edited = replaced(HAND_EDITED, "card_seconds = 12", "card_seconds = 99");
        let out = patched("hand-edit", &edited, &[FieldChange::mode(Mode::Local)]);
        assert!(out.contains("card_seconds = 99 # I read slowly"), "{out}");
        assert!(out.contains(r#"mode = "local""#));
    }

    // -- a field that is not in the file yet --------------------------------

    #[test]
    fn a_missing_key_is_added_inside_its_existing_table() {
        let out = patched(
            "missing-key",
            HAND_EDITED,
            &[set(&["ui", "text_scale"], 1.25f32)],
        );
        let loaded: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(loaded["ui"]["text_scale"].as_float(), Some(1.25));
        assert_eq!(loaded["ui"]["card_seconds"].as_integer(), Some(12));
        assert!(out.contains("# my own notes"));
        // Added to [ui], not to the table after it.
        let ui = out.find("[ui]").unwrap();
        let unknown = out.find("[unknown_future_table]").unwrap();
        let added = out.find("text_scale").unwrap();
        assert!(ui < added && added < unknown, "{out}");
    }

    #[test]
    fn a_missing_table_is_created_with_a_plain_header() {
        let out = patched(
            "missing-table",
            HAND_EDITED,
            &[set(&["capture", "redact"], true)],
        );
        assert!(out.contains("[capture]\nredact = true"), "{out}");
        assert!(
            out.starts_with(HAND_EDITED),
            "existing text must be untouched:\n{out}"
        );
    }

    #[test]
    fn a_missing_nested_table_prints_only_the_leaf_header() {
        let out = patched(
            "missing-nested",
            "",
            &[set(&["providers", "ollama", "model"], "qwen3-vl")],
        );
        assert_eq!(out, "[providers.ollama]\nmodel = \"qwen3-vl\"\n");
    }

    #[test]
    fn a_missing_top_level_key_is_added() {
        let out = patched(
            "missing-top",
            "[ui]\ncard_seconds = 4\n",
            &[FieldChange::mode(Mode::Auto)],
        );
        let loaded: Config = toml::from_str(&out).unwrap();
        assert_eq!(loaded.mode, Mode::Auto);
        assert_eq!(loaded.ui.card_seconds, 4);
    }

    #[test]
    fn a_missing_file_is_created_holding_only_the_changed_key() {
        let path = scratch("no-file");
        assert!(!path.exists());
        write_changes(&path, &[FieldChange::mode(Mode::Offline)]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "mode = \"offline\"\n"
        );
        cleanup(&path);
    }

    // -- arrays and inline tables -------------------------------------------

    #[test]
    fn an_array_is_replaced_and_the_comments_around_it_survive() {
        let out = patched(
            "array",
            HAND_EDITED,
            &[set(&["providers", "order"], vec!["gemini", "openai"])],
        );
        let loaded: Config = toml::from_str(&out).unwrap();
        assert_eq!(loaded.providers.order, vec!["gemini", "openai"]);
        assert!(out.contains("# try OpenAI first"), "{out}");
        assert!(out.contains("# the Copilot key"));
    }

    #[test]
    fn a_key_inside_an_inline_table_is_patched_in_place() {
        let out = patched(
            "inline",
            HAND_EDITED,
            &[set(&["hotkeys", "primary", "vk"], 135u32)],
        );
        assert_eq!(
            out,
            replaced(
                HAND_EDITED,
                "primary = { vk = 134,",
                "primary = { vk = 135,"
            )
        );
    }

    #[test]
    fn a_missing_key_is_added_inside_an_inline_table() {
        let text = "[hotkeys]\npalette = { vk = 32, ctrl = true, shift = false, alt = false, win = false }\n";
        let out = patched(
            "inline-add",
            text,
            &[set(&["hotkeys", "palette", "label"], "Quick Ask")],
        );
        let loaded: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(
            loaded["hotkeys"]["palette"]["label"].as_str(),
            Some("Quick Ask")
        );
        assert_eq!(loaded["hotkeys"]["palette"]["vk"].as_integer(), Some(32));
        assert!(out.contains("palette = {"), "still inline: {out}");
    }

    #[test]
    fn an_array_of_tables_is_replaced_and_the_rest_is_kept() {
        let text = "# endpoints\n[[providers.compat]]\nname = \"a\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\n[ui]\ncard_seconds = 7 # keep\n";
        let compat = vec![toml::Value::try_from(crate::config::CompatConfig {
            name: "b".into(),
            ..Default::default()
        })
        .unwrap()];
        let out = patched(
            "aot",
            text,
            &[FieldChange::Set {
                path: vec!["providers".into(), "compat".into()],
                value: toml::Value::Array(compat),
            }],
        );
        let loaded: Config = toml::from_str(&out).unwrap();
        assert_eq!(loaded.providers.compat.len(), 1);
        assert_eq!(loaded.providers.compat[0].name, "b");
        assert!(out.contains("card_seconds = 7 # keep"), "{out}");
    }

    const COMPAT_WITH_COMMENTS: &str = "# top\n[[providers.compat]]\n# the lab box\nname = \"lab\"\nbase_url = \"http://127.0.0.1:1/v1\"\nmodel = \"m\"\nmystery = 7\napi_key = \"k-lab\" # old\n\n[[providers.compat]]\nname = \"home\"\nbase_url = \"http://127.0.0.1:2/v1\"\nmodel = \"m\"\napi_key = \"k-home\"\n";

    fn two_compat(keys: [&str; 2]) -> Config {
        let mut config: Config = toml::from_str(COMPAT_WITH_COMMENTS).unwrap();
        for (entry, key) in config.providers.compat.iter_mut().zip(keys) {
            entry.api_key = key.to_string();
        }
        config
    }

    #[test]
    fn blanking_compat_keys_touches_only_the_key_fields() {
        // #899: a comment or unknown key inside a compat entry survives.
        let before = two_compat(["k-lab", "k-home"]);
        let after = two_compat(["", ""]);
        let changes = blanked_key_changes(&before, &after).unwrap();
        let out = patched("blank-compat", COMPAT_WITH_COMMENTS, &changes);
        assert!(!out.contains("k-lab") && !out.contains("k-home"), "{out}");
        assert!(out.contains("# the lab box"), "{out}");
        assert!(out.contains("mystery = 7"), "{out}");
        assert!(out.contains("# top"), "{out}");
        let loaded: Config = toml::from_str(&out).unwrap();
        assert_eq!(loaded.providers.compat.len(), 2);
        assert_eq!(loaded.providers.compat[1].name, "home");
    }

    #[test]
    fn a_compat_key_that_was_not_stored_stays_in_the_file() {
        // #899: only the entries whose key was stored are blanked.
        let before = two_compat(["k-lab", "k-home"]);
        let after = two_compat(["", "k-home"]);
        let changes = blanked_key_changes(&before, &after).unwrap();
        let out = patched("blank-one-compat", COMPAT_WITH_COMMENTS, &changes);
        assert!(!out.contains("k-lab"), "{out}");
        assert!(out.contains("api_key = \"k-home\""), "{out}");
        assert!(out.contains("mystery = 7"), "{out}");
    }

    #[test]
    fn blanking_a_compat_key_refuses_when_the_entry_moved() {
        // The index is only a position: if the list changed under the load, the
        // entry at that place is another one, so nothing is written.
        let before = two_compat(["k-lab", "k-home"]);
        let after = two_compat(["", ""]);
        let changes = blanked_key_changes(&before, &after).unwrap();
        let moved = "[[providers.compat]]\nname = \"home\"\nbase_url = \"http://127.0.0.1:2/v1\"\nmodel = \"m\"\napi_key = \"k-home\"\n\n[[providers.compat]]\nname = \"lab\"\nbase_url = \"http://127.0.0.1:1/v1\"\nmodel = \"m\"\napi_key = \"k-lab\"\n";
        let path = scratch("blank-compat-moved");
        std::fs::write(&path, moved).unwrap();
        assert!(write_changes(&path, &changes).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), moved);
        cleanup(&path);
    }

    #[test]
    fn a_dotted_key_table_is_patched_too() {
        let text = "ui.card_seconds = 9 # dotted\nui.favourite = \"x\"\n";
        let out = patched("dotted", text, &[set(&["ui", "card_seconds"], 3u32)]);
        assert_eq!(out, "ui.card_seconds = 3 # dotted\nui.favourite = \"x\"\n");
    }

    // -- removing -------------------------------------------------------------

    #[test]
    fn removing_a_key_drops_only_that_key() {
        let out = patched(
            "remove",
            HAND_EDITED,
            &[FieldChange::Remove {
                path: vec!["ui".into(), "card_seconds".into()],
            }],
        );
        assert!(!out.contains("card_seconds"));
        assert!(out.contains("favourite_colour"));
    }

    #[test]
    fn removing_a_key_that_is_not_there_changes_nothing() {
        let out = patched(
            "remove-absent",
            HAND_EDITED,
            &[FieldChange::Remove {
                path: vec!["nope".into(), "nothing".into()],
            }],
        );
        assert_eq!(out, HAND_EDITED);
    }

    // -- CRLF (autocrlf checkouts on Windows, Notepad) -------------------------

    #[test]
    fn a_crlf_file_stays_crlf_when_a_value_is_replaced() {
        let text = crlf(HAND_EDITED);
        let out = patched("crlf-replace", &text, &[FieldChange::mode(Mode::Offline)]);
        assert_eq!(
            out,
            replaced(&text, r#"mode = "auto""#, r#"mode = "offline""#)
        );
    }

    #[test]
    fn a_crlf_file_stays_crlf_when_keys_and_tables_are_added() {
        let text = crlf(HAND_EDITED);
        let out = patched(
            "crlf-add",
            &text,
            &[
                set(&["ui", "text_scale"], 1.5f32),
                set(&["capture", "redact"], true),
            ],
        );
        let bare_lf = out.replace("\r\n", "").contains('\n');
        assert!(!bare_lf, "a bare LF crept into a CRLF file:\n{out:?}");
        assert!(out.contains("text_scale = 1.5\r\n"));
        assert!(out.contains("[capture]\r\nredact = true\r\n"));
        assert!(out.contains("# my own notes\r\n"));
    }

    #[test]
    fn a_file_that_mixes_endings_is_written_with_the_one_it_uses_most() {
        let mostly_crlf =
            "mode = \"auto\"\r\n[ui]\r\ncard_seconds = 4\ncard_monitor = \"cursor\"\r\n";
        let out = patched(
            "mixed-crlf",
            mostly_crlf,
            &[set(&["ui", "card_seconds"], 6u32)],
        );
        assert!(!out.replace("\r\n", "").contains('\n'), "{out:?}");
        assert!(out.contains("card_seconds = 6\r\n"));
        let mostly_lf = "mode = \"auto\"\r\n[ui]\ncard_seconds = 4\ncard_monitor = \"cursor\"\n";
        let out = patched("mixed-lf", mostly_lf, &[set(&["ui", "card_seconds"], 6u32)]);
        assert!(!out.contains('\r'), "{out:?}");
        assert!(out.contains("card_seconds = 6\n"));
    }

    #[test]
    fn an_lf_file_stays_lf() {
        let out = patched("lf", HAND_EDITED, &[set(&["capture", "redact"], true)]);
        assert!(!out.contains('\r'));
    }

    // -- a file that does not parse is never written ---------------------------

    #[test]
    fn a_file_that_does_not_parse_is_refused_and_left_exactly_as_it_is() {
        let broken = "mode = \"auto\"\n[providers.openai]\napi_key = \"sk-SECRET-VALUE\nmodel = \n";
        let path = scratch("broken");
        std::fs::write(&path, broken).unwrap();
        let err = write_changes(&path, &[FieldChange::mode(Mode::Offline)]).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("line"), "{message}");
        assert!(message.contains("did not change it"), "{message}");
        assert!(
            !message.contains("SECRET"),
            "the error must not quote file text: {message}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        let tmp = path.with_extension("toml.tmp");
        assert!(!tmp.exists(), "no temp file may be left behind");
        cleanup(&path);
    }

    #[test]
    fn a_file_that_is_toml_but_not_a_config_is_refused_too() {
        let text = "mode = \"sideways\"\n";
        let path = scratch("bad-type");
        std::fs::write(&path, text).unwrap();
        assert!(write_changes(&path, &[set(&["ui", "card_seconds"], 3u32)]).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        cleanup(&path);
    }

    #[test]
    fn a_patch_that_would_make_the_file_unloadable_writes_nothing() {
        let path = scratch("bad-patch");
        std::fs::write(&path, HAND_EDITED).unwrap();
        // card_seconds is a number; a string there would not load.
        let err = write_changes(&path, &[set(&["ui", "card_seconds"], "soon")]).unwrap_err();
        assert!(
            format!("{err:#}").contains("nothing was written"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HAND_EDITED);
        cleanup(&path);
    }

    #[test]
    fn a_key_that_is_not_a_table_is_reported_not_overwritten() {
        let path = scratch("not-table");
        std::fs::write(&path, "[ui]\ncard_seconds = 4\n").unwrap();
        let err =
            write_changes(&path, &[set(&["ui", "card_seconds", "deeper"], 1u32)]).unwrap_err();
        assert!(format!("{err:#}").contains("is not a table"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ui]\ncard_seconds = 4\n"
        );
        cleanup(&path);
    }

    // -- no API key in the file ------------------------------------------------

    #[test]
    fn an_api_key_change_is_refused() {
        let path = scratch("api-key");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let err = write_changes(
            &path,
            &[set(&["providers", "openai", "api_key"], "sk-live-key")],
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("Credential Manager"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HAND_EDITED);
        cleanup(&path);
    }

    #[test]
    fn blanking_an_api_key_is_allowed() {
        let text = "[providers.openai]\napi_key = \"\"\n";
        let out = patched(
            "blank-key",
            text,
            &[set(&["providers", "openai", "api_key"], "")],
        );
        assert_eq!(out, text);
    }

    // -- writes nothing when nothing changes -----------------------------------

    #[test]
    fn setting_a_value_to_what_it_already_is_does_not_touch_the_file() {
        let path = scratch("noop");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let before = crate::config::file_mtime(&path);
        write_changes(&path, &[FieldChange::mode(Mode::Auto)]).unwrap();
        assert_eq!(crate::config::file_mtime(&path), before);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HAND_EDITED);
        cleanup(&path);
    }

    // -- the atomic write: ACL failure, races ----------------------------------

    #[test]
    fn a_failed_acl_step_aborts_the_write_and_leaves_the_file_alone() {
        let path = scratch("acl");
        std::fs::write(&path, HAND_EDITED).unwrap();
        crate::config::FORCE_ACL_FAILURE_FOR_TEST.with(|f| f.set(true));
        let result = write_changes(&path, &[FieldChange::mode(Mode::Offline)]);
        crate::config::FORCE_ACL_FAILURE_FOR_TEST.with(|f| f.set(false));
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HAND_EDITED);
        assert!(!path.with_extension("toml.tmp").exists());
        cleanup(&path);
    }

    /// Replaces the file and moves its modified time `secs` into the future, so
    /// the change shows even on a coarse clock.
    fn edit_file(path: &Path, text: &str, secs: u64) {
        std::fs::write(path, text).unwrap();
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn an_edit_between_the_read_and_the_write_is_kept_and_the_change_applied_on_top() {
        let path = scratch("race-once");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let during = replaced(HAND_EDITED, "card_seconds = 12", "card_seconds = 77");
        let mut hook = |attempt: u32| {
            if attempt == 0 {
                edit_file(&path, &during, 5);
            }
        };
        write_changes_with(&path, &[FieldChange::mode(Mode::Offline)], &mut hook).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(
            out.contains("card_seconds = 77"),
            "the hand edit was lost:\n{out}"
        );
        assert!(out.contains(r#"mode = "offline""#), "{out}");
        cleanup(&path);
    }

    #[test]
    fn a_file_that_keeps_changing_fails_the_write_and_keeps_the_last_edit() {
        let path = scratch("race-twice");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let mut hook = |attempt: u32| {
            let text = replaced(
                HAND_EDITED,
                "card_seconds = 12",
                &format!("card_seconds = 7{attempt}"),
            );
            edit_file(&path, &text, 10 * (u64::from(attempt) + 1));
        };
        let err =
            write_changes_with(&path, &[FieldChange::mode(Mode::Offline)], &mut hook).unwrap_err();
        assert!(
            format!("{err:#}").contains("changed while Wingman was saving"),
            "{err:#}"
        );
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(
            out.contains("card_seconds = 71"),
            "the last edit must stand:\n{out}"
        );
        assert!(out.contains(r#"mode = "auto""#));
        cleanup(&path);
    }

    // -- diff and coalesce (pure) -----------------------------------------------

    #[test]
    fn diff_lists_only_the_fields_that_changed() {
        let before = Config::default();
        let mut after = before.clone();
        after.ui.card_seconds += 1;
        after.capture.redact = !after.capture.redact;
        after.mode = Mode::Offline;
        let changes = diff(&before, &after).unwrap();
        let mut paths: Vec<String> = changes.iter().map(|c| c.path().join(".")).collect();
        paths.sort();
        assert_eq!(paths, vec!["capture.redact", "mode", "ui.card_seconds"]);
    }

    #[test]
    fn diff_of_equal_configs_is_empty() {
        assert!(diff(&Config::default(), &Config::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn diff_never_carries_an_api_key() {
        let before = Config::default();
        let mut after = before.clone();
        after.providers.openai.api_key = "sk-typed-in-settings".into();
        after.providers.gemini.api_key = "g-typed".into();
        assert!(diff(&before, &after).unwrap().is_empty());
    }

    #[test]
    fn diff_treats_an_array_as_one_value() {
        let before = Config::default();
        let mut after = before.clone();
        after.providers.order = vec!["ollama".into()];
        let changes = diff(&before, &after).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), ["providers", "order"]);
    }

    #[test]
    fn diff_removes_an_optional_table_that_was_cleared_and_sets_one_that_appeared() {
        let mut before = Config::default();
        before.hotkeys.pause = Some(chord(0x13, false));
        let mut after = before.clone();
        after.hotkeys.pause = None;
        let removed = diff(&before, &after).unwrap();
        assert_eq!(
            removed,
            vec![FieldChange::Remove {
                path: vec!["hotkeys".into(), "pause".into()]
            }]
        );
        let added = diff(&after, &before).unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].path(), ["hotkeys", "pause"]);
    }

    #[test]
    fn applying_a_diff_to_the_file_reproduces_the_new_config() {
        let before: Config = toml::from_str(HAND_EDITED).unwrap();
        let mut after = before.clone();
        after.ui.card_seconds = 3;
        after.mode = Mode::Cloud;
        after.hotkeys.pause = Some(chord(0x13, true));
        after.providers.order = vec!["anthropic".into()];
        let out = patched("roundtrip", HAND_EDITED, &diff(&before, &after).unwrap());
        let loaded: Config = toml::from_str(&out).unwrap();
        assert_eq!(loaded.ui.card_seconds, 3);
        assert_eq!(loaded.mode, Mode::Cloud);
        assert_eq!(loaded.hotkeys.pause, after.hotkeys.pause);
        assert_eq!(loaded.providers.order, vec!["anthropic"]);
        assert!(out.contains("# My Wingman settings."));
        assert!(out.contains("favourite_colour"));
    }

    #[test]
    fn coalesce_keeps_the_latest_change_per_key_in_order() {
        let a1 = set(&["ui", "card_seconds"], 1u32);
        let m = FieldChange::mode(Mode::Cloud);
        let a2 = set(&["ui", "card_seconds"], 2u32);
        let m2 = FieldChange::mode(Mode::Local);
        let other = set(&["capture", "redact"], true);
        let out = coalesce(vec![a1, m, a2.clone(), other.clone(), m2.clone()]);
        assert_eq!(out, vec![a2, other, m2]);
    }

    // -- the writer thread ------------------------------------------------------

    fn replies(writer: &ConfigWriter, jobs: Vec<(&str, Vec<FieldChange>)>) -> Vec<WriteReply> {
        let (tx, rx) = mpsc::channel();
        let n = jobs.len();
        for (label, changes) in jobs {
            let tx = tx.clone();
            writer.submit(label, changes, move |r| {
                let _ = tx.send(r);
            });
        }
        (0..n)
            .map(|_| {
                rx.recv_timeout(std::time::Duration::from_secs(30))
                    .expect("reply")
            })
            .collect()
    }

    #[test]
    fn the_writer_thread_writes_and_reports_back() {
        let path = scratch("thread");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let writer = ConfigWriter::spawn(path.clone());
        let got = replies(
            &writer,
            vec![("Mode", vec![FieldChange::mode(Mode::Offline)])],
        );
        assert_eq!(got[0].label, "Mode");
        assert_eq!(got[0].result, Ok(()));
        let out = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            out,
            replaced(HAND_EDITED, r#"mode = "auto""#, r#"mode = "offline""#)
        );
        cleanup(&path);
    }

    #[test]
    fn the_writer_thread_reports_a_refusal_as_a_plain_sentence() {
        let path = scratch("thread-broken");
        std::fs::write(&path, "mode = \n").unwrap();
        let writer = ConfigWriter::spawn(path.clone());
        let got = replies(
            &writer,
            vec![("Mode", vec![FieldChange::mode(Mode::Offline)])],
        );
        let message = got[0].result.clone().unwrap_err();
        assert!(message.contains("did not change it"), "{message}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mode = \n");
        cleanup(&path);
    }

    #[test]
    fn rapid_changes_to_different_fields_are_all_written_and_all_reported() {
        let path = scratch("thread-rapid");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let writer = ConfigWriter::spawn(path.clone());
        let jobs: Vec<(&str, Vec<FieldChange>)> = vec![
            ("one", vec![set(&["ui", "card_seconds"], 1u32)]),
            ("two", vec![set(&["capture", "redact"], true)]),
            ("three", vec![set(&["ui", "card_seconds"], 3u32)]),
        ];
        let got = replies(&writer, jobs);
        assert!(got.iter().all(|r| r.result.is_ok()));
        let loaded: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.ui.card_seconds, 3, "the latest value of a key wins");
        assert!(loaded.capture.redact);
        cleanup(&path);
    }

    #[test]
    fn a_writer_whose_thread_is_gone_still_calls_back_with_an_error() {
        let (tx, rx) = mpsc::channel::<Job>();
        drop(rx);
        let writer = ConfigWriter { jobs: tx };
        let got = replies(&writer, vec![("x", vec![FieldChange::mode(Mode::Auto)])]);
        assert!(got[0].result.is_err());
    }

    // -- review fixes (#889): nested keys, mtime order, BOM, floats --------------

    fn toml_value(text: &str) -> toml::Value {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_whole_table_change_carrying_a_nested_api_key_is_refused() {
        let with_key =
            toml_value("order = [\"a\"]\n[openai]\nmodel = \"m\"\napi_key = \"sk-live\"\n");
        let nested_array = toml_value(
            "[[compat]]\nname = \"x\"\nbase_url = \"http://127.0.0.1:1\"\napi_key = \"sk-live\"\n",
        );
        let cases = [
            FieldChange::Set {
                path: vec!["providers".into()],
                value: with_key,
            },
            FieldChange::Set {
                path: vec!["providers".into(), "compat".into()],
                value: nested_array["compat"].clone(),
            },
        ];
        for change in cases {
            let path = scratch("nested-key");
            std::fs::write(&path, HAND_EDITED).unwrap();
            let err = write_changes(&path, &[change]).unwrap_err();
            assert!(format!("{err:#}").contains("Credential Manager"), "{err:#}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), HAND_EDITED);
            cleanup(&path);
        }
    }

    #[test]
    fn a_whole_table_change_with_only_blank_api_keys_is_allowed() {
        let blank = toml_value(
            "[[compat]]\nname = \"x\"\nbase_url = \"http://127.0.0.1:1\"\napi_key = \"\"\n",
        );
        let out = patched(
            "blank-nested",
            HAND_EDITED,
            &[FieldChange::Set {
                path: vec!["providers".into(), "compat".into()],
                value: blank["compat"].clone(),
            }],
        );
        assert!(out.contains("name = \"x\""), "{out}");
    }

    fn compat(name: &str, key: &str) -> crate::config::CompatConfig {
        crate::config::CompatConfig {
            name: name.into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: key.into(),
            ..Default::default()
        }
    }

    #[test]
    fn diff_blanks_the_api_key_inside_a_whole_array_it_sets() {
        let before = Config::default();
        let mut after = before.clone();
        after.providers.compat = vec![compat("mine", "sk-hydrated-secret")];
        let changes = diff(&before, &after).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), ["providers", "compat"]);
        let out = patched("diff-compat", HAND_EDITED, &changes);
        assert!(out.contains("mine"), "{out}");
        assert!(!out.contains("sk-hydrated-secret"), "{out}");
    }

    #[test]
    fn diff_ignores_a_compat_entry_whose_only_difference_is_its_hydrated_key() {
        let mut before = Config::default();
        before.providers.compat = vec![compat("mine", "")];
        let mut after = before.clone();
        after.providers.compat[0].api_key = "sk-hydrated-secret".into();
        assert!(diff(&before, &after).unwrap().is_empty());
    }

    #[test]
    fn a_write_does_not_mark_the_file_as_seen_so_a_whole_file_save_cannot_clobber_it() {
        let path = scratch("seen");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        drop(f);
        let stale = Config::load_from(&path).unwrap();
        write_changes(&path, &[FieldChange::mode(Mode::Offline)]).unwrap();
        // The file now holds something the in-memory config has not read. Until a
        // reload reads it, a whole-file save must be refused, not overwrite it.
        let err = stale.save_to(&path).unwrap_err();
        assert!(format!("{err:#}").contains("changed on disk"), "{err:#}");
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains(r#"mode = "offline""#));
        cleanup(&path);
    }

    #[test]
    fn a_file_with_a_byte_order_mark_is_patched_and_keeps_its_mark() {
        let text = format!("\u{feff}{HAND_EDITED}");
        let path = scratch("bom");
        std::fs::write(&path, &text).unwrap();
        write_changes(&path, &[FieldChange::mode(Mode::Offline)]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]), "the BOM was lost");
        let out = String::from_utf8(bytes).unwrap();
        assert_eq!(
            out,
            replaced(&text, r#"mode = "auto""#, r#"mode = "offline""#)
        );
        cleanup(&path);
    }

    #[test]
    fn a_single_precision_float_is_written_as_the_user_would_type_it() {
        let out = patched("f32", HAND_EDITED, &[set(&["ui", "text_scale"], 1.1f32)]);
        assert!(out.contains("text_scale = 1.1\n"), "{out}");
        let loaded: Config = toml::from_str(&out).unwrap();
        assert_eq!(loaded.ui.text_scale, 1.1f32);
    }

    // -- review fixes (#889): batches, quit, path error, reapply ----------------

    #[test]
    fn a_refused_job_does_not_fail_the_jobs_batched_with_it() {
        let path = scratch("batch-refused");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let good = vec![FieldChange::mode(Mode::Offline)];
        let bad = vec![set(&["providers", "openai", "api_key"], "sk-live-key")];
        let other = vec![set(&["ui", "card_seconds"], 4u32)];
        let results = write_batch(&path, &[&good, &bad, &other]);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0], Ok(()));
        assert!(results[1]
            .clone()
            .unwrap_err()
            .contains("Credential Manager"));
        assert_eq!(results[2], Ok(()));
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains(r#"mode = "offline""#), "{out}");
        assert!(out.contains("card_seconds = 4"), "{out}");
        assert!(!out.contains("sk-live-key"), "{out}");
        cleanup(&path);
    }

    #[test]
    fn a_batch_against_a_broken_file_fails_every_job_and_leaves_the_file() {
        let path = scratch("batch-broken");
        std::fs::write(&path, "mode = \n").unwrap();
        let a = vec![FieldChange::mode(Mode::Offline)];
        let b = vec![set(&["ui", "card_seconds"], 4u32)];
        let results = write_batch(&path, &[&a, &b]);
        assert!(results.iter().all(|r| r.is_err()), "{results:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mode = \n");
        cleanup(&path);
    }

    #[test]
    fn flush_returns_only_after_every_queued_write_is_done() {
        let path = scratch("flush");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let writer = ConfigWriter::spawn(path.clone());
        let (tx, rx) = mpsc::channel();
        for n in 1..=3u32 {
            let tx = tx.clone();
            writer.submit("n", vec![set(&["ui", "card_seconds"], n)], move |r| {
                let _ = tx.send(r);
            });
        }
        assert!(writer.flush(std::time::Duration::from_secs(30)));
        assert_eq!(rx.try_iter().count(), 3, "every job must have been told");
        let loaded: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.ui.card_seconds, 3);
        cleanup(&path);
    }

    #[test]
    fn flush_gives_up_after_its_time_instead_of_hanging() {
        let path = scratch("flush-slow");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let writer = ConfigWriter::spawn(path.clone());
        writer.submit("slow", vec![FieldChange::mode(Mode::Offline)], |_| {
            std::thread::sleep(std::time::Duration::from_millis(1500));
        });
        let start = std::time::Instant::now();
        assert!(!writer.flush(std::time::Duration::from_millis(100)));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        cleanup(&path);
    }

    #[test]
    fn flush_with_nothing_queued_is_immediate() {
        let path = scratch("flush-idle");
        let writer = ConfigWriter::spawn(path.clone());
        assert!(writer.flush(std::time::Duration::from_secs(30)));
        cleanup(&path);
    }

    #[test]
    fn a_path_error_is_not_remembered_forever() {
        let cell = std::sync::OnceLock::new();
        let first = global_in(&cell, || Err(anyhow::anyhow!("no APPDATA yet")));
        assert!(first.is_err());
        let path = scratch("global-retry");
        let second = global_in(&cell, || Ok(path.clone()));
        assert!(second.is_ok(), "the second call must try again");
        cleanup(&path);
    }

    #[test]
    fn reapply_keeps_the_keys_left_in_the_file() {
        // #899: a load-time note, like `unreadable_secrets`, must survive.
        let loaded = Config {
            keys_left_in_file: vec!["openai".into()],
            ..Default::default()
        };
        let out = reapply(&loaded, &[FieldChange::mode(Mode::Offline)]).unwrap();
        assert_eq!(out.keys_left_in_file, vec!["openai".to_string()]);
    }

    #[test]
    fn reapply_puts_unsaved_changes_back_on_a_freshly_loaded_config() {
        let mut loaded = Config::default();
        loaded.providers.openai.api_key = "sk-hydrated".into();
        loaded.unreadable_secrets = vec!["anthropic".into()];
        let out = reapply(&loaded, &[FieldChange::mode(Mode::Offline)]).unwrap();
        assert_eq!(out.mode, Mode::Offline);
        assert_eq!(out.providers.openai.api_key, "sk-hydrated");
        assert_eq!(out.unreadable_secrets, vec!["anthropic".to_string()]);
    }

    /// The reload after a write (`Config::load`: Credential Manager reads, maybe
    /// `icacls`) runs on the writer thread, not the thread that hosts the
    /// keyboard hook; the handler only swaps the result in.
    #[test]
    fn the_reload_after_a_write_runs_off_the_ui_thread() {
        let app = include_str!("app.rs").replace('\r', "");
        let start = app
            .find("    fn on_config_written(")
            .expect("on_config_written moved: this scan is no longer looking at anything");
        let rest = &app[start..];
        let end = rest.find("\n    }\n").expect("end of on_config_written");
        let handler = &rest[..end];
        assert!(!handler.contains("Config::load"), "{handler}");
        assert!(!handler.contains("load_after_write"), "{handler}");
        let start = app
            .find("    fn persist_changes_with(")
            .expect("persist_changes_with moved");
        let rest = &app[start..];
        let end = rest.find("\n    }\n").expect("end of persist_changes_with");
        assert!(
            rest[..end].contains("load_after_write"),
            "the writer thread must reload"
        );
    }

    /// Quit must not lose a write still queued: `WM_DESTROY` waits (bounded) for
    /// the writer before the process exits.
    #[test]
    fn quitting_flushes_the_writer() {
        let app = include_str!("app.rs").replace('\r', "");
        let start = app
            .find("        WM_DESTROY => {")
            .expect("the WM_DESTROY arm moved: this scan is no longer looking at anything");
        let rest = &app[start..];
        let end = rest
            .find("\n        }\n")
            .expect("end of the WM_DESTROY arm");
        assert!(
            rest[..end].contains("config_writer::flush_global("),
            "{}",
            &rest[..end]
        );
    }

    // -- re-review fixes (#889) -------------------------------------------------

    #[test]
    fn replacing_an_array_of_tables_keeps_its_comment_and_its_place() {
        let text = "# endpoints I use\n[[providers.compat]]\nname = \"a\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\n# second one\n[[providers.compat]]\nname = \"a2\"\nbase_url = \"http://127.0.0.1:2/v1\"\n\n[ui]\ncard_seconds = 7 # keep\n\n[unknown_future_table]\nx = 1\n";
        let compat: Vec<toml::Value> = ["b", "b2", "b3"]
            .iter()
            .map(|n| {
                toml::Value::try_from(crate::config::CompatConfig {
                    name: (*n).into(),
                    base_url: "http://127.0.0.1:1/v1".into(),
                    ..Default::default()
                })
                .unwrap()
            })
            .collect();
        let out = patched(
            "aot-place",
            text,
            &[FieldChange::Set {
                path: vec!["providers".into(), "compat".into()],
                value: toml::Value::Array(compat),
            }],
        );
        let at = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} lost:\n{out}"))
        };
        assert!(
            at("# endpoints I use") < at("name = \"b\""),
            "the comment must stay above the first table:\n{out}"
        );
        assert!(at("# second one") < at("name = \"b2\""), "{out}");
        assert!(
            at("name = \"b3\"") < at("[ui]"),
            "the tables must stay where they were, before [ui]:\n{out}"
        );
        assert!(at("[ui]") < at("[unknown_future_table]"), "{out}");
    }

    #[test]
    fn replacing_a_table_keeps_its_comment_and_its_place() {
        let text =
            "# my cheap model\n[providers.openai]\nmodel = \"a\"\n\n[ui]\ncard_seconds = 7\n";
        let value = toml_value("model = \"b\"\n");
        let out = patched(
            "table-place",
            text,
            &[FieldChange::Set {
                path: vec!["providers".into(), "openai".into()],
                value,
            }],
        );
        assert!(
            out.starts_with("# my cheap model\n[providers.openai]"),
            "{out}"
        );
        assert!(
            out.find("model = \"b\"").unwrap() < out.find("[ui]").unwrap(),
            "{out}"
        );
    }

    #[test]
    fn two_threads_writing_the_file_do_not_trip_over_the_shared_temp_file() {
        let path = scratch("lock");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let handles: Vec<_> = (0..2)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for n in 0..15 {
                        let text = format!("# writer {t} pass {n}\n{HAND_EDITED}");
                        crate::config::write_file_atomically(&path, &text, false)
                            .unwrap_or_else(|e| panic!("writer {t} pass {n}: {e:#}"));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(toml::from_str::<Config>(&std::fs::read_to_string(&path).unwrap()).is_ok());
        cleanup(&path);
    }

    #[test]
    fn a_load_on_the_writer_thread_leaves_the_file_unseen_until_the_ui_adopts_it() {
        let path = scratch("deferred");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        let set_mtime = |t: std::time::SystemTime| {
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_modified(t).unwrap();
        };
        set_mtime(past);
        let held = Config::load_from(&path).unwrap();
        // The file changes, then the writer thread loads it.
        std::fs::write(
            &path,
            replaced(HAND_EDITED, "card_seconds = 12", "card_seconds = 5"),
        )
        .unwrap();
        let (loaded, mtime) =
            crate::config::load_deferring_mtime(&path, || Config::load_from(&path).unwrap());
        assert_eq!(loaded.ui.card_seconds, 5);
        assert!(mtime.is_some());
        // Until the UI thread swaps `loaded` in, `held` is stale: its save must
        // still be refused.
        let err = held.save_to(&path).unwrap_err();
        assert!(format!("{err:#}").contains("changed on disk"), "{err:#}");
        // The swap adopts the mtime the load saw; now a save is allowed.
        crate::config::note_mtime(&path, mtime);
        assert!(loaded.save_to(&path).is_ok());
        cleanup(&path);
    }

    fn body_of<'a>(src: &'a str, signature: &str) -> &'a str {
        let start = src.find(signature).unwrap_or_else(|| {
            panic!("{signature} moved: this scan is no longer looking at anything")
        });
        let rest = &src[start..];
        let end = rest.find("\n    }\n").expect("end of the function");
        &rest[..end]
    }

    #[test]
    fn tray_changes_reach_the_file_through_the_writer_not_a_whole_file_save() {
        let app = include_str!("app.rs").replace('\r', "");
        for signature in [
            "    fn set_provider(",
            "    fn commit_model_pick(",
            "    fn on_learned(",
        ] {
            let body = body_of(&app, signature);
            assert!(!body.contains("config.save("), "{signature}\n{body}");
            assert!(body.contains("persist_diff("), "{signature}\n{body}");
        }
        let at = app
            .find("adopt_local_model(model, &base_url)")
            .expect("the first-run adoption moved");
        let window = &app[at..(at + 900).min(app.len())];
        assert!(!window.contains("config.save("), "{window}");
        assert!(window.contains("persist_diff("), "{window}");
    }

    #[test]
    fn the_handler_swaps_only_when_no_other_write_is_in_flight() {
        let app = include_str!("app.rs").replace('\r', "");
        let body = body_of(&app, "    fn on_config_written(");
        assert!(body.contains("finish("), "{body}");
    }

    /// The low-level keyboard hook must be gone before Quit waits on the writer,
    /// or every key in the system stalls behind a wait on the hook's thread.
    #[test]
    fn quitting_unhooks_the_keyboard_before_it_waits_for_the_writer() {
        let app = include_str!("app.rs").replace('\r', "");
        let start = app
            .find("        WM_DESTROY => {")
            .expect("the WM_DESTROY arm moved");
        let rest = &app[start..];
        let end = rest
            .find("\n        }\n")
            .expect("end of the WM_DESTROY arm");
        let arm = &rest[..end];
        let unhook = arm
            .find("hook = None")
            .expect("the hook must be dropped in WM_DESTROY");
        let flush = arm.find("flush_global(").expect("flush");
        assert!(unhook < flush, "{arm}");
    }

    // -- round 3 review fixes (#889) --------------------------------------------

    #[test]
    fn a_pending_count_that_lost_its_reply_still_settles() {
        use std::sync::atomic::AtomicU32;
        let pending = AtomicU32::new(0);
        pending.fetch_add(2, Ordering::SeqCst);
        // The first write's reply could not be posted (window gone): the poster
        // settles it itself, and it must not count as the last.
        assert!(!finish(&pending));
        // The second reply then is the last.
        assert!(finish(&pending));
        // A stray extra never underflows.
        assert!(finish(&pending));
        assert_eq!(pending.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_failed_last_reply_still_adopts_the_newest_good_load() {
        let t = |s: u64| Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
        // Not the last: remember it, adopt nothing yet.
        assert_eq!(track_mtime(false, Some(t(1)), None), (None, Some(t(1))));
        assert_eq!(
            track_mtime(false, Some(t(2)), Some(t(1))),
            (None, Some(t(2)))
        );
        assert_eq!(track_mtime(false, None, Some(t(1))), (None, Some(t(1))));
        // Last and good: adopt its own, forget the rest.
        assert_eq!(
            track_mtime(true, Some(t(3)), Some(t(1))),
            (Some(t(3)), None)
        );
        // Last but failed: adopt the newest earlier good one.
        assert_eq!(track_mtime(true, None, Some(t(2))), (Some(t(2)), None));
        // Nothing good at all.
        assert_eq!(track_mtime(true, None, None), (None, None));
    }

    #[test]
    fn the_changed_since_read_check_happens_while_the_write_lock_is_held() {
        let path = scratch("lock-check");
        std::fs::write(&path, HAND_EDITED).unwrap();
        let guard = crate::config::lock_writes();
        let worker = {
            let path = path.clone();
            std::thread::spawn(move || write_changes(&path, &[FieldChange::mode(Mode::Offline)]))
        };
        // Let the worker read the file and reach the lock.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let during = replaced(HAND_EDITED, "card_seconds = 12", "card_seconds = 77");
        edit_file(&path, &during, 5);
        drop(guard);
        worker.join().unwrap().unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(
            out.contains("card_seconds = 77"),
            "the hand edit was lost:\n{out}"
        );
        assert!(out.contains(r#"mode = "offline""#), "{out}");
        cleanup(&path);
    }

    #[test]
    fn an_empty_list_replacing_a_list_of_tables_keeps_the_comment() {
        let text = "[providers]\norder = [\"openai\"]\n\n# my endpoints\n[[providers.compat]]\nname = \"a\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\n[ui]\ncard_seconds = 7 # keep\n";
        let out = patched(
            "aot-empty",
            text,
            &[FieldChange::Set {
                path: vec!["providers".into(), "compat".into()],
                value: toml::Value::Array(Vec::new()),
            }],
        );
        assert!(out.contains("# my endpoints"), "{out}");
        assert!(
            out.find("# my endpoints").unwrap() < out.find("compat = []").unwrap(),
            "{out}"
        );
        let loaded: Config = toml::from_str(&out).unwrap();
        assert!(loaded.providers.compat.is_empty());
        assert!(out.contains("card_seconds = 7 # keep"), "{out}");
    }

    #[test]
    fn a_reload_forgets_the_unsaved_changes() {
        let app = include_str!("app.rs").replace('\r', "");
        // #901: the reload's answer is applied by the write handler, which
        // clears them when the job is a reload.
        let handler = body_of(&app, "    fn on_config_written(");
        let at = handler
            .find("outcome.from_reload")
            .expect("no reload branch");
        assert!(handler[at..].contains("unsaved.clear()"), "{handler}");
    }

    /// Settings saves through the writer: its Save builds the changes with
    /// `diff` and `guard_whole_values`, posts them to the main window, and never
    /// calls `Config::save` or touches the disk itself.
    #[test]
    fn the_settings_window_saves_through_the_writer_thread() {
        let src = include_str!("ui/settings.rs").replace('\r', "");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        assert!(!code.is_empty());
        assert!(
            code.contains("crate::config_writer::diff("),
            "Save must send only the changed fields"
        );
        assert!(
            code.contains("crate::config_writer::guard_whole_values("),
            "a changed list must not replace a hand edit of the same list"
        );
        assert!(
            code.contains("crate::config_writer::file_baseline("),
            "the guard must compare with the file as read when the window opened"
        );
        assert!(code.contains("crate::app::post_settings_save("));
        assert!(
            !code.contains(".save("),
            "Settings must not save the whole config itself"
        );
        assert!(
            !code.contains("fs::write"),
            "no file writes on the UI thread"
        );
    }

    // -- whole values are written only over what the window saw (#871) -----------

    fn order(items: &[&str]) -> toml::Value {
        toml::Value::Array(items.iter().map(|s| toml::Value::from(*s)).collect())
    }

    fn change_order(text: &str, expected: &[&str], to: &[&str]) -> (Result<()>, String, PathBuf) {
        let path = scratch("guard");
        std::fs::write(&path, text).unwrap();
        let change = FieldChange::SetIfUnchanged {
            path: vec!["providers".into(), "order".into()],
            expected: Some(order(expected)),
            value: order(to),
        };
        let result = write_changes(&path, &[change]);
        let after = std::fs::read_to_string(&path).unwrap();
        (result, after, path)
    }

    #[test]
    fn a_list_is_written_when_the_file_still_holds_what_the_window_saw() {
        let text = "[providers]\norder = [\"openai\", \"anthropic\"]  # mine\n";
        let (result, after, path) = change_order(text, &["openai", "anthropic"], &["anthropic"]);
        result.unwrap();
        assert!(after.contains("order = [\"anthropic\"]"), "{after}");
        assert!(after.contains("# mine"), "{after}");
        cleanup(&path);
    }

    #[test]
    fn a_list_the_user_added_to_by_hand_meanwhile_is_not_overwritten() {
        // The window opened with two vendors; the user then added a third in the
        // file. Saving the window's list would silently drop it.
        let text = "[providers]\norder = [\"openai\", \"anthropic\", \"gemini\"]\n";
        let (result, after, path) = change_order(text, &["openai", "anthropic"], &["anthropic"]);
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("providers.order"), "{message}");
        assert!(message.contains("Open Settings again"), "{message}");
        assert!(!message.contains('\u{2014}'), "{message}");
        assert_eq!(after, text, "the file must be left exactly as it was");
        cleanup(&path);
    }

    #[test]
    fn a_list_the_file_never_spelled_out_compares_as_its_default() {
        // No `order` in the file: Config reads the default list, which is what
        // the window opened with.
        let default = Config::default().providers.order;
        let names: Vec<&str> = default.iter().map(String::as_str).collect();
        let path = scratch("guard-absent");
        std::fs::write(&path, "mode = \"auto\"\n").unwrap();
        let change = FieldChange::SetIfUnchanged {
            path: vec!["providers".into(), "order".into()],
            expected: Some(order(&names)),
            value: order(&["gemini"]),
        };
        write_changes(&path, &[change]).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("order = [\"gemini\"]"), "{after}");
        assert!(after.contains("mode = \"auto\""), "{after}");
        cleanup(&path);
    }

    /// The running config has a list the file never spelled out (a tray change
    /// not yet written): guarding against the running config refuses a save that
    /// touched nothing in the file; guarding against the file as read does not.
    #[test]
    fn a_pending_tray_change_does_not_make_an_untouched_list_look_hand_edited() {
        let path = scratch("guard-baseline");
        let text = "mode = \"auto\"\n";
        std::fs::write(&path, text).unwrap();
        let mut running = Config::default();
        running.providers.order = vec!["ollama".into()];
        let mut edited = running.clone();
        edited.providers.order = vec!["gemini".into()];
        let changes = diff(&running, &edited).unwrap();

        let wrong = guard_whole_values(changes.clone(), &running).unwrap();
        assert!(write_changes(&path, &wrong).is_err(), "the old baseline");

        let baseline = file_baseline(&path).expect("the file reads");
        assert_eq!(baseline.providers.order, Config::default().providers.order);
        let right = guard_whole_values(changes, &baseline).unwrap();
        write_changes(&path, &right).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("order = [\"gemini\"]"), "{after}");
        cleanup(&path);
    }

    #[test]
    fn a_missing_file_is_all_defaults_and_an_unreadable_one_has_no_baseline() {
        let missing = scratch("guard-missing");
        cleanup(&missing);
        let baseline = file_baseline(&missing).expect("a missing file is defaults");
        assert_eq!(baseline.providers.order, Config::default().providers.order);
        // Bytes that are not text cannot be read as a config.
        let junk = scratch("guard-junk");
        std::fs::write(&junk, [0xffu8, 0xfe, 0xfd]).unwrap();
        assert!(file_baseline(&junk).is_none());
        cleanup(&junk);
    }

    #[test]
    fn a_guarded_change_leaves_the_other_jobs_in_its_batch_alone() {
        let path = scratch("guard-batch");
        let text = "[providers]\norder = [\"openai\", \"gemini\"]\n";
        std::fs::write(&path, text).unwrap();
        let guarded = vec![FieldChange::SetIfUnchanged {
            path: vec!["providers".into(), "order".into()],
            expected: Some(order(&["openai"])),
            value: order(&["anthropic"]),
        }];
        let fine = vec![set(&["ui", "card_seconds"], 9u32)];
        let results = write_batch(&path, &[&guarded, &fine]);
        assert!(results[0].is_err());
        assert_eq!(results[1], Ok(()));
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("card_seconds = 9"), "{after}");
        assert!(after.contains("\"gemini\""), "{after}");
        cleanup(&path);
    }

    #[test]
    fn guard_whole_values_wraps_lists_and_tables_and_leaves_leaves_alone() {
        let before = Config::default();
        let mut after = before.clone();
        after.providers.order = vec!["gemini".into()];
        after.ui.card_seconds += 1;
        let changes = diff(&before, &after).unwrap();
        let guarded = guard_whole_values(changes, &before).unwrap();
        let order = guarded
            .iter()
            .find(|c| c.path() == ["providers", "order"])
            .expect("the list change is there");
        match order {
            FieldChange::SetIfUnchanged { expected, .. } => {
                let expected = expected.as_ref().expect("the default list was set");
                assert_eq!(
                    expected.as_array().unwrap().len(),
                    before.providers.order.len()
                );
            }
            other => panic!("a list must be guarded: {other:?}"),
        }
        let leaf = guarded
            .iter()
            .find(|c| c.path() == ["ui", "card_seconds"])
            .expect("the leaf change is there");
        assert!(matches!(leaf, FieldChange::Set { .. }), "{leaf:?}");
    }

    #[test]
    fn a_guarded_change_that_could_not_be_checked_is_refused() {
        // `effective_on_disk` is None only when the comparison cannot be made;
        // write_changes_with then refuses rather than writing blind.
        let src = include_str!("config_writer.rs").replace('\r', "");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        assert!(
            code.contains("could not check config.toml before changing"),
            "the fail-closed arm moved"
        );
    }

    #[test]
    fn a_settings_outcome_keeps_a_failed_save_out_of_unsaved_and_says_nothing_was_applied() {
        let o = Outcome::settings_save();
        assert!(o.from_settings);
        assert!(!Outcome::simple("x").from_settings);
        assert!(!Outcome::simple("x").quiet_ok);
        let quiet = Outcome::quiet("x");
        assert!(quiet.quiet_ok && !quiet.from_settings);
        assert_eq!(quiet.fail_headline, Outcome::simple("x").fail_headline);
        assert!(o.fail_note.contains("not applied"), "{}", o.fail_note);
        assert!(o.fail_note.contains("key"), "{}", o.fail_note);
        assert!(!o.fail_note.contains('\u{2014}') && !o.ok_headline.contains('\u{2014}'));
    }

    #[test]
    fn a_guarded_change_is_reapplied_like_a_plain_one() {
        let config = Config::default();
        let change = FieldChange::SetIfUnchanged {
            path: vec!["providers".into(), "order".into()],
            expected: None,
            value: order(&["gemini"]),
        };
        let out = reapply(&config, &[change]).unwrap();
        assert_eq!(out.providers.order, vec!["gemini".to_string()]);
    }

    #[test]
    fn a_reply_that_cannot_be_posted_settles_its_own_count() {
        let app = include_str!("app.rs").replace('\r', "");
        let body = body_of(&app, "    fn persist_changes_with(");
        assert!(body.contains("finish("), "{body}");
    }

    // -- guards ----------------------------------------------------------------

    /// The tray's Mode change reaches the file only through the writer thread:
    /// no whole-config save, and so no file or `icacls` work on the UI thread
    /// that hosts the keyboard hook.
    #[test]
    fn the_tray_mode_change_goes_through_the_writer_thread() {
        let app = include_str!("app.rs").replace('\r', "");
        let start = app
            .find("    fn set_mode_with(&mut self, mode: Mode, quiet: bool) {")
            .expect("set_mode_with moved: this scan is no longer looking at anything");
        let rest = &app[start..];
        let end = rest.find("\n    }\n").expect("end of set_mode");
        let body = &rest[..end];
        assert!(
            body.contains("config_writer::"),
            "set_mode must use the writer:\n{body}"
        );
        assert!(
            !body.contains("config.save("),
            "set_mode must not re-serialise the whole config on the UI thread:\n{body}"
        );
    }
}
