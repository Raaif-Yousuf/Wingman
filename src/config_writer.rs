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
            FieldChange::Set { path, .. } | FieldChange::Remove { path } => path,
        }
    }

    /// Patches `doc` and nothing else. Setting a key that is not in the file
    /// adds it (and any missing table above it); a table written inline
    /// (`primary = { vk = 134 }`) is patched in place. The old value's trailing
    /// comment stays with the new one.
    pub fn apply(&self, doc: &mut DocumentMut) -> Result<()> {
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
        let create = matches!(self, FieldChange::Set { .. });
        let mut table: &mut dyn TableLike = doc.as_table_mut();
        for name in parents {
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
                        *slot = item;
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
/// (`unreadable_secrets`, `load_problem`) carry over.
pub fn reapply(config: &Config, changes: &[FieldChange]) -> Result<Config> {
    let mut value = toml::Value::try_from(config).context("could not read the settings")?;
    for change in changes {
        change_value(&mut value, change)?;
    }
    let mut out: Config = value
        .try_into()
        .context("could not apply the unsaved change")?;
    out.unreadable_secrets = config.unreadable_secrets.clone();
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
        FieldChange::Set { value, .. } => {
            table.insert(key.clone(), value.clone());
        }
        FieldChange::Remove { .. } => {
            table.remove(key);
        }
    }
    Ok(())
}

/// Reads `path` fresh, applies `changes`, writes atomically. See the module doc
/// for the rules.
pub fn write_changes(path: &Path, changes: &[FieldChange]) -> Result<()> {
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
        for change in changes {
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
        if crate::config::file_mtime(path) != seen {
            continue;
        }
        if out == text {
            return Ok(());
        }
        return crate::config::write_file_atomically(path, &out, false);
    }
    bail!("config.toml changed while Wingman was saving it, so nothing was written. Try again")
}

fn refusal(text: &str, span: Option<std::ops::Range<usize>>) -> String {
    format!(
        "{}, so Wingman did not change it. Fix the file, then choose Reload settings from the tray menu",
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
}

/// After one write's reply: the writes still in flight, and whether this was the
/// last. Only the last reply may swap in a loaded config; an earlier one was read
/// before a later choice was made and would overwrite it.
pub fn settle(pending: u32) -> (u32, bool) {
    let left = pending.saturating_sub(1);
    (left, left == 0)
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
            .find("    fn persist_changes(")
            .expect("persist_changes moved");
        let rest = &app[start..];
        let end = rest.find("\n    }\n").expect("end of persist_changes");
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

    #[test]
    fn only_the_last_reply_in_a_run_of_writes_swaps_the_config() {
        // Mode toggled twice quickly: the first reply's file read predates the
        // second choice and must not overwrite it.
        let (pending, last) = settle(2);
        assert_eq!((pending, last), (1, false));
        let (pending, last) = settle(pending);
        assert_eq!((pending, last), (0, true));
        // A stray extra reply never underflows.
        assert_eq!(settle(0), (0, true));
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
        assert!(body.contains("settle("), "{body}");
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

    // -- guards ----------------------------------------------------------------

    /// The tray's Mode change reaches the file only through the writer thread:
    /// no whole-config save, and so no file or `icacls` work on the UI thread
    /// that hosts the keyboard hook.
    #[test]
    fn the_tray_mode_change_goes_through_the_writer_thread() {
        let app = include_str!("app.rs").replace('\r', "");
        let start = app
            .find("    fn set_mode(&mut self, mode: Mode) {")
            .expect("set_mode moved: this scan is no longer looking at anything");
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
