//! The knowledge folder watcher (#81, spec "Ingest pipeline > Watching").
//!
//! One thread, started only when a usable `[knowledge] folder` is set, blocked
//! in `WaitForMultipleObjects` on the directory's overlapped
//! `ReadDirectoryChangesW` event and a stop event. The timeout is infinite
//! unless changes are waiting, so while idle nothing runs: no timer, no polling
//! (AGENTS.md rule 5). A burst of events from one save collapses into one
//! entry during a bounded 750 ms wait, which exists only while something is
//! pending ([`Pending`]).
//!
//! What a change means is decided by pure code ([`parse_notify_buffer`],
//! [`plan`], [`Pending`], [`completion_kind`]); the thread only moves bytes
//! between Windows and those functions, and every file goes through the same
//! `ingest::index_file` the start-up scan uses.
//!
//! Failure rules:
//! - Folder deleted or renamed while watched: before a batch is applied the
//!   root path is checked, and if it is gone nothing is deleted from the index
//!   (a missing folder says nothing about its files), a notice is queued and
//!   the thread ends. Reload settings starts a new one.
//! - Buffer overflow (a zero-byte completion or `ERROR_NOTIFY_ENUM_DIR`): one
//!   full reconciliation, the same walk the start-up scan does.
//! - Quit or a changed folder: dropping the [`Watcher`] sets the stop event and
//!   waits at most 150 ms for the thread to end. The thread checks the event
//!   before every store write, cancels its read, waits for the cancel and ends.
//!   It is never joined without a bound, so Quit never waits on a busy disk.
//! - A file the editor still holds open: retried three times, 500 ms apart
//!   growing, inside the same pending mechanism, then left `pending` for the
//!   next event.

use super::ingest::{self, Outcome, Probe};
use super::notice;
use crate::store::knowledge::DocumentRow;
use crate::store::{self, Store, StoreError};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NOTIFY_ENUM_DIR, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetLongPathNameW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// How long a burst of events is gathered before it is applied.
pub const DEBOUNCE: Duration = Duration::from_millis(750);
/// The longest `start` waits for the first read to be armed.
const ARM_WAIT: Duration = Duration::from_millis(500);
/// The first retry of a file that is in use; each further one waits longer.
const RETRY_STEP: Duration = Duration::from_millis(500);
/// Tries for a file that is in use, inside one run of the watcher.
const MAX_TRIES: u8 = 3;
/// The directory-change buffer (spec: 64 KB).
const BUFFER_BYTES: usize = 64 * 1024;

/// One change Windows reported, with a path relative to the root and `/`
/// separators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added(String),
    Removed(String),
    Modified(String),
    RenamedOld(String),
    RenamedNew(String),
}

/// What to do about the changes of one pending window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Something changed at this path: look at it and index it if it is a file.
    Index(String),
    /// Something appeared at this path: as `Index`, and a new folder means a
    /// reconciliation (its files were never seen).
    Discover(String),
    /// Something went away: drop its rows (and any under it) if it is gone.
    Remove(String),
    /// A file was moved: keep its row and chunks under the new path.
    Rename { from: String, to: String },
    /// Walk the whole root once.
    Rescan,
}

/// Reads the `FILE_NOTIFY_INFORMATION` records Windows wrote into `bytes`.
/// Bounds-checked everywhere: a short or odd buffer ends the list, never a
/// panic.
pub fn parse_notify_buffer(bytes: &[u8]) -> Vec<Change> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(head) = at.checked_add(12).and_then(|end| bytes.get(at..end)) {
        let word = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        let next = word(0) as usize;
        let action = word(4);
        let len = word(8) as usize;
        let start = at + 12;
        let Some(raw) = start.checked_add(len).and_then(|end| bytes.get(start..end)) else {
            break;
        };
        if len % 2 == 0 {
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let name = String::from_utf16_lossy(&units).replace('\\', "/");
            if !name.is_empty() {
                match action {
                    1 => out.push(Change::Added(name)),
                    2 => out.push(Change::Removed(name)),
                    3 => out.push(Change::Modified(name)),
                    4 => out.push(Change::RenamedOld(name)),
                    5 => out.push(Change::RenamedNew(name)),
                    _ => {}
                }
            }
        }
        if next == 0 {
            break;
        }
        let Some(following) = at.checked_add(next) else {
            break;
        };
        at = following;
    }
    out
}

/// Turns the changes of one window into actions, in order, one per path.
/// Paths the scan would never index produce nothing. An overflow is one
/// [`Action::Rescan`] and nothing else, since the walk covers it all.
pub fn plan(changes: &[Change], overflow: bool) -> Vec<Action> {
    if overflow {
        return vec![Action::Rescan];
    }
    let mut out: Vec<Action> = Vec::new();
    let mut held: Option<String> = None;
    for change in changes {
        if !matches!(change, Change::RenamedNew(_)) {
            if let Some(old) = held.take() {
                plan_removed(&mut out, &old);
            }
        }
        match change {
            Change::Added(p) => plan_added(&mut out, p),
            Change::Modified(p) => plan_modified(&mut out, p),
            Change::Removed(p) => plan_removed(&mut out, p),
            Change::RenamedOld(p) => {
                if let Some(previous) = held.replace(p.clone()) {
                    plan_removed(&mut out, &previous);
                }
            }
            Change::RenamedNew(to) => match held.take() {
                Some(from) => plan_renamed(&mut out, from, to),
                None => plan_added(&mut out, to),
            },
        }
    }
    if let Some(old) = held {
        plan_removed(&mut out, &old);
    }
    out
}

fn names(action: &Action, rel: &str) -> bool {
    matches!(action, Action::Index(p) | Action::Discover(p) | Action::Remove(p) if p == rel)
}

fn plan_added(out: &mut Vec<Action>, rel: &str) {
    if ingest::is_ignored_rel(rel) {
        return;
    }
    out.retain(|a| !matches!(a, Action::Remove(p) if p == rel));
    if let Some(slot) = out
        .iter_mut()
        .find(|a| matches!(a, Action::Index(p) if p == rel))
    {
        *slot = Action::Discover(rel.to_string());
    } else if !out.iter().any(|a| names(a, rel)) {
        out.push(Action::Discover(rel.to_string()));
    }
}

fn plan_modified(out: &mut Vec<Action>, rel: &str) {
    if ingest::is_ignored_rel(rel) || out.iter().any(|a| names(a, rel)) {
        return;
    }
    out.push(Action::Index(rel.to_string()));
}

fn plan_removed(out: &mut Vec<Action>, rel: &str) {
    if ingest::is_ignored_rel(rel) {
        return;
    }
    out.retain(|a| !matches!(a, Action::Index(p) | Action::Discover(p) if p == rel));
    if !out.iter().any(|a| names(a, rel)) {
        out.push(Action::Remove(rel.to_string()));
    }
}

fn plan_renamed(out: &mut Vec<Action>, from: String, to: &str) {
    match (ingest::is_ignored_rel(&from), ingest::is_ignored_rel(to)) {
        (true, true) => {}
        (true, false) => plan_added(out, to),
        (false, true) => plan_removed(out, &from),
        (false, false) => {
            out.retain(|a| !matches!(a, Action::Index(p) | Action::Discover(p) if *p == from));
            out.push(Action::Rename {
                from,
                to: to.to_string(),
            });
        }
    }
}

/// Changes waiting for their window to close.
#[derive(Debug, Default)]
pub struct Pending {
    changes: Vec<Change>,
    overflow: bool,
    deadline: Option<Instant>,
}

impl Pending {
    /// The window closes at the earliest deadline asked for, so later events
    /// of the same burst never push it out.
    fn arm(&mut self, due: Instant) {
        self.deadline = Some(match self.deadline {
            Some(existing) if existing < due => existing,
            _ => due,
        });
    }

    /// Adds changes; the first of a burst opens a [`DEBOUNCE`] window.
    pub fn record(&mut self, changes: Vec<Change>, now: Instant) {
        if changes.is_empty() {
            return;
        }
        self.changes.extend(changes);
        self.arm(now + DEBOUNCE);
    }

    /// Some changes were not reported: one full reconciliation is due.
    pub fn mark_overflow(&mut self, now: Instant) {
        self.overflow = true;
        self.arm(now + DEBOUNCE);
    }

    /// Looks at `rel` again after `delay` (a file that was in use).
    pub fn retry(&mut self, rel: String, now: Instant, delay: Duration) {
        self.changes.push(Change::Modified(rel));
        self.arm(now + delay);
    }

    /// How long to wait for the next event: `None` is forever.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        self.deadline.map(|d| d.saturating_duration_since(now))
    }

    pub fn is_due(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|d| now >= d)
    }

    /// Empties the window and returns what to do.
    pub fn take(&mut self) -> Vec<Action> {
        let actions = plan(&self.changes, self.overflow);
        self.changes.clear();
        self.overflow = false;
        self.deadline = None;
        actions
    }
}

/// The wait timeout in whole milliseconds, rounded up: a window with less than
/// a millisecond left must not wait 0 (a busy loop until it closes). `None` is
/// forever. Capped at a minute; the loop just comes round again.
pub fn wait_millis(wait: Option<Duration>) -> u32 {
    wait.map_or(INFINITE, |d| {
        u32::try_from(d.as_nanos().div_ceil(1_000_000))
            .unwrap_or(u32::MAX)
            .min(60_000)
    })
}

/// `rel` with every folder and the file spelled as the disk stores them. Windows
/// can report a change under an 8.3 short name (`NOTES~1.MD`); keyed as is it
/// would become a second document next to `notes.md`. A file that is already
/// gone cannot be looked up, so its folder is and its own name is kept.
pub fn long_rel(root: &Path, rel: &str) -> String {
    let full = root.join(rel.replace('/', "\\"));
    let long = long_path(&full).or_else(|| {
        let name = full.file_name()?;
        Some(long_path(full.parent()?)?.join(name))
    });
    long.and_then(|p| p.strip_prefix(root).ok().map(ingest::rel_key))
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| rel.to_string())
}

fn long_path(path: &Path) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut buf = vec![0u16; 32_768];
    // SAFETY: `wide` is NUL-terminated and outlives the call; `buf` is the
    // buffer length given.
    let n = unsafe { GetLongPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf)) } as usize;
    (n > 0 && n < buf.len())
        .then(|| std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])))
}

fn long_changes(root: &Path, changes: Vec<Change>) -> Vec<Change> {
    changes
        .into_iter()
        .map(|c| match c {
            Change::Added(p) => Change::Added(long_rel(root, &p)),
            Change::Removed(p) => Change::Removed(long_rel(root, &p)),
            Change::Modified(p) => Change::Modified(long_rel(root, &p)),
            Change::RenamedOld(p) => Change::RenamedOld(long_rel(root, &p)),
            Change::RenamedNew(p) => Change::RenamedNew(long_rel(root, &p)),
        })
        .collect()
}

/// What a finished `ReadDirectoryChangesW` means.
#[derive(Debug, PartialEq, Eq)]
pub enum Completion {
    /// This many bytes of records to read.
    Records(usize),
    /// The buffer overflowed: some changes were not reported.
    Overflow,
    /// The watch is broken (folder gone, handle invalid): the Win32 code.
    Failed(u32),
}

/// `Ok(bytes)` or `Err(win32 code)` from `GetOverlappedResult`, as a decision.
pub fn completion_kind(result: Result<u32, u32>) -> Completion {
    match result {
        Ok(0) => Completion::Overflow,
        Ok(n) => Completion::Records(n as usize),
        Err(code) if code == ERROR_NOTIFY_ENUM_DIR.0 => Completion::Overflow,
        Err(code) => Completion::Failed(code),
    }
}

/// The rows at or under `rel` (a file, or a folder and everything in it).
/// An empty `rel` matches nothing: a delete never means the whole index.
pub fn docs_under(rows: &[DocumentRow], rel: &str) -> Vec<i64> {
    if rel.is_empty() {
        return Vec::new();
    }
    let prefix = format!("{rel}/");
    rows.iter()
        .filter(|r| r.rel_path == rel || r.rel_path.starts_with(&prefix))
        .map(|r| r.id)
        .collect()
}

/// Brings the store in line with `actions`. Returns the paths that could not
/// be opened (in use) so the caller can try them again shortly. A store error
/// stops it. `stopped` is asked before every write: once it says yes nothing
/// more is written (a watcher that was replaced must not touch rows the new
/// scan now owns).
pub fn apply(
    store: &Store,
    root_id: i64,
    root: &Path,
    actions: &[Action],
    now: i64,
    stopped: &dyn Fn() -> bool,
) -> Result<Vec<String>, StoreError> {
    let mut busy = Vec::new();
    let mut rescan = false;
    for action in actions {
        if stopped() {
            return Ok(busy);
        }
        match action {
            Action::Rescan => rescan = true,
            Action::Index(rel) | Action::Discover(rel) | Action::Remove(rel) => {
                let discovering = matches!(action, Action::Discover(_));
                match ingest::probe(root, rel) {
                    Probe::Gone => remove_under(store, root_id, rel)?,
                    Probe::Dir => rescan |= discovering,
                    Probe::Ignored => {}
                    Probe::Unreadable => busy.push(rel.clone()),
                    Probe::File(file) => {
                        if index_one(store, root_id, &file, now)? {
                            busy.push(rel.clone());
                        }
                    }
                }
            }
            Action::Rename { from, to } => match ingest::probe(root, to) {
                Probe::File(file) => {
                    store.rename_document(root_id, from, to)?;
                    if matches!(ingest::probe(root, from), Probe::Gone) {
                        remove_under(store, root_id, from)?;
                    }
                    if index_one(store, root_id, &file, now)? {
                        busy.push(to.clone());
                    }
                }
                Probe::Dir => rescan = true,
                Probe::Gone | Probe::Ignored => {
                    if matches!(ingest::probe(root, from), Probe::Gone | Probe::Ignored) {
                        remove_under(store, root_id, from)?;
                    }
                }
                Probe::Unreadable => busy.push(to.clone()),
            },
        }
    }
    if rescan && !stopped() {
        let summary = ingest::scan_folder(store, root, now)?;
        if let Some(text) = ingest::summary_notice(&summary) {
            notice::report(&text);
        }
    }
    Ok(busy)
}

/// Indexes one file through the scan's own per-file path. `true` when it could
/// not be opened and should be retried.
fn index_one(
    store: &Store,
    root_id: i64,
    file: &ingest::FoundFile,
    now: i64,
) -> Result<bool, StoreError> {
    let existing = store.document_row(root_id, &file.rel)?;
    let outcome = ingest::index_file(store, root_id, file, existing.as_ref(), now)?;
    Ok(matches!(outcome, Outcome::Pending))
}

/// Deletes the rows at or under `rel`, but only when nothing is there now.
fn remove_under(store: &Store, root_id: i64, rel: &str) -> Result<(), StoreError> {
    let rows = store.documents_in_root(root_id)?;
    store.delete_documents(&docs_under(&rows, rel))
}

/// The thread's handle on the outside: dropping it asks the thread to stop.
pub struct Watcher {
    stop: Arc<StopEvent>,
    thread: std::thread::JoinHandle<()>,
}

/// The stop event. Shared with the thread; closed when both are done with it.
struct StopEvent(HANDLE);

// SAFETY: an event handle is a kernel object usable from any thread.
unsafe impl Send for StopEvent {}
unsafe impl Sync for StopEvent {}

impl Drop for StopEvent {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Watcher {
    /// False once the thread has ended (stopped, or the folder went away).
    pub fn is_running(&self) -> bool {
        !self.thread.is_finished()
    }

    #[cfg(test)]
    fn wait_stopped(&self, within: Duration) -> bool {
        let end = Instant::now() + within;
        while Instant::now() < end {
            if self.thread.is_finished() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.thread.is_finished()
    }
}

/// The longest dropping a [`Watcher`] waits for its thread to notice the stop.
const STOP_WAIT: Duration = Duration::from_millis(150);

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: `stop` holds a live event handle.
        unsafe {
            let _ = SetEvent(self.stop.0);
        }
        // Reload settings drops the old watcher and starts a scan for the new
        // folder. The thread checks the stop event before every write, so a
        // short wait here lets it be out before that scan retires the old root.
        // Bounded on purpose: the UI thread never waits on a busy disk, and a
        // thread still inside one file write is covered by the per-write check.
        let end = Instant::now() + STOP_WAIT;
        while !self.thread.is_finished() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Starts watching the folder from the `[knowledge] folder` setting. `Ok(None)`
/// when the setting is empty or unusable (nothing to watch; the start-up card
/// already says why). `Err` is a plain sentence for a card: the thread or its
/// event could not be created.
pub fn start(folder: &str) -> Result<Option<Watcher>, String> {
    let Some(target) = ingest::watch_target(folder) else {
        return Ok(None);
    };
    // SAFETY: plain event creation, no pointers.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|e| failed_to_start(&e.to_string()))?;
    let stop = Arc::new(StopEvent(event));
    let for_thread = Arc::clone(&stop);
    let (ready, armed) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("knowledge-watch".into())
        .spawn(move || ingest::guarded(|| watch(&target, &for_thread, ready)))
        .map_err(|e| failed_to_start(&e.to_string()))?;
    // Wait (bounded) for the first read to be armed, so the caller can start
    // the start-up scan knowing no later change can slip between the two. A
    // slow or dead drive only costs this bound; the scan still runs.
    let _ = armed.recv_timeout(ARM_WAIT);
    Ok(Some(Watcher { stop, thread }))
}

fn failed_to_start(error: &str) -> String {
    format!(
        "Wingman could not start watching your knowledge folder for changes ({}). New and changed files are picked up at the next start.",
        error.trim()
    )
}

/// The folder watch ended because the folder is not there any more.
fn gone_notice(root: &Path) -> String {
    format!(
        "The knowledge folder {} is no longer available, so Wingman stopped watching it. Nothing was removed from the index. Choose Reload settings once the folder is back.",
        root.display()
    )
}

/// One directory watch: the handle, the event its reads signal, and the buffer
/// and `OVERLAPPED` Windows writes into. Dropping it cancels a read in flight
/// and waits for the cancel, so the kernel never writes into freed memory.
struct DirWatch {
    dir: HANDLE,
    event: HANDLE,
    overlapped: Box<OVERLAPPED>,
    buffer: Vec<u32>,
    in_flight: bool,
}

impl DirWatch {
    fn open(root: &Path) -> Result<DirWatch, String> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = root
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call. Sharing
        // delete lets the user delete or rename the folder while it is watched.
        let dir = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(|e| e.to_string())?;
        // SAFETY: plain event creation.
        let event = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
            Ok(event) => event,
            Err(e) => {
                // SAFETY: `dir` was just opened.
                unsafe {
                    let _ = CloseHandle(dir);
                }
                return Err(e.to_string());
            }
        };
        let overlapped = Box::new(OVERLAPPED {
            hEvent: event,
            ..Default::default()
        });
        Ok(DirWatch {
            dir,
            event,
            overlapped,
            buffer: vec![0u32; BUFFER_BYTES / 4],
            in_flight: false,
        })
    }

    /// Asks Windows to report the next changes into the buffer.
    fn issue(&mut self) -> Result<(), String> {
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE;
        // SAFETY: the buffer and the OVERLAPPED are heap allocations owned by
        // `self` that stay put until the read completes or is cancelled in
        // `Drop`; the handle was opened with FILE_FLAG_OVERLAPPED.
        let issued = unsafe {
            ReadDirectoryChangesW(
                self.dir,
                self.buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
                true,
                filter,
                None,
                Some(&mut *self.overlapped),
                None,
            )
        };
        match issued {
            Ok(()) => {
                self.in_flight = true;
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Collects a finished read. Call only after the event signalled. `Err` is
    /// the Win32 code of a watch that is broken.
    fn complete(&mut self) -> Result<Read, u32> {
        let mut got = 0u32;
        // SAFETY: the read was issued on this handle with this OVERLAPPED.
        let result = unsafe { GetOverlappedResult(self.dir, &*self.overlapped, &mut got, false) };
        self.in_flight = false;
        let result = result.map(|()| got).map_err(|e| win32_code(e.code()));
        match completion_kind(result) {
            Completion::Records(n) => {
                // SAFETY: Windows wrote `n` bytes into the start of the buffer,
                // which holds BUFFER_BYTES; `n` is clamped to that.
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        self.buffer.as_ptr().cast::<u8>(),
                        n.min(BUFFER_BYTES),
                    )
                };
                Ok(Read::Changes(parse_notify_buffer(bytes)))
            }
            Completion::Overflow => Ok(Read::Overflow),
            Completion::Failed(code) => Err(code),
        }
    }
}

enum Read {
    Changes(Vec<Change>),
    Overflow,
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        // SAFETY: cancel and wait so the kernel is done with the buffer, then
        // close each handle once.
        unsafe {
            if self.in_flight {
                let _ = CancelIoEx(self.dir, Some(&*self.overlapped));
                let mut got = 0u32;
                let _ = GetOverlappedResult(self.dir, &*self.overlapped, &mut got, true);
            }
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.dir);
        }
    }
}

fn win32_code(code: HRESULT) -> u32 {
    // A Win32 error wrapped as an HRESULT is 0x8007xxxx.
    (code.0 as u32) & 0xFFFF
}

/// The watch ended on its own: say why in plain words, once.
fn report_ended(root: &Path, code: Option<u32>) {
    if root.is_dir() {
        let why = code.map_or_else(String::new, |c| format!(" (Windows error {c})"));
        notice::report(&format!(
            "Wingman stopped watching your knowledge folder{why}. New and changed files are picked up at the next start."
        ));
    } else {
        notice::report(&gone_notice(root));
    }
}

fn stop_requested(stop: &StopEvent) -> bool {
    // SAFETY: `stop` holds a live event handle.
    unsafe { WaitForSingleObject(stop.0, 0) == WAIT_OBJECT_0 }
}

/// The thread body: open the folder, then wait for changes or the stop event.
/// `ready` is sent once the first read is armed; dropping it unsent (any early
/// return) tells `start` there is nothing to wait for.
fn watch(folder: &Path, stop: &StopEvent, ready: std::sync::mpsc::SyncSender<()>) {
    // A folder that is not there is the scan's notice to give, not ours.
    let Ok(root) = ingest::canonical_root(folder) else {
        return;
    };
    // Armed before anything slow (the store may still be opening), so a change
    // made from here on is buffered, and a scan started after `start` returns
    // cannot miss one.
    let mut dir = match DirWatch::open(&root) {
        Ok(dir) => dir,
        Err(e) => {
            notice::report(&failed_to_start(&e));
            return;
        }
    };
    if let Err(e) = dir.issue() {
        notice::report(&failed_to_start(&e));
        return;
    }
    let _ = ready.send(());
    // #1053: open the store (if the start-up open has not) at normal priority,
    // as the scan does, and only then drop to background. A store that cannot
    // open has already reported itself.
    let opened = store::global();
    ingest::lower_thread_priority();
    let Ok(store) = opened else { return };
    let root_key = root.to_string_lossy().into_owned();
    let root_id = match store.ensure_root(&root_key, ingest::now_unix()) {
        Ok(id) => id,
        Err(e) => {
            store::report(&e);
            return;
        }
    };
    let mut pending = Pending::default();
    let mut tries: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
    loop {
        let timeout = wait_millis(pending.wait(Instant::now()));
        let handles = [dir.event, stop.0];
        // SAFETY: both handles are live events owned by `dir` and `stop`.
        let woke = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
        if woke == WAIT_OBJECT_0 {
            match dir.complete() {
                Ok(Read::Changes(changes)) => {
                    pending.record(long_changes(&root, changes), Instant::now());
                }
                Ok(Read::Overflow) => pending.mark_overflow(Instant::now()),
                Err(code) => {
                    report_ended(&root, Some(code));
                    return;
                }
            }
            if let Err(e) = dir.issue() {
                let _ = e;
                report_ended(&root, None);
                return;
            }
        } else if woke != WAIT_TIMEOUT {
            // The stop event, or the wait itself failed: either way, done.
            return;
        }
        if !pending.is_due(Instant::now()) {
            continue;
        }
        if stop_requested(stop) {
            return;
        }
        // A folder that was deleted or renamed says nothing about its files:
        // touch nothing and stop.
        if !root.is_dir() {
            report_ended(&root, None);
            return;
        }
        let actions = pending.take();
        let stopped = || stop_requested(stop);
        match apply(
            &store,
            root_id,
            &root,
            &actions,
            ingest::now_unix(),
            &stopped,
        ) {
            Ok(busy) => {
                tries.retain(|rel, _| busy.contains(rel));
                let now = Instant::now();
                for rel in busy {
                    let n = {
                        let n = tries.entry(rel.clone()).or_insert(0);
                        *n += 1;
                        *n
                    };
                    if n <= MAX_TRIES {
                        pending.retry(rel, now, RETRY_STEP * u32::from(n));
                    } else {
                        tries.remove(&rel);
                    }
                }
            }
            Err(e) => {
                store::report(&e);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> String {
        x.to_string()
    }

    /// One record the way Windows lays it out.
    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut out = Vec::new();
        out.extend(next.to_le_bytes());
        out.extend(action.to_le_bytes());
        out.extend(((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            out.extend(u.to_le_bytes());
        }
        out
    }

    #[test]
    fn a_buffer_of_records_becomes_changes_with_slash_paths() {
        let first = record(0, 1, r"sub\a.md");
        let mut buf = record(first.len() as u32, 1, r"sub\a.md");
        buf.extend(record(0, 2, "b.txt"));
        assert_eq!(
            parse_notify_buffer(&buf),
            vec![Change::Added(s("sub/a.md")), Change::Removed(s("b.txt"))]
        );
        let mut four = record(0, 3, "m.md");
        assert_eq!(
            parse_notify_buffer(&four),
            vec![Change::Modified(s("m.md"))]
        );
        four = record(0, 4, "old.md");
        assert_eq!(
            parse_notify_buffer(&four),
            vec![Change::RenamedOld(s("old.md"))]
        );
        assert_eq!(
            parse_notify_buffer(&record(0, 5, "new.md")),
            vec![Change::RenamedNew(s("new.md"))]
        );
    }

    #[test]
    fn a_short_or_hostile_buffer_ends_the_list_without_a_panic() {
        assert!(parse_notify_buffer(&[]).is_empty());
        assert!(parse_notify_buffer(&[1, 2, 3]).is_empty());
        // A name length that runs past the end.
        let mut lying = record(0, 1, "a.md");
        lying[8] = 0xFF;
        assert!(parse_notify_buffer(&lying).is_empty());
        // A next offset that points past the end, and one that points at itself.
        let mut far = record(9999, 1, "a.md");
        assert_eq!(parse_notify_buffer(&far).len(), 1);
        far = record(0, 99, "a.md");
        assert!(parse_notify_buffer(&far).is_empty(), "unknown action");
        // An odd name length is refused.
        let mut odd = record(0, 1, "a.md");
        odd[8] = 7;
        assert!(parse_notify_buffer(&odd).is_empty());
    }

    #[test]
    fn a_save_burst_collapses_to_one_index_per_file() {
        let changes = [
            Change::Modified(s("a.md")),
            Change::Modified(s("a.md")),
            Change::Modified(s("b.md")),
            Change::Modified(s("a.md")),
        ];
        assert_eq!(
            plan(&changes, false),
            vec![Action::Index(s("a.md")), Action::Index(s("b.md"))]
        );
    }

    #[test]
    fn a_new_file_is_discovered_and_stays_discovered_when_modified_after() {
        let changes = [Change::Added(s("n.md")), Change::Modified(s("n.md"))];
        assert_eq!(plan(&changes, false), vec![Action::Discover(s("n.md"))]);
        let late = [Change::Modified(s("n.md")), Change::Added(s("n.md"))];
        assert_eq!(plan(&late, false), vec![Action::Discover(s("n.md"))]);
    }

    #[test]
    fn a_delete_cancels_an_index_and_a_recreate_cancels_the_delete() {
        let gone = [Change::Modified(s("a.md")), Change::Removed(s("a.md"))];
        assert_eq!(plan(&gone, false), vec![Action::Remove(s("a.md"))]);
        let back = [Change::Removed(s("a.md")), Change::Added(s("a.md"))];
        assert_eq!(plan(&back, false), vec![Action::Discover(s("a.md"))]);
    }

    #[test]
    fn a_rename_keeps_the_row_but_an_atomic_save_is_just_a_new_version() {
        let moved = [
            Change::RenamedOld(s("old.md")),
            Change::RenamedNew(s("sub/new.md")),
        ];
        assert_eq!(
            plan(&moved, false),
            vec![Action::Rename {
                from: s("old.md"),
                to: s("sub/new.md")
            }]
        );
        // An editor writes notes.md.tmp and renames it over notes.md.
        let atomic = [
            Change::Added(s("notes.md.tmp")),
            Change::Modified(s("notes.md.tmp")),
            Change::RenamedOld(s("notes.md.tmp")),
            Change::RenamedNew(s("notes.md")),
        ];
        assert_eq!(plan(&atomic, false), vec![Action::Discover(s("notes.md"))]);
        // Renamed to a name that is ignored: the old row goes.
        let hidden = [
            Change::RenamedOld(s("a.md")),
            Change::RenamedNew(s("a.md.tmp")),
        ];
        assert_eq!(plan(&hidden, false), vec![Action::Remove(s("a.md"))]);
        // An old name with no new one is a removal.
        let lone = [Change::RenamedOld(s("a.md")), Change::Modified(s("b.md"))];
        assert_eq!(
            plan(&lone, false),
            vec![Action::Remove(s("a.md")), Action::Index(s("b.md"))]
        );
        assert_eq!(
            plan(&[Change::RenamedOld(s("a.md"))], false),
            vec![Action::Remove(s("a.md"))]
        );
    }

    #[test]
    fn ignored_paths_never_become_actions_and_an_overflow_is_one_rescan() {
        let changes = [
            Change::Added(s(".obsidian/x.md")),
            Change::Modified(s("~$lock.docx")),
            Change::Modified(s("scratch.tmp")),
            Change::Removed(s(".git/HEAD")),
        ];
        assert!(plan(&changes, false).is_empty());
        let some = [Change::Modified(s("a.md")), Change::Removed(s("b.md"))];
        assert_eq!(plan(&some, true), vec![Action::Rescan]);
        assert_eq!(plan(&[], true), vec![Action::Rescan]);
    }

    #[test]
    fn a_window_closes_750_ms_after_its_first_event_and_not_before() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        assert_eq!(p.wait(t0), None, "nothing pending waits forever");
        assert!(!p.is_due(t0));
        p.record(vec![Change::Modified(s("a.md"))], t0);
        assert_eq!(p.wait(t0), Some(DEBOUNCE));
        // A later event in the same burst does not push the deadline out.
        p.record(
            vec![Change::Modified(s("b.md"))],
            t0 + Duration::from_millis(500),
        );
        assert_eq!(
            p.wait(t0 + Duration::from_millis(500)),
            Some(Duration::from_millis(250))
        );
        assert!(!p.is_due(t0 + Duration::from_millis(749)));
        assert!(p.is_due(t0 + DEBOUNCE));
        assert_eq!(p.take().len(), 2);
        assert_eq!(p.wait(t0 + DEBOUNCE), None, "empty again, so forever");
        assert!(!p.is_due(t0 + DEBOUNCE * 4));
    }

    #[test]
    fn recording_nothing_arms_no_window() {
        let mut p = Pending::default();
        p.record(Vec::new(), Instant::now());
        assert_eq!(p.wait(Instant::now()), None);
    }

    #[test]
    fn an_overflow_and_a_retry_each_arm_a_window() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        p.mark_overflow(t0);
        assert_eq!(p.wait(t0), Some(DEBOUNCE));
        assert_eq!(p.take(), vec![Action::Rescan]);
        p.retry(s("busy.md"), t0, Duration::from_millis(500));
        assert_eq!(p.wait(t0), Some(Duration::from_millis(500)));
        assert_eq!(p.take(), vec![Action::Index(s("busy.md"))]);
    }

    #[test]
    fn a_finished_read_is_records_an_overflow_or_a_broken_watch() {
        assert_eq!(completion_kind(Ok(48)), Completion::Records(48));
        assert_eq!(completion_kind(Ok(0)), Completion::Overflow);
        // ERROR_NOTIFY_ENUM_DIR
        assert_eq!(completion_kind(Err(1022)), Completion::Overflow);
        // ERROR_ACCESS_DENIED, what a deleted folder reports
        assert_eq!(completion_kind(Err(5)), Completion::Failed(5));
    }

    #[test]
    fn a_removed_folder_takes_everything_under_it_and_nothing_beside_it() {
        let row = |id: i64, rel: &str| DocumentRow {
            id,
            rel_path: rel.to_string(),
            size: None,
            mtime_unix: None,
            content_hash: None,
            parser_ver: None,
            status: "indexed".into(),
            ingested_unix: None,
        };
        let rows = [
            row(1, "a.md"),
            row(2, "sub/b.md"),
            row(3, "sub/deep/c.md"),
            row(4, "subtle.md"),
            row(5, "sub.md"),
        ];
        assert_eq!(docs_under(&rows, "sub"), vec![2, 3]);
        assert_eq!(docs_under(&rows, "a.md"), vec![1]);
        assert_eq!(docs_under(&rows, "sub.md"), vec![5]);
        assert!(docs_under(&rows, "nope").is_empty());
        assert!(docs_under(&rows, "").is_empty(), "never the whole index");
    }

    // -- apply: the same per-file path as the scan, on a real folder ----------

    use crate::knowledge::ingest::GLOBAL_STORE_LOCK;
    use std::fs;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wingman-test-watch-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Later than any file in a test, so a file counts as settled.
    const FUT: i64 = 4_000_000_000;

    #[test]
    fn a_new_file_is_indexed_and_a_deleted_one_loses_its_rows() {
        let dir = scratch("apply-basic");
        let store = Store::in_memory();
        let root = store.ensure_root("C:\\apply-basic", 0).unwrap();
        fs::write(dir.join("notes.md"), "# Notes\nfrobnicate the widget\n").unwrap();
        let busy = apply(
            &store,
            root,
            &dir,
            &[Action::Discover(s("notes.md"))],
            FUT,
            &|| false,
        )
        .unwrap();
        assert!(busy.is_empty());
        assert_eq!(store.fts_hits("frobnicate").unwrap(), 1);

        fs::remove_file(dir.join("notes.md")).unwrap();
        apply(
            &store,
            root,
            &dir,
            &[Action::Remove(s("notes.md"))],
            FUT,
            &|| false,
        )
        .unwrap();
        assert!(store.chunks_of(root, "notes.md").unwrap().is_empty());
        assert_eq!(store.document_state(root, "notes.md").unwrap(), None);
        assert_eq!(store.fts_hits("frobnicate").unwrap(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remove_for_a_file_that_is_still_there_deletes_nothing() {
        let dir = scratch("apply-still-there");
        let store = Store::in_memory();
        let root = store.ensure_root("C:\\apply-still", 0).unwrap();
        fs::write(dir.join("a.md"), "# A\nkeepword\n").unwrap();
        apply(
            &store,
            root,
            &dir,
            &[Action::Discover(s("a.md"))],
            FUT,
            &|| false,
        )
        .unwrap();
        // The event said removed, but it was saved again before we looked.
        apply(
            &store,
            root,
            &dir,
            &[Action::Remove(s("a.md"))],
            FUT,
            &|| false,
        )
        .unwrap();
        assert_eq!(store.fts_hits("keepword").unwrap(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_renamed_file_keeps_its_row_and_chunks() {
        let dir = scratch("apply-rename");
        let store = Store::in_memory();
        let root = store.ensure_root("C:\\apply-rename", 0).unwrap();
        fs::write(dir.join("old.md"), "# Old\nrenamedword here\n").unwrap();
        apply(
            &store,
            root,
            &dir,
            &[Action::Discover(s("old.md"))],
            FUT,
            &|| false,
        )
        .unwrap();
        let before = store.chunks_of(root, "old.md").unwrap();
        let id_before = store.document_row(root, "old.md").unwrap().unwrap().id;

        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::rename(dir.join("old.md"), dir.join("sub").join("new.md")).unwrap();
        let rename = Action::Rename {
            from: s("old.md"),
            to: s("sub/new.md"),
        };
        apply(&store, root, &dir, &[rename], FUT, &|| false).unwrap();

        assert_eq!(store.document_state(root, "old.md").unwrap(), None);
        assert_eq!(store.chunks_of(root, "sub/new.md").unwrap(), before);
        let after = store.document_row(root, "sub/new.md").unwrap().unwrap();
        assert_eq!(after.id, id_before, "same row, not a re-index");
        assert_eq!(store.fts_hits("renamedword").unwrap(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deleted_folder_takes_its_files_and_a_new_folder_is_walked() {
        let dir = scratch("apply-dirs");
        let store = Store::in_memory();
        // The walk registers the real path, so the rows live under that root.
        let root = store.ensure_root(&dir.to_string_lossy(), 0).unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("a.md"), "# A\nsubfolderword\n").unwrap();
        fs::write(dir.join("top.md"), "# T\ntopword\n").unwrap();
        // Adding the folder is a Discover: its files were never seen.
        apply(
            &store,
            root,
            &dir,
            &[Action::Discover(s("sub"))],
            FUT,
            &|| false,
        )
        .unwrap();
        assert_eq!(store.fts_hits("subfolderword").unwrap(), 1);
        assert_eq!(store.fts_hits("topword").unwrap(), 1, "the walk covers all");
        // A change inside a folder (its own Modified event) is not a walk.
        let none = apply(&store, root, &dir, &[Action::Index(s("sub"))], FUT, &|| {
            false
        })
        .unwrap();
        assert!(none.is_empty());

        fs::remove_dir_all(dir.join("sub")).unwrap();
        apply(
            &store,
            root,
            &dir,
            &[Action::Remove(s("sub"))],
            FUT,
            &|| false,
        )
        .unwrap();
        assert_eq!(store.fts_hits("subfolderword").unwrap(), 0);
        assert_eq!(store.fts_hits("topword").unwrap(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rescan_catches_what_an_overflow_lost() {
        let dir = scratch("apply-rescan");
        let store = Store::in_memory();
        let root = store.ensure_root("C:\\apply-rescan", 0).unwrap();
        // Registered under the real path so the walk's root matches.
        let key = dir.to_string_lossy().into_owned();
        let root_real = store.ensure_root(&key, 0).unwrap();
        let _ = root;
        fs::write(dir.join("lost.md"), "# L\nlostevent\n").unwrap();
        apply(&store, root_real, &dir, &[Action::Rescan], FUT, &|| false).unwrap();
        assert_eq!(store.fts_hits("lostevent").unwrap(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_in_use_is_reported_busy_for_a_retry() {
        let dir = scratch("apply-busy");
        let store = Store::in_memory();
        let root = store.ensure_root("C:\\apply-busy", 0).unwrap();
        let path = dir.join("held.md");
        fs::write(&path, "# H\nheldword\n").unwrap();
        // An exclusive open: no sharing, so the scan's read fails with 32.
        let held = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&path)
                .unwrap()
        };
        let busy = apply(
            &store,
            root,
            &dir,
            &[Action::Discover(s("held.md"))],
            FUT,
            &|| false,
        )
        .unwrap();
        assert_eq!(busy, vec![s("held.md")]);
        let (status, _) = store.document_state(root, "held.md").unwrap().unwrap();
        assert_eq!(status, "pending");
        drop(held);
        let busy = apply(
            &store,
            root,
            &dir,
            &[Action::Discover(s("held.md"))],
            FUT,
            &|| false,
        )
        .unwrap();
        assert!(busy.is_empty());
        assert_eq!(store.fts_hits("heldword").unwrap(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wait_is_rounded_up_to_whole_milliseconds_and_never_zero_while_pending() {
        assert_eq!(wait_millis(None), INFINITE);
        assert_eq!(wait_millis(Some(Duration::from_micros(1))), 1);
        assert_eq!(wait_millis(Some(Duration::from_micros(1_001))), 2);
        assert_eq!(wait_millis(Some(Duration::from_millis(750))), 750);
        assert_eq!(wait_millis(Some(Duration::from_secs(3_600))), 60_000);
        assert_eq!(wait_millis(Some(Duration::ZERO)), 0, "already due");
    }

    #[test]
    fn a_stopped_apply_writes_nothing() {
        let dir = scratch("apply-stopped");
        let store = Store::in_memory();
        let root = store.ensure_root("C:\\apply-stopped", 0).unwrap();
        fs::write(dir.join("a.md"), "# A\nstoppedword\n").unwrap();
        apply(
            &store,
            root,
            &dir,
            &[Action::Discover(s("a.md"))],
            FUT,
            &|| true,
        )
        .unwrap();
        assert_eq!(store.fts_hits("stoppedword").unwrap(), 0);
        assert!(store.documents_in_root(root).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_short_name_from_windows_is_keyed_as_the_long_name() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows::Win32::Storage::FileSystem::GetShortPathNameW;
        let dir = scratch("short-names");
        let root = ingest::canonical_root(&dir).unwrap();
        fs::create_dir_all(root.join("a long folder")).unwrap();
        let file = root.join("a long folder").join("long notes file.md");
        fs::write(&file, "# N\nshortword\n").unwrap();
        let wide: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: NUL-terminated input, buffer length passed.
        let n = unsafe { GetShortPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf)) } as usize;
        if n == 0 {
            let _ = fs::remove_dir_all(&dir);
            return; // 8.3 names are switched off on this volume
        }
        let short = std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]));
        // The short form shortens the whole path (the temp dir too), so take
        // only the two components under the root.
        let parts: Vec<_> = short.components().collect();
        let short_rel = parts[parts.len() - 2..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if short_rel == "a long folder/long notes file.md" {
            // THEORY (unverified): 8.3 name creation is off for this volume.
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        assert_eq!(
            long_rel(&root, &short_rel),
            "a long folder/long notes file.md"
        );
        // An ordinary name, and one that is gone, come back as they were.
        assert_eq!(
            long_rel(&root, "a long folder/long notes file.md"),
            "a long folder/long notes file.md"
        );
        assert_eq!(
            long_rel(&root, "a long folder/missing.md"),
            "a long folder/missing.md"
        );
        assert_eq!(long_rel(&root, "nowhere/x.md"), "nowhere/x.md");
        let _ = fs::remove_dir_all(&dir);
    }

    // -- the live thread: real Win32 events -------------------------------------

    fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) -> Duration {
        let start = Instant::now();
        while start.elapsed() < limit {
            if done() {
                return start.elapsed();
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "timed out after {limit:?} waiting for: {what} (queued notice: {:?})",
            notice::take()
        );
    }

    #[test]
    fn the_live_watcher_indexes_a_dropped_file_and_forgets_a_deleted_one() {
        let _one_at_a_time = GLOBAL_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("live");
        let watcher = start(&dir.to_string_lossy())
            .unwrap()
            .expect("a thread starts for a set folder");
        let store = store::global().unwrap();
        let root = ingest::canonical_root(&dir).unwrap();
        // The thread registers the root as it starts.
        let root_id = {
            let key = root.to_string_lossy().into_owned();
            store.ensure_root(&key, 0).unwrap()
        };

        // The K1 observable: drop notes.md into the folder; it is indexed.
        fs::write(
            dir.join("notes.md"),
            "# Notes\nlivewatchword in the notes\n",
        )
        .unwrap();
        let took = wait_for("notes.md indexed", Duration::from_secs(10), || {
            !store.chunks_of(root_id, "notes.md").unwrap().is_empty()
        });
        println!("MEASURED: dropped file indexed after {took:?}");
        assert_eq!(store.fts_hits("livewatchword").unwrap(), 1);

        // Delete it: `chunk` has no rows for it.
        fs::remove_file(dir.join("notes.md")).unwrap();
        wait_for("notes.md forgotten", Duration::from_secs(10), || {
            store.chunks_of(root_id, "notes.md").unwrap().is_empty()
                && store.document_state(root_id, "notes.md").unwrap().is_none()
        });
        assert_eq!(store.fts_hits("livewatchword").unwrap(), 0);

        // Quit: the thread ends promptly, with nothing pending to flush.
        let stop_at = Instant::now();
        assert!(watcher.is_running(), "still watching until it is stopped");
        let thread_done = drop_and_wait(watcher);
        println!(
            "MEASURED: watcher thread ended {:?} after the stop",
            stop_at.elapsed()
        );
        assert!(thread_done, "the thread must end after the stop event");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Drops the watcher (which sets the stop event) and waits for its thread
    /// by watching a clone of the join state through `is_running` beforehand.
    fn drop_and_wait(watcher: Watcher) -> bool {
        let stop = Arc::clone(&watcher.stop);
        drop(watcher);
        // The Watcher is gone; the thread's own clone of the event is the only
        // other owner, so the strong count falls back to 1 when it ends.
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if Arc::strong_count(&stop) == 1 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_folder_deleted_while_watched_stops_the_watch_with_a_notice_and_keeps_the_index() {
        let _one_at_a_time = GLOBAL_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("live-gone");
        fs::write(dir.join("keep.md"), "# K\ngoneprotectword\n").unwrap();
        let store = store::global().unwrap();
        // Index it first, through the same entry the app uses.
        let root = ingest::canonical_root(&dir).unwrap();
        let root_id = store.ensure_root(&root.to_string_lossy(), 0).unwrap();
        ingest::scan_folder(&store, &root, ingest::now_unix()).unwrap();
        assert_eq!(store.fts_hits("goneprotectword").unwrap(), 1);

        let _ = notice::take();
        let watcher = start(&dir.to_string_lossy()).unwrap().unwrap();
        // Let the thread arm its read before the folder goes.
        std::thread::sleep(Duration::from_millis(300));
        fs::remove_dir_all(&dir).unwrap();
        assert!(
            watcher.wait_stopped(Duration::from_secs(10)),
            "the watch must end when its folder is deleted"
        );
        let text = notice::take().expect("a card-bound notice");
        assert!(text.contains("no longer available"), "{text}");
        assert_eq!(
            store.fts_hits("goneprotectword").unwrap(),
            1,
            "a missing folder must not empty the index"
        );
        assert!(store.document_row(root_id, "keep.md").unwrap().is_some());
    }

    #[test]
    fn nothing_is_watched_for_an_empty_or_unusable_setting() {
        assert!(start("").unwrap().is_none());
        assert!(start("Notes").unwrap().is_none(), "a relative path");
        assert!(start(r"C:\").unwrap().is_none(), "a whole drive");
    }

    #[test]
    fn the_notices_are_plain_sentences() {
        for text in [
            failed_to_start("no handles"),
            gone_notice(Path::new(r"C:\Notes")),
        ] {
            assert!(!text.contains('\u{2014}'), "{text}");
        }
        assert!(gone_notice(Path::new(r"C:\Notes")).contains(r"C:\Notes"));
    }
}
