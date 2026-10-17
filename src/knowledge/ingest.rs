//! The start-up scan of the knowledge folder (spec: "Reconciliation").
//!
//! One short-lived background thread, started only when a folder is
//! configured, walks the folder once, indexes the files that changed, and ends.
//! Unchanged files cost a directory entry (same size and modified time: not
//! even read). It is not a timer and not a watcher: nothing runs after it
//! finishes (AGENTS.md rule 5), so changes made while Wingman is running are
//! picked up at the next start. Making no network request of any kind, it needs
//! no Pause or Offline check.
//!
//! File bytes, names and sizes are untrusted: every read is capped, nothing is
//! indexed or sliced without a bounds check, and a failure is a row with plain
//! words, never a panic (release is `panic = "abort"`).

use super::chunk::chunk_text;
use super::hash::sha256_hex;
use super::mask::mask_sensitive;
use super::notice;
use super::parse::{parse, parser_for, Parser, MAX_FILE_BYTES, PARSER_VERSION};
use crate::store::chunks::IndexedChunk;
use crate::store::knowledge::{DocumentRow, DocumentUpdate};
use crate::store::{self, Store, StoreError};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Most files indexed per root; the rest are left out (spec: caps).
pub const MAX_FILES_PER_ROOT: usize = 20_000;
/// Deepest folder level walked, so a loop of junctions cannot run forever.
const MAX_DEPTH: usize = 32;

const ATTR_OFFLINE: u32 = 0x0000_1000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// What one scan did, for the notice and the tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub indexed: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Could not be opened (in use, locked): retried at the next scan.
    pub pending: usize,
    pub removed: usize,
    /// Part of the folder could not be listed, or it held more than
    /// [`MAX_FILES_PER_ROOT`] files. Nothing is deleted then: a file that was
    /// merely not seen must not lose its index.
    pub incomplete: bool,
}

/// What the `[knowledge] folder` setting asks for.
#[derive(Debug, PartialEq, Eq)]
enum Setting {
    Off,
    Invalid(&'static str),
    Folder(PathBuf),
}

fn read_setting(folder: &str) -> Setting {
    let folder = folder.trim();
    if folder.is_empty() {
        return Setting::Off;
    }
    let path = PathBuf::from(folder);
    if !path.is_absolute() {
        return Setting::Invalid(
            "The knowledge folder must be a full path, such as C:\\Notes. Nothing was indexed.",
        );
    }
    if path.parent().is_none() {
        return Setting::Invalid(
            "The knowledge folder must be a folder, not a whole drive. Nothing was indexed.",
        );
    }
    Setting::Folder(path)
}

/// Starts the start-up pass on a background thread that ends with it. With a
/// folder set it indexes that folder. With the setting empty or invalid it
/// reads no folder at all and only retires a folder indexed earlier (a cheap
/// store check), so clearing the setting also clears the stored text.
/// Returns the thread so a test can wait for it; the app drops it.
///
/// Until Settings can add folders, `[knowledge] folder` is the whole
/// configuration. When Settings lands, this value is imported into
/// `knowledge_root` once and this function goes away.
///
/// An `Err` is a plain sentence for a card (the thread could not be created,
/// so no index will be made this run). A setting that is invalid is told
/// apart before this by [`startup_problem`], which the start-up card shows.
pub fn start_scan(folder: &str) -> Result<std::thread::JoinHandle<()>, String> {
    let setting = read_setting(folder);
    std::thread::Builder::new()
        .name("knowledge-scan".into())
        .spawn(move || guarded(|| run_scan(setting)))
        .map_err(|e| spawn_failed_notice(&e.to_string()))
}

/// What is wrong with the `[knowledge] folder` setting itself, knowable
/// without touching the disk, for the start-up card. `None` when the setting
/// is empty or usable. (A folder that does not exist is only known on the scan
/// thread, which queues its own notice.)
pub fn startup_problem(folder: &str) -> Option<String> {
    match read_setting(folder) {
        Setting::Invalid(why) => Some(why.to_string()),
        Setting::Off | Setting::Folder(_) => None,
    }
}

fn spawn_failed_notice(error: &str) -> String {
    format!(
        "Your knowledge folder could not be indexed this time: the background task could not be started ({}).",
        error.trim()
    )
}

/// Runs the scan body so a panic is caught and ends in a notice instead of
/// nothing. Only matters where panics unwind (dev and test builds): the
/// release profile is `panic = "abort"`, where nothing can be caught and the
/// crash note (`crash_note.rs`) shows a card at the next start instead. The
/// panic text is never shown (it can come from file contents).
fn guarded(work: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_err() {
        notice::report(
            "Indexing your knowledge folder stopped unexpectedly. Wingman keeps running; it will try again at the next start.",
        );
    }
}

/// Why a file could not be opened, in the two ways that matter: it may be
/// free next time, or it never will be.
#[derive(Debug, PartialEq, Eq)]
enum OpenFailure {
    /// In use right now (a sharing or lock violation), or gone since it was
    /// listed: try again at the next scan.
    Busy,
    /// Refused by permissions: retrying at every start only repeats the same
    /// notice, so it is recorded as failed and left alone until the file changes.
    Denied,
}

fn open_failure(e: &std::io::Error) -> OpenFailure {
    // ERROR_ACCESS_DENIED
    if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(5) {
        OpenFailure::Denied
    } else {
        OpenFailure::Busy
    }
}

/// The one sentence a finished scan leaves for the next card, if any.
fn summary_notice(summary: &ScanSummary) -> Option<String> {
    let unreadable = summary.failed + summary.pending;
    if unreadable > 0 {
        Some(format!(
            "{unreadable} {} in your knowledge folder could not be read.",
            if unreadable == 1 { "file" } else { "files" }
        ))
    } else if summary.incomplete {
        Some(
            "Part of your knowledge folder could not be listed, so some files may be missing from the index."
                .to_string(),
        )
    } else {
        None
    }
}

fn run_scan(setting: Setting) {
    // #1053: the first open of the store (if the start-up open has not done
    // it yet) must run at normal priority. At background priority it would
    // hold the store's lock for a long time while any other thread waits.
    let opened = store::global();
    lower_thread_priority();
    let wanted = match setting {
        Setting::Off => None,
        // Shown on the start-up card (`startup_problem`), not queued twice.
        Setting::Invalid(_) => None,
        Setting::Folder(folder) => match canonical_root(&folder) {
            Ok(root) => Some(root),
            Err(why) => {
                // The folder may be on a drive that is not connected right
                // now: keep what was indexed, say so, and try next start.
                notice::report(&why);
                return;
            }
        },
    };
    // A store that cannot open has already reported itself.
    let Ok(store) = opened else { return };
    let keep = wanted.as_ref().map(|r| r.to_string_lossy().into_owned());
    if let Err(e) = store.retire_default_roots_except(keep.as_deref()) {
        store::report(&e);
        return;
    }
    let Some(root) = wanted else { return };
    match scan_folder(&store, &root, now_unix()) {
        Ok(summary) => {
            if let Some(text) = summary_notice(&summary) {
                notice::report(&text);
            }
        }
        Err(e) => store::report(&e),
    }
}

/// Background mode lowers CPU, disk and memory priority for this thread.
/// Best effort: a refusal only means the scan runs at normal priority.
fn lower_thread_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    };
    // SAFETY: the pseudo-handle of the calling thread is always valid.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The folder as an absolute path without the `\\?\` prefix. The folder is
/// never created here (it is made on first use, spec question 1).
fn canonical_root(folder: &Path) -> Result<PathBuf, String> {
    let canon = std::fs::canonicalize(folder).map_err(|_| {
        format!(
            "The knowledge folder {} was not found, so nothing was indexed.",
            folder.display()
        )
    })?;
    if !canon.is_dir() {
        return Err(format!(
            "{} is not a folder, so nothing was indexed.",
            folder.display()
        ));
    }
    let text = canon.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => Ok(PathBuf::from(rest)),
        _ => Ok(canon),
    }
}

struct FoundFile {
    rel: String,
    path: PathBuf,
    size: u64,
    mtime: Option<i64>,
    attributes: u32,
    parser: Parser,
}

/// Walks `root` once and brings the store in line with it. A store error
/// stops the scan (the store is likely broken); a bad file never does.
pub fn scan_folder(store: &Store, root: &Path, now: i64) -> Result<ScanSummary, StoreError> {
    let root_key = root.to_string_lossy().into_owned();
    let root_id = store.ensure_root(&root_key, now)?;
    let (files, mut incomplete) = walk(root);
    let known = store.documents_in_root(root_id)?;
    let by_path: std::collections::HashMap<&str, &DocumentRow> =
        known.iter().map(|d| (d.rel_path.as_str(), d)).collect();
    let mut summary = ScanSummary::default();

    for file in &files {
        let existing = by_path.get(file.rel.as_str()).copied();
        match index_file(store, root_id, file, existing, now)? {
            Outcome::Indexed => summary.indexed += 1,
            Outcome::Unchanged => summary.unchanged += 1,
            Outcome::Skipped => summary.skipped += 1,
            Outcome::Failed => summary.failed += 1,
            Outcome::Pending => summary.pending += 1,
        }
    }

    if !incomplete {
        let seen: std::collections::HashSet<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        let gone: Vec<i64> = known
            .iter()
            .filter(|d| !seen.contains(d.rel_path.as_str()))
            .map(|d| d.id)
            .collect();
        summary.removed = gone.len();
        store.delete_documents(&gone)?;
    }
    incomplete |= files.len() >= MAX_FILES_PER_ROOT;
    summary.incomplete = incomplete;
    Ok(summary)
}

/// The indexable files under `root`, sorted by path and capped, and whether
/// any folder could not be listed. Symbolic links and junctions are not
/// followed; dot-folders, `~$` lock files and `.tmp` files are ignored.
fn walk(root: &Path) -> (Vec<FoundFile>, bool) {
    let mut files = Vec::new();
    let mut incomplete = false;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            incomplete = true;
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                incomplete = true;
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                incomplete = true;
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if name.starts_with('.') {
                    continue;
                }
                if depth + 1 > MAX_DEPTH {
                    incomplete = true;
                } else {
                    stack.push((entry.path(), depth + 1));
                }
                continue;
            }
            if !kind.is_file() || is_ignored_file(&name) {
                continue;
            }
            let Some(parser) = parser_for(&name) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                incomplete = true;
                continue;
            };
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            files.push(FoundFile {
                rel: rel_key(rel),
                size: meta.len(),
                mtime: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64),
                attributes: attributes(&meta),
                parser,
                path,
            });
        }
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    files.truncate(MAX_FILES_PER_ROOT);
    (files, incomplete)
}

fn is_ignored_file(name: &str) -> bool {
    name.starts_with("~$") || name.to_ascii_lowercase().ends_with(".tmp")
}

/// A path relative to its root, with `/` separators, as the document key.
fn rel_key(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn attributes(meta: &std::fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes()
}

enum Outcome {
    Indexed,
    Unchanged,
    Skipped,
    Failed,
    Pending,
}

fn index_file(
    store: &Store,
    root_id: i64,
    file: &FoundFile,
    existing: Option<&DocumentRow>,
    now: i64,
) -> Result<Outcome, StoreError> {
    let size = i64::try_from(file.size).ok();
    let stamp = DocumentUpdate {
        size,
        mtime_unix: file.mtime,
        ingested_unix: Some(now),
        ..Default::default()
    };

    // A file that exists only in the cloud: reading it would download it.
    if file.attributes & (ATTR_OFFLINE | ATTR_RECALL_ON_DATA_ACCESS) != 0 {
        store.mark_document(
            root_id,
            &file.rel,
            "skipped",
            Some("the file is not downloaded to this PC"),
            &stamp,
        )?;
        return Ok(Outcome::Skipped);
    }
    if file.size > MAX_FILE_BYTES as u64 {
        store.mark_document(
            root_id,
            &file.rel,
            "skipped",
            Some("the file is too large to index"),
            &stamp,
        )?;
        return Ok(Outcome::Skipped);
    }
    // Same size, same modified time, same parser: not even read.
    if let Some(doc) = existing {
        let settled = matches!(doc.status.as_str(), "indexed" | "failed")
            && doc.parser_ver == Some(PARSER_VERSION)
            && doc.size == size
            && doc.mtime_unix == file.mtime
            // Strictly older than the last index: a save in the same second
            // as the scan, with the same size, would otherwise be missed.
            && matches!((file.mtime, doc.ingested_unix), (Some(m), Some(t)) if m < t);
        if settled {
            return Ok(Outcome::Unchanged);
        }
    }

    let bytes = match read_capped(&file.path) {
        Ok(bytes) => bytes,
        Err(OpenFailure::Denied) => {
            return fail(
                store,
                root_id,
                file,
                &stamp,
                "the file could not be opened or read (permission refused or damaged)",
            );
        }
        Err(OpenFailure::Busy) => {
            store.mark_document(
                root_id,
                &file.rel,
                "pending",
                Some("the file could not be opened, it may be in use"),
                &DocumentUpdate::default(),
            )?;
            return Ok(Outcome::Pending);
        }
    };
    let Ok(hash) = sha256_hex(&bytes) else {
        return fail(
            store,
            root_id,
            file,
            &stamp,
            "the file could not be checked",
        );
    };
    if let Some(doc) = existing {
        if doc.status == "indexed"
            && doc.parser_ver == Some(PARSER_VERSION)
            && doc.content_hash.as_deref() == Some(hash.as_str())
        {
            // Touched but not changed: remember the new size and time.
            store.mark_document(root_id, &file.rel, "indexed", None, &stamp)?;
            return Ok(Outcome::Unchanged);
        }
    }
    let parsed = match parse(file.parser, &bytes) {
        Ok(parsed) => parsed,
        Err(e) => return fail(store, root_id, file, &stamp, &e.to_string()),
    };
    // Mask before chunking, so no chunk and no overlap can hold a number.
    let masked = mask_sensitive(&parsed.text);
    // The title is text from the file too (a front-matter title or the first
    // heading), and goes into the store, so it is masked the same way.
    let title = parsed.title.as_deref().map(mask_sensitive);
    let mut chunks = Vec::new();
    for chunk in chunk_text(&masked) {
        let Ok(text_hash) = sha256_hex(chunk.text.as_bytes()) else {
            return fail(
                store,
                root_id,
                file,
                &stamp,
                "the text could not be checked",
            );
        };
        chunks.push(IndexedChunk { chunk, text_hash });
    }

    // Pending first: a crash between these writes leaves a file that the next
    // scan reads again, never an indexed row with half its chunks.
    let doc_id = store.mark_document(
        root_id,
        &file.rel,
        "pending",
        None,
        &DocumentUpdate::default(),
    )?;
    store.replace_chunks(doc_id, &chunks)?;
    store.mark_document(
        root_id,
        &file.rel,
        "indexed",
        None,
        &DocumentUpdate {
            title: title.as_deref(),
            content_hash: Some(&hash),
            parser: Some(file.parser.name()),
            parser_ver: Some(PARSER_VERSION),
            ..stamp
        },
    )?;
    Ok(Outcome::Indexed)
}

/// A failed file keeps its older chunks (a half-saved file must not erase a
/// good index) and records why, in words that never quote the file.
fn fail(
    store: &Store,
    root_id: i64,
    file: &FoundFile,
    stamp: &DocumentUpdate<'_>,
    why: &str,
) -> Result<Outcome, StoreError> {
    store.mark_document(
        root_id,
        &file.rel,
        "failed",
        Some(why),
        &DocumentUpdate {
            parser_ver: Some(PARSER_VERSION),
            ..stamp.clone()
        },
    )?;
    Ok(Outcome::Failed)
}

/// The file's bytes, at most one byte past the size cap, so a file that grew
/// since it was listed is still bounded. An `Err` says whether trying again at
/// the next scan can help.
fn read_capped(path: &Path) -> Result<Vec<u8>, OpenFailure> {
    let file = std::fs::File::open(path).map_err(|e| open_failure(&e))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| read_failure(&e))?;
    Ok(bytes)
}

/// A read that failed after the file opened. Only a sharing or lock violation
/// (or a file gone since it was listed) is worth retrying at the next scan;
/// any other read error (a damaged or EFS file) would repeat, so it is
/// recorded as failed once and retried when the file changes.
fn read_failure(e: &std::io::Error) -> OpenFailure {
    match e.raw_os_error() {
        Some(32 | 33) => OpenFailure::Busy,
        _ if e.kind() == std::io::ErrorKind::NotFound => OpenFailure::Busy,
        _ => OpenFailure::Denied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A scan time later than any file in a test, so a file counts as settled.
    const FUT: i64 = 4_000_000_000;

    /// `start_scan` works on the one process-wide store and retires the roots
    /// it was not given, so the tests that go through it take turns.
    static GLOBAL_STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wingman-test-knowledge-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn section(name: &str, body: &str) -> String {
        format!("# {name}\n{}\n\n", body.repeat(60))
    }

    fn notes() -> String {
        [
            section("Work", "I joined Acme in 2019. "),
            section("Education", "BSc Physics, 2015. "),
            section("Skills", "Rust and Win32. "),
        ]
        .concat()
    }

    #[test]
    fn a_scan_indexes_text_and_markdown_and_ignores_the_rest() {
        let dir = scratch("basic");
        fs::write(dir.join("a.md"), "# Title\nfrobnicate the widget\n").unwrap();
        fs::write(dir.join("b.txt"), "plain words about gadgets\n").unwrap();
        fs::write(dir.join("image.png"), [0x89, b'P', b'N', b'G']).unwrap();
        fs::write(dir.join("~$lock.txt"), "lock").unwrap();
        fs::write(dir.join("scratch.tmp"), "tmp").unwrap();
        fs::create_dir_all(dir.join(".obsidian")).unwrap();
        fs::write(dir.join(".obsidian").join("x.md"), "hidden").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("c.md"), "nested widget\n").unwrap();

        let store = Store::in_memory();
        let summary = scan_folder(&store, &dir, FUT).unwrap();
        assert_eq!(summary.indexed, 3);
        assert_eq!(
            summary,
            ScanSummary {
                indexed: 3,
                ..Default::default()
            }
        );
        let root = store.ensure_root(&dir.to_string_lossy(), 0).unwrap();
        let mut rels: Vec<String> = store
            .documents_in_root(root)
            .unwrap()
            .into_iter()
            .map(|d| d.rel_path)
            .collect();
        rels.sort();
        assert_eq!(rels, ["a.md", "b.txt", "sub/c.md"]);
        assert_eq!(store.fts_hits("frobnicate").unwrap(), 1);
        assert_eq!(store.fts_hits("widget").unwrap(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_scan_after_one_edit_keeps_the_other_files_chunks() {
        let dir = scratch("edit");
        fs::write(dir.join("one.md"), notes()).unwrap();
        fs::write(dir.join("two.md"), section("Other", "unrelated text. ")).unwrap();
        let store = Store::in_memory();
        scan_folder(&store, &dir, FUT).unwrap();
        let root = store.ensure_root(&dir.to_string_lossy(), 0).unwrap();
        let one_before = store.chunks_of(root, "one.md").unwrap();
        let two_before = store.chunks_of(root, "two.md").unwrap();
        assert!(one_before.len() >= 3 && !two_before.is_empty());

        // Edit one section of one.md.
        fs::write(
            dir.join("one.md"),
            notes().replace("BSc Physics, 2015.", "MSc Physics, 2017.!"),
        )
        .unwrap();
        let summary = scan_folder(&store, &dir, FUT).unwrap();

        assert_eq!(summary.indexed, 1, "only the edited file is re-indexed");
        assert_eq!(summary.unchanged, 1);
        assert_eq!(store.chunks_of(root, "two.md").unwrap(), two_before);
        let one_after = store.chunks_of(root, "one.md").unwrap();
        assert_eq!(one_after.len(), one_before.len());
        let same = one_before
            .iter()
            .filter(|c| one_after.iter().any(|a| a.0 == c.0))
            .count();
        assert_eq!(
            same,
            one_before.len() - 1,
            "the untouched sections keep their rows"
        );
        assert!(one_after.iter().any(|c| c.1.contains("MSc Physics")));
        assert_eq!(
            store.fts_hits("BSc").unwrap(),
            0,
            "the old text left the index"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unchanged_file_is_not_read_again() {
        let dir = scratch("unchanged");
        fs::write(dir.join("a.md"), "# A\nsome words here\n").unwrap();
        let store = Store::in_memory();
        scan_folder(&store, &dir, FUT).unwrap();
        let again = scan_folder(&store, &dir, FUT).unwrap();
        assert_eq!(
            again,
            ScanSummary {
                unchanged: 1,
                ..Default::default()
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deleted_file_loses_its_document_chunks_and_search_hits() {
        let dir = scratch("delete");
        fs::write(dir.join("keep.md"), "# K\nkeepword\n").unwrap();
        fs::write(dir.join("gone.md"), "# G\ngoneword\n").unwrap();
        let store = Store::in_memory();
        scan_folder(&store, &dir, FUT).unwrap();
        let root = store.ensure_root(&dir.to_string_lossy(), 0).unwrap();
        assert_eq!(store.fts_hits("goneword").unwrap(), 1);

        fs::remove_file(dir.join("gone.md")).unwrap();
        let summary = scan_folder(&store, &dir, FUT).unwrap();

        assert_eq!(summary.removed, 1);
        assert!(store.chunks_of(root, "gone.md").unwrap().is_empty());
        assert_eq!(store.document_state(root, "gone.md").unwrap(), None);
        assert_eq!(store.fts_hits("goneword").unwrap(), 0);
        assert_eq!(store.fts_hits("keepword").unwrap(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_be_parsed_fails_and_keeps_its_old_chunks() {
        let dir = scratch("failed");
        fs::write(dir.join("a.txt"), "goodword lives here\n").unwrap();
        let store = Store::in_memory();
        scan_folder(&store, &dir, FUT).unwrap();
        let root = store.ensure_root(&dir.to_string_lossy(), 0).unwrap();
        let before = store.chunks_of(root, "a.txt").unwrap();

        // A NUL byte marks a binary file, which the parser refuses.
        fs::write(dir.join("a.txt"), b"bad\0bytes that are longer now").unwrap();
        let summary = scan_folder(&store, &dir, FUT).unwrap();

        assert_eq!(summary.failed, 1);
        let (status, error) = store.document_state(root, "a.txt").unwrap().unwrap();
        assert_eq!(status, "failed");
        assert!(error.unwrap().contains("text"));
        assert_eq!(store.chunks_of(root, "a.txt").unwrap(), before);
        // Not retried every start while it stays the same.
        let again = scan_folder(&store, &dir, FUT).unwrap();
        assert_eq!(again.failed, 0);
        assert_eq!(again.unchanged, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn card_and_national_id_numbers_never_reach_the_store() {
        let dir = scratch("mask");
        fs::write(
            dir.join("a.txt"),
            "Pay with 4111 1111 1111 1111 or SSN 123-45-6789, IBAN GB82 WEST 1234 5698 7654 32. visible\n",
        )
        .unwrap();
        let store = Store::in_memory();
        scan_folder(&store, &dir, FUT).unwrap();
        let root = store.ensure_root(&dir.to_string_lossy(), 0).unwrap();
        let text = store.chunks_of(root, "a.txt").unwrap()[0].1.clone();
        assert!(text.contains("[card number]") && text.contains("[national id]"));
        assert!(text.contains("[IBAN]") && text.contains("visible"));
        assert!(!text.contains("4111") && !text.contains("6789") && !text.contains("WEST"));
        assert_eq!(store.fts_hits("4111").unwrap(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_that_cannot_be_listed_is_incomplete_so_nothing_is_deleted() {
        let dir = scratch("incomplete");
        let not_a_dir = dir.join("file.txt");
        fs::write(&not_a_dir, "x").unwrap();
        let (files, incomplete) = walk(&not_a_dir);
        assert!(files.is_empty() && incomplete);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_same_size_save_in_the_same_second_as_the_scan_is_not_missed() {
        let dir = scratch("racy");
        fs::write(
            dir.join("a.txt"),
            "first version
",
        )
        .unwrap();
        let store = Store::in_memory();
        // Indexed at the very second the file was last written: not older
        // than the index, so a later same-size save cannot be told apart by
        // size and time alone.
        let at = fs::metadata(dir.join("a.txt"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        scan_folder(&store, &dir, at).unwrap();
        fs::write(
            dir.join("a.txt"),
            "second versio
",
        )
        .unwrap();
        let summary = scan_folder(&store, &dir, at).unwrap();
        assert_eq!(summary.indexed, 1);
        let root = store.ensure_root(&dir.to_string_lossy(), 0).unwrap();
        assert!(store.chunks_of(root, "a.txt").unwrap()[0]
            .1
            .contains("second"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unset_folder_reads_no_folder() {
        assert_eq!(read_setting(""), Setting::Off);
        assert_eq!(read_setting("   "), Setting::Off);
    }

    #[test]
    fn a_relative_path_and_a_drive_root_are_refused() {
        for bad in ["Notes", r"..\Notes", "C:", r"C:\", "/", r"\\server\share"] {
            assert!(
                matches!(read_setting(bad), Setting::Invalid(_)),
                "{bad} must be refused"
            );
        }
        assert!(!matches!(read_setting(r"C:\Notes"), Setting::Invalid(_)));
        for why in [read_setting("Notes"), read_setting(r"C:\")] {
            if let Setting::Invalid(text) = why {
                assert!(!text.contains('\u{2014}'));
            }
        }
    }

    #[test]
    fn clearing_or_changing_the_folder_retires_the_old_root() {
        let a = scratch("retire-a");
        let b = scratch("retire-b");
        fs::write(a.join("a.md"), "# A\noldfolderword\n").unwrap();
        fs::write(b.join("b.md"), "# B\nnewfolderword\n").unwrap();
        let store = Store::in_memory();
        scan_folder(&store, &a, FUT).unwrap();
        assert_eq!(store.fts_hits("oldfolderword").unwrap(), 1);

        // The same folder: nothing is retired.
        let a_key = a.to_string_lossy().into_owned();
        assert_eq!(store.retire_default_roots_except(Some(&a_key)).unwrap(), 0);
        assert_eq!(store.fts_hits("oldfolderword").unwrap(), 1);

        // A different folder: the old text goes, the new one is scanned.
        let b_key = b.to_string_lossy().into_owned();
        assert_eq!(store.retire_default_roots_except(Some(&b_key)).unwrap(), 1);
        scan_folder(&store, &b, FUT).unwrap();
        assert_eq!(store.fts_hits("oldfolderword").unwrap(), 0);
        assert_eq!(store.fts_hits("newfolderword").unwrap(), 1);

        // Cleared: everything goes, and a second call finds nothing to do.
        assert_eq!(store.retire_default_roots_except(None).unwrap(), 1);
        assert_eq!(store.knowledge_counts().unwrap(), (0, 0, 0));
        assert_eq!(store.fts_hits("newfolderword").unwrap(), 0);
        assert_eq!(store.retire_default_roots_except(None).unwrap(), 0);
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn an_empty_setting_still_clears_a_folder_indexed_earlier() {
        let _one_at_a_time = GLOBAL_STORE.lock().unwrap_or_else(|e| e.into_inner());
        // The wiring: start_scan("") runs on a thread and retires the root in
        // the process-wide store.
        let dir = scratch("clear-wired");
        fs::write(dir.join("n.md"), "# N\nclearedwiredword\n").unwrap();
        start_scan(&dir.to_string_lossy()).unwrap().join().unwrap();
        let store = store::global().unwrap();
        assert_eq!(store.fts_hits("clearedwiredword").unwrap(), 1);
        start_scan("").unwrap().join().unwrap();
        assert_eq!(store.fts_hits("clearedwiredword").unwrap(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_title_with_a_card_number_is_masked_in_the_store() {
        let dir = scratch("title");
        fs::write(
            dir.join("t.md"),
            "---\ntitle: 4111 1111 1111 1111\n---\nbody words\n",
        )
        .unwrap();
        let store = Store::in_memory();
        scan_folder(&store, &dir, FUT).unwrap();
        let root = store.ensure_root(&dir.to_string_lossy(), 0).unwrap();
        let title = store.document_title(root, "t.md").unwrap().unwrap();
        assert!(!title.contains("4111"), "title was {title}");
        assert_eq!(title, "[card number]");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_set_folder_is_indexed_through_the_global_store() {
        let _one_at_a_time = GLOBAL_STORE.lock().unwrap_or_else(|e| e.into_inner());
        // The wiring test: the entry point `app.rs` calls, a real thread, and
        // the process-wide store (a per-process temp directory under test).
        let dir = scratch("wired");
        fs::write(dir.join("n.md"), "# N\nwiredtestword\n").unwrap();
        start_scan(&dir.to_string_lossy())
            .expect("a thread starts for a set folder")
            .join()
            .unwrap();
        let store = store::global().unwrap();
        let root = canonical_root(&dir).unwrap();
        let root_id = store.ensure_root(&root.to_string_lossy(), 0).unwrap();
        assert_eq!(store.chunks_of(root_id, "n.md").unwrap().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_locked_file_is_retried_but_a_denied_file_is_not() {
        use std::io::{Error, ErrorKind};
        // 32 is ERROR_SHARING_VIOLATION, 33 ERROR_LOCK_VIOLATION: in use now.
        assert_eq!(
            open_failure(&Error::from_raw_os_error(32)),
            OpenFailure::Busy
        );
        assert_eq!(
            open_failure(&Error::from_raw_os_error(33)),
            OpenFailure::Busy
        );
        assert_eq!(
            open_failure(&Error::from(ErrorKind::NotFound)),
            OpenFailure::Busy
        );
        assert_eq!(
            open_failure(&Error::from(ErrorKind::PermissionDenied)),
            OpenFailure::Denied
        );
        assert_eq!(
            open_failure(&Error::from_raw_os_error(5)),
            OpenFailure::Denied
        );
    }

    #[test]
    fn a_read_error_is_retried_only_when_the_file_is_in_use_or_gone() {
        use std::io::{Error, ErrorKind};
        for busy in [
            Error::from_raw_os_error(32),
            Error::from_raw_os_error(33),
            Error::from(ErrorKind::NotFound),
        ] {
            assert_eq!(read_failure(&busy), OpenFailure::Busy, "{busy:?}");
        }
        for permanent in [
            Error::from_raw_os_error(23),
            Error::from(ErrorKind::InvalidData),
            Error::from(ErrorKind::UnexpectedEof),
        ] {
            assert_eq!(
                read_failure(&permanent),
                OpenFailure::Denied,
                "{permanent:?}"
            );
        }
    }

    #[test]
    fn a_file_denied_access_is_reported_once_and_not_on_the_next_scan() {
        let dir = scratch("denied");
        // Opening a folder as a file is refused with "access denied".
        let file = FoundFile {
            rel: "locked.md".into(),
            path: dir.clone(),
            size: 10,
            mtime: Some(100),
            attributes: 0,
            parser: parser_for("locked.md").unwrap(),
        };
        let store = Store::in_memory();
        let root = store.ensure_root("C:\\denied-root", 0).unwrap();
        let first = index_file(&store, root, &file, None, FUT).unwrap();
        assert!(matches!(first, Outcome::Failed), "denied is a failure");
        let (status, why) = store.document_state(root, "locked.md").unwrap().unwrap();
        assert_eq!(status, "failed");
        assert!(why.unwrap().contains("permission refused"));
        let known = store.documents_in_root(root).unwrap();
        let again = index_file(&store, root, &file, known.first(), FUT).unwrap();
        assert!(
            matches!(again, Outcome::Unchanged),
            "a denied file is not counted again at the next start"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_summary_notice_says_how_many_could_not_be_read() {
        assert_eq!(summary_notice(&ScanSummary::default()), None);
        let one = ScanSummary {
            failed: 1,
            ..Default::default()
        };
        assert_eq!(
            summary_notice(&one).as_deref(),
            Some("1 file in your knowledge folder could not be read.")
        );
        let many = ScanSummary {
            failed: 1,
            pending: 2,
            ..Default::default()
        };
        assert!(summary_notice(&many).unwrap().starts_with("3 files"));
        let partial = ScanSummary {
            incomplete: true,
            ..Default::default()
        };
        assert!(summary_notice(&partial)
            .unwrap()
            .contains("could not be listed"));
        for s in [&one, &many, &partial] {
            assert!(!summary_notice(s).unwrap().contains('\u{2014}'));
        }
    }

    #[test]
    fn a_bad_setting_has_a_start_up_problem_and_a_good_one_has_none() {
        assert_eq!(startup_problem(""), None);
        assert_eq!(startup_problem(r"C:\Notes"), None);
        assert!(startup_problem("Notes").unwrap().contains("full path"));
        assert!(startup_problem(r"C:\")
            .unwrap()
            .contains("not a whole drive"));
    }

    #[test]
    fn a_panic_in_the_scan_body_is_caught_and_ends_in_a_notice() {
        // Tests build with unwind; a release build aborts and the crash note
        // (crash_note.rs) covers that instead.
        // The notice queue is process-wide and the start_scan tests fill it.
        let _one_at_a_time = GLOBAL_STORE.lock().unwrap_or_else(|e| e.into_inner());
        let _ = notice::take();
        guarded(|| panic!("boom in the scan"));
        let text = notice::take().expect("a panic must leave a notice");
        assert!(text.contains("stopped unexpectedly"), "{text}");
        assert!(
            !text.contains("boom"),
            "the panic text is never shown: {text}"
        );
        guarded(|| {});
        assert_eq!(notice::take(), None, "a clean run leaves nothing");
    }

    #[test]
    fn a_thread_that_cannot_start_is_a_plain_sentence() {
        let text = spawn_failed_notice("not enough resources");
        assert!(text.contains("could not be started"), "{text}");
        assert!(text.contains("not enough resources"), "{text}");
        assert!(!text.contains('\u{2014}'));
    }

    #[test]
    fn a_missing_folder_is_a_message_not_a_new_folder() {
        let dir = scratch("missing").join("nope");
        let err = canonical_root(&dir).unwrap_err();
        assert!(err.contains("not found"));
        assert!(!dir.exists(), "the scan never creates the folder");
    }
}
