//! The user's current text selection (#28, expansion plan §7's `inputs/
//! selection.rs` row: "selected text via UI Automation `TextPattern`,
//! fallback synthetic Ctrl+C with byte-exact clipboard restore" / "must not:
//! leave the clipboard changed").
//!
//! Two layers, same split as `inputs::uia` and for the same reason (AGENTS.md
//! rule 8: pure logic is unit-tested, Win32 is checked by hand and the check
//! is named):
//!
//! - **Pure** (top of this file): which path to take given a UIA probe
//!   result ([`plan_from_probe`]), bounded truncation ([`truncate_bounded`]),
//!   which stray modifiers to release before injecting Ctrl+C
//!   ([`modifier_release_plan`], [`build_ctrl_c_plan`]), and the clipboard
//!   snapshot/restore/verify logic ([`snapshot`], [`restore_and_verify`],
//!   [`ClipboardGuard`]) against the injectable [`RawClipboard`] trait. None
//!   of this touches a `windows` COM or Win32 type, so it is exercised with
//!   plain Rust values and a fake clipboard, no OS clipboard or COM
//!   apartment involved.
//! - **Win32** ([`com`], [`win32`]): the real UI Automation calls
//!   (`GetFocusedElement`, `GetCurrentPattern(TextPattern2` then
//!   `TextPattern)`, `GetSelection`, range `GetText`) and the real clipboard
//!   backend (`OpenClipboard`/`GetClipboardData`/`SetClipboardData` over
//!   `HGLOBAL` byte buffers) plus the `SendInput` Ctrl+C injection and the
//!   event-driven wait for `WM_CLIPBOARDUPDATE`.
//!
//! # Primary path: UIA `TextPattern`
//!
//! [`probe_focused`] (production) / [`probe_element`] (the integration
//! test's seam, see its doc comment) reads the focused element's
//! `IsPassword` property first -- if true, the whole operation stops right
//! there ([`SelectionPlan::SkipPasswordField`]): neither the UIA read nor
//! the clipboard fallback ever touches a password field's contents, because
//! the fallback's synthetic Ctrl+C would copy the same sensitive text the
//! UIA path was told to skip. Otherwise it tries `TextPattern2` first (the
//! task brief's preference), falling back to plain `TextPattern` --
//! `IUIAutomationTextPattern2` extends `IUIAutomationTextPattern` in the UIA
//! COM interface hierarchy, so once either pattern object is in hand,
//! `.cast::<IUIAutomationTextPattern>()` reaches the same `GetSelection`/
//! `GetText` calls regardless of which one was actually returned.
//!
//! # Fallback path: clipboard-safe Ctrl+C
//!
//! Used when there is no focused element, no `TextPattern`, or the UIA
//! selection came back empty (nothing selected via UIA does not
//! necessarily mean nothing is selected -- plenty of controls, including
//! most browser-rendered text, do not implement `TextPattern` at all).
//!
//! 1. **Release stray modifiers.** The task's own pitfall: the Copilot key
//!    is `Win+Shift+F23`, and `hotkey.rs`'s workaround for the Win-key
//!    release problem exists because Windows can still think Win (and
//!    whatever else was held for the chord) is logically down right after
//!    the hook swallows it. [`modifier_release_plan`] reads which of
//!    Win/Shift/Alt/Ctrl currently read as down and emits synthetic keyups
//!    for them (both Win keys when `win` is set, since a `GetAsyncKeyState`
//!    read cannot tell which physical Win key it was) before pressing our
//!    own fresh Ctrl+C, so the injected combo is never accidentally
//!    Win+Ctrl+C or Shift+Ctrl+C.
//! 2. **Snapshot the clipboard.** [`ClipboardGuard::capture`] reads back
//!    whatever is on the clipboard *right now*, byte-exact, for every
//!    format this module knows how to restore -- see "What is and is not
//!    preserved" below.
//! 3. **Inject Ctrl+C**, `dwExtraInfo` tagged with
//!    [`crate::hotkey::INJECTED_MARKER`] (issue #209: `hotkey.rs`'s
//!    `hook_proc` now reads this tag and ignores anything carrying it,
//!    rather than treating the injected Ctrl+C as a real keypress).
//! 4. **Wait, event-driven, bounded.** [`win32::wait_for_clipboard_update`]
//!    registers `AddClipboardFormatListener` on a message-only window and
//!    blocks on `MsgWaitForMultipleObjects` up to the caller's budget,
//!    rather than a sleep-polling loop (AGENTS.md rule 5).
//! 5. **Read `CF_UNICODETEXT`**, then **always restore** via
//!    [`ClipboardGuard`]'s explicit `restore_now` on the success path, and
//!    its `Drop` impl as the safety net for every other path (timeout,
//!    error, an early `?`) -- AGENTS.md rule 7's "every failure ends in a
//!    card", never a modified clipboard.
//!
//! ## What is and is not preserved
//!
//! Snapshotted and restored byte-exact, via `HGLOBAL` buffers copied
//! straight off the real clipboard with no reinterpretation:
//! `CF_UNICODETEXT`, `CF_HDROP`, `CF_DIB`, `CF_DIBV5` (#265: a distinct
//! registered format from `CF_DIB`, carrying a `BITMAPV5HEADER`, that
//! several common copy sources -- Snipping Tool/Snip & Sketch, some browser
//! image copies -- put on the clipboard, sometimes with no parallel
//! `CF_DIB` entry), the registered `"HTML Format"` and `"Rich Text Format"`
//! formats.
//!
//! **`CF_BITMAP` is NOT byte-copied.** It is a GDI bitmap *handle*, not an
//! `HGLOBAL` buffer -- there is no byte buffer to snapshot without decoding
//! and re-encoding a bitmap, which this module does not attempt. In
//! practice this rarely loses anything: an application that puts an image
//! on the clipboard conventionally also offers `CF_DIB` (delayed-rendering
//! `CF_BITMAP` is usually synthesized by Windows from `CF_DIB` on demand for
//! whichever reader asks for it specifically), and `CF_DIB` *is* preserved
//! here. A clipboard holding `CF_BITMAP` with no `CF_DIB` alongside it (rare
//! in practice) loses that image across a selection-fallback round trip --
//! `THEORY (unverified)`, not measured against a real application that does
//! this.
//!
//! Any other format on the clipboard (a custom app-specific format, a file
//! contents stream, etc.) is left untouched by `EmptyClipboard` +
//! `SetClipboardData`'s all-or-nothing semantics: **`EmptyClipboard` clears
//! every format**, so anything not in the five above is unconditionally lost
//! across this round trip, not merely "not preserved". This is the honest
//! limit of a "known formats" snapshot approach; see the filed follow-up
//! finding for the alternative (a full `IDataObject` capture) this module
//! does not attempt.
//!
//! # Not wired yet
//!
//! Same status `inputs::uia` had until #27 landed: most public items below
//! are exercised only by this file's own tests. [`get_selection_foreground`]
//! and [`get_selection_foreground_with_target`] ARE wired, via
//! `actions::review_email::capture_input` (#38, #219) -- wiring a real key
//! press or palette chip directly to this module for some other action is
//! still a later issue.
//!
//! # Selection identity and offsets (#219)
//!
//! [`get_selection_foreground`] returns text only -- no element identity, so
//! nothing can write back through it. [`get_selection_foreground_with_target`]
//! is the same capture plus, when the UIA `TextPattern` path (not the
//! clipboard fallback) finds exactly one contiguous selection range, a
//! [`SelectionTarget`]: the owning element's identity (mirroring
//! `executors::replace_text::TargetRef`'s fields byte for byte -- that type
//! is private to the `executors` module tree, so this is a deliberate,
//! independent twin, the same "duplicated, not shared" call
//! `actions::review_email::CapturedTarget`'s own doc comment already makes),
//! the element's whole current text, and the selection's own UTF-16
//! `(start, end)` code-unit offsets into it, read from the same
//! `IUIAutomationTextRange` the selection came from
//! (`DocumentRange().GetText`/`MoveEndpointByRange` to find where the
//! selection starts, never a byte offset or a char count -- see
//! `executors::replace_text::splice_utf16`'s own doc comment for why that
//! distinction matters for a non-BMP character).
//!
//! [`Selection::target`] is `None` whenever a wrong offset would otherwise be
//! possible, never a guess: no caller-supplied `foreground_hwnd`, a
//! `ValuePattern`-only control with no `TextPattern` at all (already
//! [`UiaProbe::NoTextPattern`], degrading to the clipboard fallback, which
//! never carries identity either), or a discontiguous multi-range selection
//! (e.g. a multi-cell spreadsheet selection), which has no single `(start,
//! end)` pair to report. A password field is stopped even earlier
//! ([`UiaProbe::FocusedIsPassword`]) and never reaches any of this.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Pure types and decision logic
// ---------------------------------------------------------------------------

/// Bounded length for the text this module returns (task brief: "bounded
/// length (e.g. 20k chars, truncated flag)").
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub const DEFAULT_MAX_CHARS: usize = 20_000;

/// How long the clipboard-fallback path waits for `WM_CLIPBOARDUPDATE` (or a
/// sequence-number change) after injecting Ctrl+C before giving up and
/// restoring the clipboard anyway. Generous enough for a slow app to finish
/// its own copy handler; still short enough that a card is never stuck
/// waiting on it (AGENTS.md rule 7).
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub const DEFAULT_CLIPBOARD_WAIT_BUDGET: Duration = Duration::from_millis(1500);

/// Result of probing the focused element for a `TextPattern` selection, the
/// seam between the Win32 layer and [`plan_from_probe`]. Constructed by
/// [`com::probe_from_element`] in production, and directly by this file's
/// pure tests with no COM involved.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiaProbe {
    /// No element currently has UIA focus (or the lookup itself failed --
    /// see [`get_selection_foreground`]'s doc comment on why a COM error
    /// here is treated the same as "no element", not as "stop entirely").
    NoFocusedElement,
    /// The focused element's `IsPassword` property is true. Checked BEFORE
    /// any pattern lookup: the real password text is never even read into
    /// this process for this element, the same defense-in-depth shape as
    /// `inputs::uia::com::extract`'s password handling.
    FocusedIsPassword,
    /// The element has neither `TextPattern2` nor `TextPattern`.
    NoTextPattern,
    /// A `TextPattern`/`TextPattern2` was found and `GetSelection` was
    /// read; `text` may be empty (nothing selected). `identity` is `Some`
    /// only when exactly one contiguous selection range was found and its
    /// owning element's identity/offsets were all read successfully -- see
    /// the module doc comment's "Selection identity and offsets" section.
    Selection {
        text: String,
        identity: Option<SelectionIdentity>,
    },
}

/// The UIA-derived half of a [`SelectionTarget`]: everything except the
/// caller-supplied `hwnd` (see [`SelectionTarget`]'s own doc comment for why
/// `hwnd` is never read from UIA here). Produced only by
/// [`com::probe_from_element`]'s single-contiguous-range case.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionIdentity {
    pub runtime_id: Vec<i32>,
    pub automation_id: String,
    pub name: String,
    pub control_type: String,
    /// The element's whole current text at capture time
    /// (`TextPattern.DocumentRange().GetText(-1)`), the same "full text"
    /// role `executors::replace_text::ReplaceTextProposal::expected_current_text`
    /// plays: the stale-target check's baseline and the base string
    /// `splice_utf16` splices into.
    pub full_text: String,
    /// UTF-16 code-unit offsets `(start, end)` of the selection within
    /// `full_text` -- never byte offsets, never char counts.
    pub start: usize,
    pub end: usize,
}

/// Identifies the UIA element a captured selection lives in, plus the
/// selection's own UTF-16 `(start, end)` offsets into its whole current
/// text -- everything `executors::replace_text`'s `ReplaceSelection` mode
/// needs to re-resolve the element and splice the edited text back in.
/// `hwnd` is supplied by the caller (the foreground window handle it
/// already has, e.g. from `GetForegroundWindow()`), never read from UIA:
/// `IUIAutomationElement::GetFocusedElement()` reports identity relative to
/// the desktop, not a window, the same reason
/// `actions::review_email::com::capture_focused_compose_body` takes its
/// `hwnd: isize` as a parameter instead of deriving one.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionTarget {
    pub hwnd: isize,
    pub runtime_id: Vec<i32>,
    pub automation_id: String,
    pub name: String,
    pub control_type: String,
    pub full_text: String,
    pub start: usize,
    pub end: usize,
}

/// Combines a UIA-derived [`SelectionIdentity`] with the caller-supplied
/// foreground window handle into the [`SelectionTarget`] [`Selection`]
/// exposes. Pure and total: `None` in, `None` out, on either side -- kept
/// separate from [`resolve`] so this combination has a plain unit test
/// independent of any live UIA call.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
fn combine_target(
    foreground_hwnd: Option<isize>,
    identity: Option<SelectionIdentity>,
) -> Option<SelectionTarget> {
    let hwnd = foreground_hwnd?;
    let identity = identity?;
    Some(SelectionTarget {
        hwnd,
        runtime_id: identity.runtime_id,
        automation_id: identity.automation_id,
        name: identity.name,
        control_type: identity.control_type,
        full_text: identity.full_text,
        start: identity.start,
        end: identity.end,
    })
}

/// What [`get_selection_foreground`] (or its by-handle test seam) should do
/// next, decided purely from a [`UiaProbe`]. Kept separate from `UiaProbe`
/// itself so "empty selection falls back" and "over-length selection
/// truncates" are both decided here, in one pure function, rather than
/// duplicated between production code and tests.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionPlan {
    /// Use the UIA text as-is (already bounded). `identity` carries straight
    /// through from [`UiaProbe::Selection`] unexamined -- see [`resolve`]'s
    /// use of [`combine_target`] for where it becomes a real
    /// [`SelectionTarget`] (or is discarded).
    UseUia {
        text: String,
        truncated: bool,
        identity: Option<SelectionIdentity>,
    },
    /// The focused element is a password field: stop entirely, no clipboard
    /// fallback either.
    SkipPasswordField,
    /// No usable UIA selection: try the clipboard fallback.
    Fallback,
}

/// The single decision point for "which path" (tests-first: this is the
/// observable the first test in this module asserts). `NoFocusedElement`
/// and `NoTextPattern` both fall back for the same reason: neither means
/// "nothing is selected", only "UIA cannot tell us" -- see the module doc
/// comment. An empty [`UiaProbe::Selection`] falls back too, since an empty
/// UIA selection is indistinguishable from "this control doesn't really
/// support `TextPattern` in a way that reports selections" (measured against
/// several real controls during design; not asserted here since it would
/// require a live app).
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub fn plan_from_probe(probe: UiaProbe, max_chars: usize) -> SelectionPlan {
    match probe {
        UiaProbe::FocusedIsPassword => SelectionPlan::SkipPasswordField,
        UiaProbe::NoFocusedElement | UiaProbe::NoTextPattern => SelectionPlan::Fallback,
        // Discards `identity` too, even if one was somehow computed for an
        // empty range: an empty selection falls back to the clipboard path
        // exactly as before, never to a "Do it" target with a zero-width
        // range (#219's "must degrade... not a wrong offset").
        UiaProbe::Selection { text, .. } if text.is_empty() => SelectionPlan::Fallback,
        UiaProbe::Selection { text, identity } => {
            let (text, truncated) = truncate_bounded(&text, max_chars);
            SelectionPlan::UseUia {
                text,
                truncated,
                identity,
            }
        }
    }
}

/// #263: resolves a `CurrentIsPassword` read to a plain `bool`, failing
/// CLOSED (treated as a password field) rather than open when the read
/// itself errors. This module's own doc comment says the whole point of
/// checking `IsPassword` first is that "neither the UIA read nor the
/// clipboard fallback ever touches a password field's contents" -- a COM
/// error on the read itself (a hung provider, a non-conformant control)
/// must not silently take the less-safe "not a password" branch. Generic
/// over the error type so this stays in the pure, no-`windows`-crate-types
/// section and gets a plain unit test with no COM call involved;
/// `com::probe_from_element` is the sole caller.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
fn resolve_is_password<E>(read: Result<bool, E>) -> bool {
    read.unwrap_or(true)
}

/// Truncates `text` to at most `max_chars` Unicode scalar values (never a
/// byte count -- see the neighbouring test with multi-byte characters),
/// returning the truncated flag the task brief asks for. `total ==
/// max_chars` keeps everything and is NOT truncated, matching
/// `inputs::uia::bounded_count`'s inclusive-boundary convention.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub fn truncate_bounded(text: &str, max_chars: usize) -> (String, bool) {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    let truncated = chars.next().is_some();
    (head, truncated)
}

/// Decodes a raw `CF_UNICODETEXT` byte buffer (UTF-16LE, NUL-terminated) as
/// read straight off the clipboard's `HGLOBAL`, the same shape
/// [`win32::Win32Clipboard::get_format`] returns. Pure: no COM, no handles,
/// just bytes in, `String` out, so the neighbouring cases (no terminator,
/// empty buffer, an odd byte left over) are unit-tested without touching the
/// real clipboard.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub fn decode_unicode_text(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
        .collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// Which of Win/Shift/Alt/Ctrl currently read as logically down, as
/// [`win32::current_modifier_state`] observes via `GetAsyncKeyState` --
/// mirrors `hotkey::current_chord`'s modifier read, kept as a plain struct
/// here (rather than reusing `hotkey::Chord`, which also carries a trigger
/// `vk` this module has no use for) so this file's pure functions stay free
/// of any dependency on `hotkey.rs`.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModifierState {
    pub win: bool,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// Virtual-key codes, standard Windows values (not imported from `windows`
/// so this section of the file stays free of any Win32 type).
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_SHIFT: u32 = 0x10;
const VK_MENU: u32 = 0x12; // Alt
const VK_CONTROL: u32 = 0x11;
const VK_C: u32 = 0x43;

/// Which synthetic keyups to inject, in this fixed order, before pressing
/// our own fresh Ctrl+C -- the task brief's "release Win/Shift from the
/// hotkey first" pitfall, generalized to every modifier `GetAsyncKeyState`
/// still reports down. Win is released as BOTH `VK_LWIN` and `VK_RWIN`
/// unconditionally when `state.win` is set: `GetAsyncKeyState(VK_LWIN) ||
/// GetAsyncKeyState(VK_RWIN)` (the same read `hotkey::current_chord` uses)
/// cannot tell which physical key it was, and releasing a key that was
/// never down is a harmless no-op keyup, not an error.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub fn modifier_release_plan(state: ModifierState) -> Vec<u32> {
    let mut plan = Vec::new();
    if state.win {
        plan.push(VK_LWIN);
        plan.push(VK_RWIN);
    }
    if state.shift {
        plan.push(VK_SHIFT);
    }
    if state.alt {
        plan.push(VK_MENU);
    }
    if state.ctrl {
        plan.push(VK_CONTROL);
    }
    plan
}

/// One synthetic key event in [`build_ctrl_c_plan`]'s output.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntheticKeyEvent {
    pub vk: u32,
    pub key_up: bool,
}

/// The full ordered list of synthetic key events for one clipboard-fallback
/// attempt: every keyup in `release` (in the order given), then a fresh
/// Ctrl-down, C-down, C-up, Ctrl-up. Pure so the shape is asserted without
/// ever calling `SendInput` (this module's tests never do -- see the module
/// doc comment and [`win32::inject_events`]'s doc comment for why).
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub fn build_ctrl_c_plan(release: &[u32]) -> Vec<SyntheticKeyEvent> {
    let mut plan: Vec<SyntheticKeyEvent> = release
        .iter()
        .map(|&vk| SyntheticKeyEvent { vk, key_up: true })
        .collect();
    plan.push(SyntheticKeyEvent {
        vk: VK_CONTROL,
        key_up: false,
    });
    plan.push(SyntheticKeyEvent {
        vk: VK_C,
        key_up: false,
    });
    plan.push(SyntheticKeyEvent {
        vk: VK_C,
        key_up: true,
    });
    plan.push(SyntheticKeyEvent {
        vk: VK_CONTROL,
        key_up: true,
    });
    plan
}

// ---------------------------------------------------------------------------
// Clipboard snapshot / restore, injectable
// ---------------------------------------------------------------------------

/// Seam between this module's snapshot/restore logic and the real OS
/// clipboard (see [`win32::Win32Clipboard`]) or a test fake. Deliberately
/// small: three operations, all format-agnostic, so a fake needs no Win32
/// knowledge at all (the same shape rule 9 asks for -- a test that touches
/// the real shared clipboard is flaky and can clobber whatever the
/// developer had copied, `executors::clipboard`'s `ClipboardAccess` doc
/// comment says the identical thing about the single-format case).
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub trait RawClipboard {
    /// `GetClipboardSequenceNumber()`: bumped by Windows on every clipboard
    /// write, including ours -- the mechanism [`restore_and_verify`] uses to
    /// confirm a restore actually landed rather than silently no-op'ing.
    fn sequence_number(&self) -> u32;
    /// The raw bytes currently on the clipboard for `format`, or `None` if
    /// that format is not present. Byte-exact: no reinterpretation, so a
    /// caller that wants a specific format's meaning (e.g. UTF-16 text)
    /// decodes it separately (see [`decode_unicode_text`]).
    fn get_format(&self, format: u32) -> Option<Vec<u8>>;
    /// Atomically replaces the ENTIRE clipboard with exactly these formats
    /// (empty `formats` clears the clipboard). This is `EmptyClipboard` +
    /// `SetClipboardData` semantics, not a merge -- see the module doc
    /// comment's "what is and is not preserved" for why that matters.
    fn set_formats(&self, formats: &[(u32, Vec<u8>)]) -> anyhow::Result<()>;
}

/// One point-in-time capture of every format in `formats_of_interest` that
/// was actually present.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipboardSnapshot {
    formats: Vec<(u32, Vec<u8>)>,
}

/// Captures whichever of `formats_of_interest` are currently on the
/// clipboard. A format absent from the source clipboard is simply absent
/// from the snapshot (not a zero-length entry) -- [`restore`] then replaces
/// the clipboard with exactly this shorter list, which is correct: a
/// clipboard that never had `CF_HDROP` must not gain an empty `CF_HDROP`
/// entry on restore.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub fn snapshot(clipboard: &impl RawClipboard, formats_of_interest: &[u32]) -> ClipboardSnapshot {
    let formats = formats_of_interest
        .iter()
        .filter_map(|&fmt| clipboard.get_format(fmt).map(|bytes| (fmt, bytes)))
        .collect();
    ClipboardSnapshot { formats }
}

/// Writes `snapshot` back to the clipboard, replacing whatever is there.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub fn restore(clipboard: &impl RawClipboard, snapshot: &ClipboardSnapshot) -> anyhow::Result<()> {
    clipboard.set_formats(&snapshot.formats)
}

/// [`restore`], then confirms the write actually happened: the sequence
/// number read immediately before this call must differ from the one read
/// immediately after. Comparing against `snapshot`'s own capture-time
/// sequence number would be wrong -- restoring necessarily leaves the
/// clipboard's sequence number higher than it was at capture time, that is
/// expected and not a failure; what must never happen is a restore that
/// silently touches nothing.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub fn restore_and_verify(
    clipboard: &impl RawClipboard,
    snapshot: &ClipboardSnapshot,
) -> anyhow::Result<()> {
    let before = clipboard.sequence_number();
    restore(clipboard, snapshot)?;
    let after = clipboard.sequence_number();
    anyhow::ensure!(
        after != before,
        "clipboard restore did not change the sequence number; the clipboard may still hold \
         the injected copy"
    );
    Ok(())
}

/// RAII guard: captures the clipboard on construction, and guarantees a
/// restore attempt no matter how the caller's scope ends (AGENTS.md rule 7 /
/// the task brief: "Never leave the user's clipboard modified on any path").
/// [`ClipboardGuard::restore_now`] is the primary, verified path; `Drop` is
/// the safety net for every path that does not reach it (an early `?`, a
/// panic unwind, a timeout branch that forgot to call it) -- best-effort
/// there, since `Drop` cannot propagate a `Result`.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub struct ClipboardGuard<'a, C: RawClipboard> {
    clipboard: &'a C,
    snapshot: ClipboardSnapshot,
    done: bool,
}

#[allow(dead_code)] // see the module doc comment's "not wired yet"
impl<'a, C: RawClipboard> ClipboardGuard<'a, C> {
    pub fn capture(clipboard: &'a C, formats_of_interest: &[u32]) -> Self {
        Self {
            clipboard,
            snapshot: snapshot(clipboard, formats_of_interest),
            done: false,
        }
    }

    /// Restores and verifies now. Idempotent on SUCCESS: a second call after
    /// a successful restore (including the one `Drop` would otherwise make)
    /// is a no-op `Ok(())`. On FAILURE, `done` is left `false` so `Drop`
    /// still gets its documented one more attempt (#262: a failed restore
    /// must not forfeit the safety net at exactly the moment it is needed).
    pub fn restore_now(&mut self) -> anyhow::Result<()> {
        if self.done {
            return Ok(());
        }
        let result = restore_and_verify(self.clipboard, &self.snapshot);
        self.done = result.is_ok();
        result
    }
}

impl<'a, C: RawClipboard> Drop for ClipboardGuard<'a, C> {
    fn drop(&mut self) {
        if !self.done {
            // Best-effort: Drop cannot return a Result, and there is no
            // useful recovery from a failed restore during unwind anyway.
            let _ = restore_and_verify(self.clipboard, &self.snapshot);
            self.done = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Public result type
// ---------------------------------------------------------------------------

/// Where the returned text came from.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionSource {
    Uia,
    ClipboardFallback,
    /// Neither path found any selected text.
    Empty,
    /// The focused element is a password field; the whole operation was
    /// skipped and `text` is always empty.
    SkippedPasswordField,
}

#[allow(dead_code)] // see the module doc comment's "not wired yet"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub text: String,
    pub truncated: bool,
    pub source: SelectionSource,
    /// `Some` only when [`get_selection_foreground_with_target`] was called
    /// (a `foreground_hwnd` was supplied) AND the UIA path found exactly one
    /// contiguous selection range with everything readable -- see the
    /// module doc comment's "Selection identity and offsets" section. Always
    /// `None` from plain [`get_selection_foreground`], and always `None` for
    /// [`SelectionSource::ClipboardFallback`]/`Empty`/`SkippedPasswordField`.
    pub target: Option<SelectionTarget>,
}

// ---------------------------------------------------------------------------
// Win32 / UI Automation
// ---------------------------------------------------------------------------

mod com {
    #![allow(dead_code)] // see the module doc comment's "not wired yet"

    use super::{resolve_is_password, SelectionIdentity, UiaProbe};
    use windows::core::Interface;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, SAFEARRAY,
    };
    use windows::Win32::System::Ole::{
        SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
        SafeArrayUnaccessData,
    };
    use windows::Win32::UI::Accessibility::{
        IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationTextRange,
        TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start, UIA_TextPattern2Id,
        UIA_TextPatternId,
    };

    /// Same RAII pairing as `inputs::uia::com::ComApartment`, duplicated
    /// rather than shared: that type is private to `uia.rs` and this task's
    /// scope is this file plus one `mod.rs` line, not a cross-module
    /// refactor.
    struct ComApartment;

    impl ComApartment {
        fn enter() -> anyhow::Result<Self> {
            let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            hr.ok()?;
            Ok(Self)
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    fn is_null<I: Interface>(i: &I) -> bool {
        i.as_raw().is_null()
    }

    /// Probes the current desktop focus -- the real production path.
    pub(super) fn probe_focused() -> anyhow::Result<UiaProbe> {
        let _apartment = ComApartment::enter()?;
        let automation: IUIAutomation =
            unsafe { crate::inputs::uia_automation::create_automation() }?;
        let element = crate::inputs::uia_automation::describe_timeout(
            unsafe { automation.GetFocusedElement() },
            "GetFocusedElement",
        )?;
        if is_null(&element) {
            return Ok(UiaProbe::NoFocusedElement);
        }
        probe_from_element(&element)
    }

    /// Probes a specific window's element directly via `ElementFromHandle`,
    /// bypassing real desktop focus entirely -- the integration test's seam
    /// (see the win32 test module's doc comment), the same shape
    /// `inputs::uia::snapshot_hwnd` uses relative to
    /// `inputs::uia::snapshot_foreground`.
    pub(super) fn probe_element(hwnd: HWND) -> anyhow::Result<UiaProbe> {
        let _apartment = ComApartment::enter()?;
        let automation: IUIAutomation =
            unsafe { crate::inputs::uia_automation::create_automation() }?;
        let element = crate::inputs::uia_automation::describe_timeout(
            unsafe { automation.ElementFromHandle(hwnd) },
            "ElementFromHandle",
        )?;
        if is_null(&element) {
            return Ok(UiaProbe::NoFocusedElement);
        }
        probe_from_element(&element)
    }

    /// Shared logic once an element is in hand: password check, then
    /// `TextPattern2`-else-`TextPattern`, then `GetSelection`.
    fn probe_from_element(element: &IUIAutomationElement) -> anyhow::Result<UiaProbe> {
        // NEVER read anything else from a password field's selection --
        // checked before any pattern lookup, same shape as
        // `inputs::uia::com::extract`'s `is_password` guard. #263: a failed
        // read fails CLOSED via `resolve_is_password`, never open.
        let is_password =
            resolve_is_password(unsafe { element.CurrentIsPassword() }.map(|b| b.as_bool()));
        if is_password {
            return Ok(UiaProbe::FocusedIsPassword);
        }

        let Some(pattern) = text_pattern(element)? else {
            return Ok(UiaProbe::NoTextPattern);
        };

        let ranges = unsafe { pattern.GetSelection() }?;
        let count = unsafe { ranges.Length() }?.max(0) as usize;
        let mut text = String::new();
        // Only a single, contiguous range has one meaningful (start, end)
        // pair to report -- a discontiguous multi-range selection (e.g. a
        // multi-cell Excel selection) degrades to text-only, same as the
        // module doc comment's "Selection identity and offsets" section
        // says.
        let mut only_range: Option<IUIAutomationTextRange> = None;
        for i in 0..count {
            let range = unsafe { ranges.GetElement(i as i32) }?;
            let range_text = unsafe { range.GetText(-1) }?.to_string();
            if range_text.is_empty() {
                continue;
            }
            if !text.is_empty() {
                // Separates discontiguous ranges (e.g. a multi-cell Excel
                // selection) rather than concatenating them run-on.
                text.push('\n');
            }
            text.push_str(&range_text);
            if count == 1 {
                only_range = Some(range);
            }
        }

        // `.ok()`: any failure while reading identity/offsets (a provider
        // quirk, a COM hiccup) degrades to no target, never a wrong one --
        // the text itself is already captured above and is returned either
        // way.
        let identity =
            only_range.and_then(|range| selection_identity(element, &pattern, &range).ok());

        Ok(UiaProbe::Selection { text, identity })
    }

    /// Reads the owning element's identity plus the selection's own UTF-16
    /// `(start, end)` offsets, both from the same `IUIAutomationTextRange`
    /// [`probe_from_element`] already has in hand. The offset technique:
    /// clone the document's whole range, move its END to the selection's
    /// START (`MoveEndpointByRange`), then the UTF-16 length of THAT
    /// range's text is `start`; `end` is `start` plus the UTF-16 length of
    /// the selection's own text (already read by the caller as
    /// `range_text`, but re-read here via `GetText` again rather than
    /// threaded through, since a second `GetText` call on the same
    /// unmodified range is cheap and keeps this function's inputs/outputs
    /// self-contained). Never a byte offset or a char count -- see
    /// `executors::replace_text::splice_utf16`'s own doc comment for why
    /// that distinction matters for a non-BMP character.
    fn selection_identity(
        element: &IUIAutomationElement,
        pattern: &IUIAutomationTextPattern,
        selection_range: &IUIAutomationTextRange,
    ) -> anyhow::Result<SelectionIdentity> {
        let document_range = unsafe { pattern.DocumentRange() }?;
        let before_range = unsafe { document_range.Clone() }?;
        unsafe {
            before_range.MoveEndpointByRange(
                TextPatternRangeEndpoint_End,
                selection_range,
                TextPatternRangeEndpoint_Start,
            )
        }?;
        let before_text = unsafe { before_range.GetText(-1) }?.to_string();
        let start = before_text.encode_utf16().count();

        let selection_text = unsafe { selection_range.GetText(-1) }?.to_string();
        let end = start + selection_text.encode_utf16().count();

        let full_text = unsafe { document_range.GetText(-1) }?.to_string();

        let name = unsafe { element.CurrentName() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let automation_id = unsafe { element.CurrentAutomationId() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let control_type = control_type_to_string(unsafe { element.CurrentControlType() }?);
        let runtime_id = unsafe { element.GetRuntimeId() }
            .ok()
            .and_then(|psa| unsafe { runtime_id_from_safearray(psa) }.ok())
            .unwrap_or_default();

        Ok(SelectionIdentity {
            runtime_id,
            automation_id,
            name,
            control_type,
            full_text,
            start,
            end,
        })
    }

    /// Same mapping as `actions::review_email::com::control_type_to_string`
    /// (duplicated, not shared -- see this file's module doc comment on
    /// `SelectionTarget` for why).
    fn control_type_to_string(id: windows::Win32::UI::Accessibility::UIA_CONTROLTYPE_ID) -> String {
        use windows::Win32::UI::Accessibility::{
            UIA_CheckBoxControlTypeId as CHECKBOX, UIA_ComboBoxControlTypeId as COMBOBOX,
            UIA_DocumentControlTypeId as DOCUMENT, UIA_EditControlTypeId as EDIT,
            UIA_ListControlTypeId as LIST, UIA_RadioButtonControlTypeId as RADIOBUTTON,
            UIA_TextControlTypeId as TEXT,
        };
        if id == EDIT {
            "Edit"
        } else if id == COMBOBOX {
            "ComboBox"
        } else if id == DOCUMENT {
            "Document"
        } else if id == CHECKBOX {
            "CheckBox"
        } else if id == RADIOBUTTON {
            "RadioButton"
        } else if id == LIST {
            "List"
        } else if id == TEXT {
            "Text"
        } else {
            "Other"
        }
        .to_string()
    }

    /// Same shape as `executors::target::com::runtime_id_from_safearray`
    /// (duplicated, not shared -- see this file's module doc comment on
    /// `SelectionTarget` for why).
    unsafe fn runtime_id_from_safearray(psa: *mut SAFEARRAY) -> anyhow::Result<Vec<i32>> {
        if psa.is_null() {
            return Ok(Vec::new());
        }

        struct SafeArrayGuard(*mut SAFEARRAY);
        impl Drop for SafeArrayGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = SafeArrayDestroy(self.0);
                }
            }
        }
        let _guard = SafeArrayGuard(psa);

        let lbound = unsafe { SafeArrayGetLBound(psa, 1) }?;
        let ubound = unsafe { SafeArrayGetUBound(psa, 1) }?;
        if ubound < lbound {
            return Ok(Vec::new());
        }
        let count = (ubound - lbound + 1) as usize;

        let mut data_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe { SafeArrayAccessData(psa, &mut data_ptr) }?;
        let slice = unsafe { std::slice::from_raw_parts(data_ptr as *const i32, count) };
        let result = slice.to_vec();
        unsafe { SafeArrayUnaccessData(psa) }?;

        Ok(result)
    }

    /// `TextPattern2` first (the task brief's preference: it is a superset),
    /// falling back to plain `TextPattern`. Both come back as
    /// `IUIAutomationTextPattern` because of the interface hierarchy (see
    /// the module doc comment) -- callers only ever need the one type.
    fn text_pattern(
        element: &IUIAutomationElement,
    ) -> anyhow::Result<Option<IUIAutomationTextPattern>> {
        let unknown = unsafe { element.GetCurrentPattern(UIA_TextPattern2Id) }
            .ok()
            .filter(|u| !is_null(u))
            .or(unsafe { element.GetCurrentPattern(UIA_TextPatternId) }
                .ok()
                .filter(|u| !is_null(u)));
        let Some(unknown) = unknown else {
            return Ok(None);
        };
        Ok(Some(unknown.cast()?))
    }
}

mod win32 {
    #![allow(dead_code)] // see the module doc comment's "not wired yet"

    use super::{RawClipboard, SyntheticKeyEvent};
    use crate::hotkey::INJECTED_MARKER;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    use windows::core::w;
    use windows::Win32::Foundation::{
        GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM,
    };
    use windows::Win32::System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
        GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW,
        RemoveClipboardFormatListener, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        MsgWaitForMultipleObjects, PeekMessageW, RegisterClassExW, TranslateMessage, HWND_MESSAGE,
        MSG, PM_REMOVE, QS_ALLINPUT, WNDCLASSEXW,
    };

    pub(super) const CF_UNICODETEXT_U32: u32 = CF_UNICODETEXT.0 as u32;
    pub(super) const CF_HDROP_U32: u32 = CF_HDROP.0 as u32;
    pub(super) const CF_DIB_U32: u32 = CF_DIB.0 as u32;
    /// #265: a distinct registered format from `CF_DIB` (carries a
    /// `BITMAPV5HEADER`, used for images with an embedded ICC profile or a
    /// real alpha channel). Several common copy sources (Snipping
    /// Tool/Snip & Sketch, some browser image copies) put this on the
    /// clipboard, sometimes with no parallel `CF_DIB` entry.
    pub(super) const CF_DIBV5_U32: u32 = CF_DIBV5.0 as u32;

    fn html_format() -> u32 {
        static FMT: OnceLock<u32> = OnceLock::new();
        *FMT.get_or_init(|| unsafe { RegisterClipboardFormatW(w!("HTML Format")) })
    }

    fn rtf_format() -> u32 {
        static FMT: OnceLock<u32> = OnceLock::new();
        *FMT.get_or_init(|| unsafe { RegisterClipboardFormatW(w!("Rich Text Format")) })
    }

    /// The six formats [`super::snapshot`]/[`super::restore`] round-trip.
    /// See the module doc comment's "what is and is not preserved".
    pub(super) fn preserved_formats() -> [u32; 6] {
        [
            CF_UNICODETEXT_U32,
            CF_HDROP_U32,
            CF_DIB_U32,
            CF_DIBV5_U32,
            html_format(),
            rtf_format(),
        ]
    }

    /// RAII pairing of `OpenClipboard`/`CloseClipboard`. `OpenClipboard` can
    /// transiently fail if another process holds the clipboard open (a very
    /// short window in practice); this retries a bounded number of times
    /// with a short sleep between attempts. This is NOT the idle-polling
    /// AGENTS.md rule 5 forbids -- it only runs during an actively
    /// in-progress, user-triggered clipboard operation, never while idle,
    /// and gives up (returning the last error) after a bounded number of
    /// tries rather than looping forever.
    struct OpenGuard;

    impl OpenGuard {
        fn open() -> anyhow::Result<Self> {
            const ATTEMPTS: u32 = 10;
            let mut last_err = None;
            for attempt in 0..ATTEMPTS {
                match unsafe { OpenClipboard(None) } {
                    Ok(()) => return Ok(Self),
                    Err(e) => {
                        last_err = Some(e);
                        if attempt + 1 < ATTEMPTS {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
            }
            Err(anyhow::anyhow!(
                "OpenClipboard failed after {ATTEMPTS} attempts: {}",
                last_err.expect("loop always sets last_err before exhausting ATTEMPTS")
            ))
        }
    }

    impl Drop for OpenGuard {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    fn read_hglobal_bytes(handle: HANDLE) -> Option<Vec<u8>> {
        if handle.0.is_null() {
            return None;
        }
        let hglobal = HGLOBAL(handle.0);
        let size = unsafe { GlobalSize(hglobal) };
        if size == 0 {
            return None;
        }
        let ptr = unsafe { GlobalLock(hglobal) };
        if ptr.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) }.to_vec();
        let _ = unsafe { GlobalUnlock(hglobal) };
        Some(bytes)
    }

    fn alloc_hglobal_bytes(bytes: &[u8]) -> anyhow::Result<HGLOBAL> {
        use windows::Win32::System::Memory::GMEM_MOVEABLE;
        // A zero-length GlobalAlloc is legal but GlobalLock on it is not
        // guaranteed useful; every format this module writes back was
        // itself read from a real clipboard entry, so `.max(1)` only
        // matters for a deliberately-empty test payload.
        let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) }?;
        let ptr = unsafe { GlobalLock(hglobal) };
        if ptr.is_null() {
            let _ = unsafe { GlobalFree(Some(hglobal)) };
            anyhow::bail!("GlobalLock failed while preparing clipboard data");
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len()) };
        let _ = unsafe { GlobalUnlock(hglobal) };
        Ok(hglobal)
    }

    /// The real OS clipboard, implementing [`RawClipboard`]. Analogous to
    /// `executors::clipboard::ArboardClipboard`, but over raw formats rather
    /// than `arboard`'s text-only API -- byte-exact multi-format restore is
    /// not something `arboard` exposes.
    pub(super) struct Win32Clipboard;

    impl RawClipboard for Win32Clipboard {
        fn sequence_number(&self) -> u32 {
            unsafe { GetClipboardSequenceNumber() }
        }

        fn get_format(&self, format: u32) -> Option<Vec<u8>> {
            let _guard = OpenGuard::open().ok()?;
            let handle = unsafe { GetClipboardData(format) }.ok()?;
            read_hglobal_bytes(handle)
        }

        fn set_formats(&self, formats: &[(u32, Vec<u8>)]) -> anyhow::Result<()> {
            let _guard = OpenGuard::open()?;
            unsafe { EmptyClipboard() }?;
            for (format, bytes) in formats {
                let hglobal = alloc_hglobal_bytes(bytes)?;
                // Ownership of hglobal transfers to the clipboard on
                // success; it must NOT be freed here either way -- on
                // success the clipboard now owns it, and on failure
                // EmptyClipboard/CloseClipboard already tore down the
                // clipboard's other entries, so leaking one HGLOBAL is
                // preferable to a double-free race with the OS.
                unsafe { SetClipboardData(*format, Some(HANDLE(hglobal.0))) }
                    .map_err(|e| anyhow::anyhow!("SetClipboardData(0x{format:X}) failed: {e}"))?;
            }
            Ok(())
        }
    }

    /// Reads live modifier state the same way `hotkey::current_chord` does
    /// (`GetAsyncKeyState`), as [`super::ModifierState`].
    pub(super) fn current_modifier_state() -> super::ModifierState {
        let down =
            |vk: VIRTUAL_KEY| (unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000) != 0;
        super::ModifierState {
            win: down(VK_LWIN) || down(VK_RWIN),
            shift: down(VK_SHIFT),
            alt: down(VK_MENU),
            ctrl: down(VK_CONTROL),
        }
    }

    fn to_input(ev: &SyntheticKeyEvent) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(ev.vk as u16),
                    wScan: 0,
                    dwFlags: if ev.key_up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    time: 0,
                    dwExtraInfo: INJECTED_MARKER,
                },
            },
        }
    }

    /// `SendInput`s the given plan. Never called by this module's own
    /// automated tests (see [`crate::hotkey::INJECTED_MARKER`]'s doc comment
    /// and the crate task brief: it would type into whatever real window has
    /// focus when the test runs). Covered by [`super::build_ctrl_c_plan`]'s
    /// pure tests plus the manual check filed to #166.
    pub(super) fn inject_events(events: &[SyntheticKeyEvent]) {
        let inputs: Vec<INPUT> = events.iter().map(to_input).collect();
        if inputs.is_empty() {
            return;
        }
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    }

    const LISTENER_CLASS: windows::core::PCWSTR = w!("Wingman.Inputs.SelectionClipboardListener");

    unsafe extern "system" fn listener_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn ensure_listener_class_registered() -> bool {
        static REGISTERED: std::sync::Once = std::sync::Once::new();
        static OK: OnceLock<bool> = OnceLock::new();
        REGISTERED.call_once(|| {
            let hinstance =
                unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
                    .map(|h| windows::Win32::Foundation::HINSTANCE(h.0));
            let ok = match hinstance {
                Ok(hinstance) => unsafe {
                    let wc = WNDCLASSEXW {
                        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                        lpfnWndProc: Some(listener_wndproc),
                        hInstance: hinstance,
                        lpszClassName: LISTENER_CLASS,
                        ..Default::default()
                    };
                    RegisterClassExW(&wc) != 0
                },
                Err(_) => false,
            };
            let _ = OK.set(ok);
        });
        OK.get().copied().unwrap_or(false)
    }

    /// Blocks (event-driven, no sleep-polling loop -- AGENTS.md rule 5)
    /// until either the clipboard's sequence number differs from
    /// `sequence_before` or `budget` elapses, whichever comes first. Returns
    /// whether a change was observed. A message-only listener window plus
    /// `AddClipboardFormatListener` gives the wait something to block on via
    /// `MsgWaitForMultipleObjects`; the sequence-number comparison (not the
    /// message content) is the actual detection, matching the task brief's
    /// "a single `GetClipboardSequenceNumber` check before/after a bounded
    /// wait on a message is acceptable" -- this also covers the race where
    /// the update already happened before the listener was registered.
    pub(super) fn wait_for_clipboard_update(budget: Duration, sequence_before: u32) -> bool {
        if unsafe { GetClipboardSequenceNumber() } != sequence_before {
            return true;
        }
        if !ensure_listener_class_registered() {
            return false;
        }
        let Ok(hwnd) = (unsafe {
            CreateWindowExW(
                Default::default(),
                LISTENER_CLASS,
                w!(""),
                Default::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }) else {
            return false;
        };
        let _ = unsafe { AddClipboardFormatListener(hwnd) };

        let deadline = Instant::now() + budget;
        let mut changed = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let wait = unsafe {
                MsgWaitForMultipleObjects(
                    None,
                    false,
                    remaining.as_millis().min(u32::MAX as u128) as u32,
                    QS_ALLINPUT,
                )
            };
            if wait != WAIT_OBJECT_0 {
                break; // timeout or error: give up, caller treats as "no change"
            }
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if unsafe { GetClipboardSequenceNumber() } != sequence_before {
                changed = true;
                break;
            }
        }

        let _ = unsafe { RemoveClipboardFormatListener(hwnd) };
        let _ = unsafe { DestroyWindow(hwnd) };
        changed
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// The real production entry point (not wired to `app.rs` yet -- see the
/// module doc comment). Runs `TextPattern` first; if that yields no usable
/// text, falls back to a clipboard-safe synthetic Ctrl+C. **Must be called
/// from a dedicated worker thread**, same requirement as
/// `inputs::uia::snapshot_foreground` (a hung UIA provider or a slow app's
/// own clipboard handling can block for a while; never call this from the
/// low-level keyboard hook's thread).
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub fn get_selection_foreground(
    max_chars: usize,
    clipboard_wait_budget: Duration,
) -> anyhow::Result<Selection> {
    // A COM/UIA error here (a hung provider, an element that vanished mid
    // call) is deliberately treated the same as "no usable selection", not
    // as "stop": the clipboard fallback needs no UIA element at all, and
    // there is no reason a UIA hiccup should prevent it -- EXCEPT that a
    // genuine `FocusedIsPassword` read must still short-circuit everything,
    // which is why that variant is never folded into this `unwrap_or`.
    let probe = com::probe_focused().unwrap_or(UiaProbe::NoFocusedElement);
    resolve(probe, max_chars, clipboard_wait_budget, None)
}

/// Same as [`get_selection_foreground`], plus a [`Selection::target`] when
/// the UIA path can support one (#219). `foreground_hwnd` is the caller's
/// already-known foreground window handle (e.g. `GetForegroundWindow()`),
/// the same value `actions::review_email::capture_input` already threads
/// through for the compose-body path -- see [`SelectionTarget`]'s doc
/// comment for why this module never derives it from UIA itself.
#[allow(dead_code)] // see the module doc comment's "not wired yet"
pub fn get_selection_foreground_with_target(
    foreground_hwnd: isize,
    max_chars: usize,
    clipboard_wait_budget: Duration,
) -> anyhow::Result<Selection> {
    let probe = com::probe_focused().unwrap_or(UiaProbe::NoFocusedElement);
    resolve(
        probe,
        max_chars,
        clipboard_wait_budget,
        Some(foreground_hwnd),
    )
}

fn resolve(
    probe: UiaProbe,
    max_chars: usize,
    clipboard_wait_budget: Duration,
    foreground_hwnd: Option<isize>,
) -> anyhow::Result<Selection> {
    match plan_from_probe(probe, max_chars) {
        SelectionPlan::UseUia {
            text,
            truncated,
            identity,
        } => Ok(Selection {
            text,
            truncated,
            source: SelectionSource::Uia,
            target: combine_target(foreground_hwnd, identity),
        }),
        SelectionPlan::SkipPasswordField => Ok(Selection {
            text: String::new(),
            truncated: false,
            source: SelectionSource::SkippedPasswordField,
            target: None,
        }),
        SelectionPlan::Fallback => clipboard_fallback(max_chars, clipboard_wait_budget),
    }
}

/// The clipboard-safe Ctrl+C fallback, end to end. See the module doc
/// comment's numbered steps. Never exercised by this file's automated tests
/// (see [`win32::inject_events`]'s doc comment); the pure steps
/// ([`modifier_release_plan`], [`build_ctrl_c_plan`], the
/// [`ClipboardGuard`]/[`RawClipboard`] pair) are.
fn clipboard_fallback(max_chars: usize, wait_budget: Duration) -> anyhow::Result<Selection> {
    let clipboard = win32::Win32Clipboard;
    let mut guard = ClipboardGuard::capture(&clipboard, &win32::preserved_formats());

    let modifiers = win32::current_modifier_state();
    let release = modifier_release_plan(modifiers);
    let plan = build_ctrl_c_plan(&release);
    let sequence_before = clipboard.sequence_number();
    win32::inject_events(&plan);

    let changed = win32::wait_for_clipboard_update(wait_budget, sequence_before);
    let text = if changed {
        clipboard
            .get_format(win32::CF_UNICODETEXT_U32)
            .map(|bytes| decode_unicode_text(&bytes))
    } else {
        None
    };

    // Always restore, success or not -- the explicit, verified path. `Drop`
    // remains the safety net if anything above this line returned early via
    // `?` (it cannot: nothing above does today, but the guard exists
    // precisely so that changes to this function keep that guarantee
    // without relying on every future edit remembering to restore by hand).
    guard.restore_now()?;

    match text {
        Some(text) if !text.is_empty() => {
            let (text, truncated) = truncate_bounded(&text, max_chars);
            Ok(Selection {
                text,
                truncated,
                source: SelectionSource::ClipboardFallback,
                target: None,
            })
        }
        _ => Ok(Selection {
            text: String::new(),
            truncated: false,
            source: SelectionSource::Empty,
            target: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- plan_from_probe: which path (tests-first's first observable) -----

    #[test]
    fn no_focused_element_falls_back() {
        assert_eq!(
            plan_from_probe(UiaProbe::NoFocusedElement, DEFAULT_MAX_CHARS),
            SelectionPlan::Fallback
        );
    }

    #[test]
    fn password_field_is_skipped_entirely_not_fallen_back_to() {
        assert_eq!(
            plan_from_probe(UiaProbe::FocusedIsPassword, DEFAULT_MAX_CHARS),
            SelectionPlan::SkipPasswordField
        );
    }

    #[test]
    fn resolve_is_password_fails_closed_when_the_read_itself_errors() {
        // #263: a COM error reading IsPassword must be treated as "this is
        // a password field", never as "this is not".
        assert!(resolve_is_password::<()>(Err(())));
    }

    #[test]
    fn resolve_is_password_reports_a_successful_read_unchanged() {
        assert!(!resolve_is_password::<()>(Ok(false)));
        assert!(resolve_is_password::<()>(Ok(true)));
    }

    #[test]
    fn no_text_pattern_falls_back() {
        assert_eq!(
            plan_from_probe(UiaProbe::NoTextPattern, DEFAULT_MAX_CHARS),
            SelectionPlan::Fallback
        );
    }

    #[test]
    fn empty_uia_selection_falls_back() {
        assert_eq!(
            plan_from_probe(
                UiaProbe::Selection {
                    text: String::new(),
                    identity: None
                },
                DEFAULT_MAX_CHARS
            ),
            SelectionPlan::Fallback
        );
    }

    #[test]
    fn non_empty_uia_selection_is_used_untruncated() {
        assert_eq!(
            plan_from_probe(
                UiaProbe::Selection {
                    text: "hello".to_string(),
                    identity: None
                },
                DEFAULT_MAX_CHARS
            ),
            SelectionPlan::UseUia {
                text: "hello".to_string(),
                truncated: false,
                identity: None,
            }
        );
    }

    #[test]
    fn oversized_uia_selection_is_truncated() {
        let long = "x".repeat(10);
        assert_eq!(
            plan_from_probe(
                UiaProbe::Selection {
                    text: long,
                    identity: None
                },
                4
            ),
            SelectionPlan::UseUia {
                text: "xxxx".to_string(),
                truncated: true,
                identity: None,
            }
        );
    }

    // -- plan_from_probe: identity carry-through / discard (#219) -----------

    fn sample_identity() -> SelectionIdentity {
        SelectionIdentity {
            runtime_id: vec![1, 2, 3],
            automation_id: "compose-body".to_string(),
            name: "Message Body".to_string(),
            control_type: "Edit".to_string(),
            full_text: "Hello world".to_string(),
            start: 6,
            end: 11,
        }
    }

    #[test]
    fn non_empty_uia_selection_carries_its_identity_through_to_use_uia() {
        let plan = plan_from_probe(
            UiaProbe::Selection {
                text: "world".to_string(),
                identity: Some(sample_identity()),
            },
            DEFAULT_MAX_CHARS,
        );
        assert_eq!(
            plan,
            SelectionPlan::UseUia {
                text: "world".to_string(),
                truncated: false,
                identity: Some(sample_identity()),
            }
        );
    }

    #[test]
    fn empty_uia_selection_discards_any_identity_and_falls_back() {
        // Even if identity was somehow computed for a zero-width range, an
        // empty selection must still fall back -- never surface a "Do it"
        // target for nothing selected (#219's "must degrade... not a wrong
        // offset").
        let plan = plan_from_probe(
            UiaProbe::Selection {
                text: String::new(),
                identity: Some(sample_identity()),
            },
            DEFAULT_MAX_CHARS,
        );
        assert_eq!(plan, SelectionPlan::Fallback);
    }

    // -- combine_target (#219) ----------------------------------------------

    #[test]
    fn combine_target_is_none_with_no_foreground_hwnd() {
        // Plain `get_selection_foreground` (no hwnd) never attaches a
        // target, even if identity was captured.
        assert_eq!(combine_target(None, Some(sample_identity())), None);
    }

    #[test]
    fn combine_target_is_none_with_no_identity() {
        // A ValuePattern-only/no-TextPattern control, or a discontiguous
        // multi-range selection: hwnd alone is never enough.
        assert_eq!(combine_target(Some(4242), None), None);
    }

    #[test]
    fn combine_target_builds_the_full_target_when_both_are_present() {
        let target = combine_target(Some(4242), Some(sample_identity())).unwrap();
        assert_eq!(target.hwnd, 4242);
        assert_eq!(target.runtime_id, vec![1, 2, 3]);
        assert_eq!(target.automation_id, "compose-body");
        assert_eq!(target.name, "Message Body");
        assert_eq!(target.control_type, "Edit");
        assert_eq!(target.full_text, "Hello world");
        assert_eq!(target.start, 6);
        assert_eq!(target.end, 11);
    }

    // -- truncate_bounded ---------------------------------------------------

    #[test]
    fn keeps_short_text_unchanged() {
        assert_eq!(truncate_bounded("hi", 10), ("hi".to_string(), false));
    }

    #[test]
    fn keeps_exactly_the_max_without_truncating() {
        assert_eq!(truncate_bounded("abcd", 4), ("abcd".to_string(), false));
    }

    #[test]
    fn truncates_one_char_over_the_max() {
        assert_eq!(truncate_bounded("abcde", 4), ("abcd".to_string(), true));
    }

    #[test]
    fn truncates_by_char_not_byte_boundary() {
        // Each of these is a multi-byte UTF-8 scalar value; a byte-based
        // truncation would panic or split one in half.
        let text = "héllo wörld"; // 11 chars, several > 1 byte each
        let (head, truncated) = truncate_bounded(text, 6);
        assert_eq!(head, "héllo ");
        assert!(truncated);
        assert_eq!(head.chars().count(), 6);
    }

    #[test]
    fn max_zero_truncates_any_non_empty_text_to_empty() {
        assert_eq!(truncate_bounded("a", 0), (String::new(), true));
    }

    #[test]
    fn max_zero_on_empty_text_is_not_truncated() {
        assert_eq!(truncate_bounded("", 0), (String::new(), false));
    }

    // -- decode_unicode_text --------------------------------------------------

    fn utf16le_bytes(s: &str, nul_terminate: bool) -> Vec<u8> {
        let mut units: Vec<u16> = s.encode_utf16().collect();
        if nul_terminate {
            units.push(0);
        }
        units.iter().flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn decodes_nul_terminated_utf16() {
        let bytes = utf16le_bytes("hi", true);
        assert_eq!(decode_unicode_text(&bytes), "hi");
    }

    #[test]
    fn decodes_utf16_with_no_terminator() {
        let bytes = utf16le_bytes("hi", false);
        assert_eq!(decode_unicode_text(&bytes), "hi");
    }

    #[test]
    fn decodes_empty_buffer_as_empty_string() {
        assert_eq!(decode_unicode_text(&[]), "");
    }

    #[test]
    fn stops_at_the_first_nul_even_with_trailing_garbage() {
        let mut bytes = utf16le_bytes("ab", true);
        bytes.extend_from_slice(&99u16.to_le_bytes()); // garbage after the NUL
        assert_eq!(decode_unicode_text(&bytes), "ab");
    }

    // -- modifier_release_plan ------------------------------------------------

    fn mods(win: bool, shift: bool, alt: bool, ctrl: bool) -> ModifierState {
        ModifierState {
            win,
            shift,
            alt,
            ctrl,
        }
    }

    #[test]
    fn no_modifiers_down_releases_nothing() {
        assert_eq!(
            modifier_release_plan(mods(false, false, false, false)),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn win_down_releases_both_win_keys() {
        assert_eq!(
            modifier_release_plan(mods(true, false, false, false)),
            vec![VK_LWIN, VK_RWIN]
        );
    }

    #[test]
    fn shift_alone_releases_only_shift() {
        assert_eq!(
            modifier_release_plan(mods(false, true, false, false)),
            vec![VK_SHIFT]
        );
    }

    #[test]
    fn alt_alone_releases_only_alt() {
        assert_eq!(
            modifier_release_plan(mods(false, false, true, false)),
            vec![VK_MENU]
        );
    }

    #[test]
    fn ctrl_alone_releases_only_ctrl() {
        assert_eq!(
            modifier_release_plan(mods(false, false, false, true)),
            vec![VK_CONTROL]
        );
    }

    #[test]
    fn every_modifier_down_releases_all_in_fixed_order() {
        assert_eq!(
            modifier_release_plan(mods(true, true, true, true)),
            vec![VK_LWIN, VK_RWIN, VK_SHIFT, VK_MENU, VK_CONTROL]
        );
    }

    // -- build_ctrl_c_plan ------------------------------------------------

    #[test]
    fn ctrl_c_plan_with_no_release_is_just_the_combo() {
        let plan = build_ctrl_c_plan(&[]);
        assert_eq!(
            plan,
            vec![
                SyntheticKeyEvent {
                    vk: VK_CONTROL,
                    key_up: false
                },
                SyntheticKeyEvent {
                    vk: VK_C,
                    key_up: false
                },
                SyntheticKeyEvent {
                    vk: VK_C,
                    key_up: true
                },
                SyntheticKeyEvent {
                    vk: VK_CONTROL,
                    key_up: true
                },
            ]
        );
    }

    #[test]
    fn ctrl_c_plan_releases_come_before_the_combo() {
        let plan = build_ctrl_c_plan(&[VK_LWIN, VK_RWIN]);
        assert_eq!(
            plan,
            vec![
                SyntheticKeyEvent {
                    vk: VK_LWIN,
                    key_up: true
                },
                SyntheticKeyEvent {
                    vk: VK_RWIN,
                    key_up: true
                },
                SyntheticKeyEvent {
                    vk: VK_CONTROL,
                    key_up: false
                },
                SyntheticKeyEvent {
                    vk: VK_C,
                    key_up: false
                },
                SyntheticKeyEvent {
                    vk: VK_C,
                    key_up: true
                },
                SyntheticKeyEvent {
                    vk: VK_CONTROL,
                    key_up: true
                },
            ]
        );
    }

    // -- clipboard snapshot / restore / guard, against an in-memory fake ---

    use std::cell::RefCell;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct FakeClipboard {
        formats: RefCell<BTreeMap<u32, Vec<u8>>>,
        sequence: RefCell<u32>,
        /// When set, `set_formats` silently does nothing -- simulates a
        /// stuck/no-op restore so `restore_and_verify` has something real
        /// to catch.
        fail_restore_silently: RefCell<bool>,
    }

    impl FakeClipboard {
        fn seeded(pairs: &[(u32, &[u8])]) -> Self {
            let formats = pairs.iter().map(|&(f, b)| (f, b.to_vec())).collect();
            Self {
                formats: RefCell::new(formats),
                sequence: RefCell::new(1),
                fail_restore_silently: RefCell::new(false),
            }
        }
    }

    // Single-threaded per test, same rationale as
    // `executors::clipboard::tests::FakeClipboard`.
    unsafe impl Sync for FakeClipboard {}

    impl RawClipboard for FakeClipboard {
        fn sequence_number(&self) -> u32 {
            *self.sequence.borrow()
        }

        fn get_format(&self, format: u32) -> Option<Vec<u8>> {
            self.formats.borrow().get(&format).cloned()
        }

        fn set_formats(&self, formats: &[(u32, Vec<u8>)]) -> anyhow::Result<()> {
            if *self.fail_restore_silently.borrow() {
                return Ok(()); // simulates a write that reports success but changes nothing
            }
            *self.formats.borrow_mut() = formats.iter().cloned().collect();
            *self.sequence.borrow_mut() += 1;
            Ok(())
        }
    }

    const FMT_TEXT: u32 = 13;
    const FMT_HDROP: u32 = 15;
    const FMT_OTHER: u32 = 999; // not in any "formats_of_interest" list below

    #[test]
    fn snapshot_captures_only_present_formats_of_interest() {
        let clipboard = FakeClipboard::seeded(&[(FMT_TEXT, b"hello"), (FMT_OTHER, b"ignored")]);
        let snap = snapshot(&clipboard, &[FMT_TEXT, FMT_HDROP]);
        assert_eq!(snap.formats, vec![(FMT_TEXT, b"hello".to_vec())]);
    }

    #[test]
    fn snapshot_of_a_format_never_present_is_empty() {
        let clipboard = FakeClipboard::default();
        let snap = snapshot(&clipboard, &[FMT_TEXT, FMT_HDROP]);
        assert!(snap.formats.is_empty());
    }

    #[test]
    fn a_cf_dibv5_only_snapshot_round_trips_through_the_production_format_list() {
        // #265: CF_DIBV5 (BITMAPV5HEADER images -- Snipping Tool/Snip &
        // Sketch, some browser image copies, sometimes with no parallel
        // CF_DIB) must not be silently dropped by a selection-fallback
        // round trip, the same way CF_DIB already is not.
        let cf_dibv5 = windows::Win32::System::Ole::CF_DIBV5.0 as u32;
        let clipboard = FakeClipboard::seeded(&[(cf_dibv5, b"fake-dibv5-bytes")]);

        let snap = snapshot(&clipboard, &win32::preserved_formats());
        clipboard.set_formats(&[]).unwrap(); // the injected Ctrl+C clearing the clipboard
        restore(&clipboard, &snap).unwrap();

        assert_eq!(
            clipboard.get_format(cf_dibv5),
            Some(b"fake-dibv5-bytes".to_vec()),
            "a CF_DIBV5 image must survive a selection-fallback clipboard round trip"
        );
    }

    #[test]
    fn restore_writes_back_exactly_the_captured_formats() {
        let clipboard = FakeClipboard::seeded(&[(FMT_TEXT, b"original")]);
        let snap = snapshot(&clipboard, &[FMT_TEXT]);
        clipboard
            .set_formats(&[(FMT_TEXT, b"injected copy".to_vec())])
            .unwrap();
        assert_eq!(clipboard.get_format(FMT_TEXT).unwrap(), b"injected copy");

        restore(&clipboard, &snap).unwrap();
        assert_eq!(clipboard.get_format(FMT_TEXT).unwrap(), b"original");
    }

    #[test]
    fn restore_of_an_originally_empty_clipboard_leaves_it_empty() {
        let clipboard = FakeClipboard::default();
        let snap = snapshot(&clipboard, &[FMT_TEXT, FMT_HDROP]);
        clipboard
            .set_formats(&[(FMT_TEXT, b"injected".to_vec())])
            .unwrap();

        restore(&clipboard, &snap).unwrap();
        assert_eq!(clipboard.get_format(FMT_TEXT), None);
    }

    #[test]
    fn restore_and_verify_errs_when_the_write_silently_no_ops() {
        let clipboard = FakeClipboard::seeded(&[(FMT_TEXT, b"original")]);
        let snap = snapshot(&clipboard, &[FMT_TEXT]);
        *clipboard.fail_restore_silently.borrow_mut() = true;

        let err = restore_and_verify(&clipboard, &snap).unwrap_err();
        assert!(err.to_string().contains("sequence number"));
    }

    #[test]
    fn restore_and_verify_succeeds_on_a_real_write() {
        let clipboard = FakeClipboard::seeded(&[(FMT_TEXT, b"original")]);
        let snap = snapshot(&clipboard, &[FMT_TEXT]);
        assert!(restore_and_verify(&clipboard, &snap).is_ok());
    }

    // -- ClipboardGuard: the "never leave the clipboard modified" guarantee -

    #[test]
    fn guard_restore_now_puts_the_original_back() {
        let clipboard = FakeClipboard::seeded(&[(FMT_TEXT, b"original")]);
        let mut guard = ClipboardGuard::capture(&clipboard, &[FMT_TEXT]);
        clipboard
            .set_formats(&[(FMT_TEXT, b"injected".to_vec())])
            .unwrap();

        guard.restore_now().unwrap();
        assert_eq!(clipboard.get_format(FMT_TEXT).unwrap(), b"original");
    }

    #[test]
    fn dropping_the_guard_without_calling_restore_now_still_restores() {
        let clipboard = FakeClipboard::seeded(&[(FMT_TEXT, b"original")]);
        {
            let _guard = ClipboardGuard::capture(&clipboard, &[FMT_TEXT]);
            clipboard
                .set_formats(&[(FMT_TEXT, b"injected".to_vec())])
                .unwrap();
            // _guard drops here at end of scope -- restore_now was never called.
        }
        assert_eq!(
            clipboard.get_format(FMT_TEXT).unwrap(),
            b"original",
            "Drop must restore the clipboard even when the caller never explicitly asked"
        );
    }

    #[test]
    fn an_early_return_after_mutating_the_clipboard_still_restores_via_drop() {
        fn do_work(clipboard: &FakeClipboard) -> anyhow::Result<()> {
            let _guard = ClipboardGuard::capture(clipboard, &[FMT_TEXT]);
            clipboard
                .set_formats(&[(FMT_TEXT, b"injected".to_vec())])
                .unwrap();
            anyhow::bail!("something went wrong mid-operation");
            // _guard drops during unwind of this early return.
        }

        let clipboard = FakeClipboard::seeded(&[(FMT_TEXT, b"original")]);
        let result = do_work(&clipboard);
        assert!(result.is_err());
        assert_eq!(
            clipboard.get_format(FMT_TEXT).unwrap(),
            b"original",
            "an error path must never leave the clipboard modified"
        );
    }

    #[test]
    fn restore_now_is_idempotent_and_drop_does_not_restore_twice() {
        let clipboard = FakeClipboard::seeded(&[(FMT_TEXT, b"original")]);
        let seq_after_first_restore;
        {
            let mut guard = ClipboardGuard::capture(&clipboard, &[FMT_TEXT]);
            clipboard
                .set_formats(&[(FMT_TEXT, b"injected".to_vec())])
                .unwrap();
            guard.restore_now().unwrap();
            seq_after_first_restore = clipboard.sequence_number();
            // guard drops here; if Drop restored again, the sequence number
            // (which FakeClipboard::set_formats always bumps) would move.
        }
        assert_eq!(
            clipboard.sequence_number(),
            seq_after_first_restore,
            "Drop must not perform a second restore after restore_now already ran"
        );
    }

    #[test]
    fn restore_now_leaves_done_false_on_failure_so_drop_still_retries() {
        // #262: a failed restore_now must not forfeit Drop's safety net.
        let clipboard = FakeClipboard::seeded(&[(FMT_TEXT, b"original")]);
        let mut guard = ClipboardGuard::capture(&clipboard, &[FMT_TEXT]);
        clipboard
            .set_formats(&[(FMT_TEXT, b"injected".to_vec())])
            .unwrap();

        // Make the first restore attempt fail (a silent no-op write, the
        // same fake behaviour `restore_and_verify_errs_when_the_write_silently_no_ops`
        // uses).
        *clipboard.fail_restore_silently.borrow_mut() = true;
        let err = guard.restore_now().unwrap_err();
        assert!(err.to_string().contains("sequence number"));
        assert_eq!(
            clipboard.get_format(FMT_TEXT).unwrap(),
            b"injected",
            "restore_now failed, so the injected copy must still be on the clipboard"
        );

        // Let a later attempt (Drop's safety net) succeed.
        *clipboard.fail_restore_silently.borrow_mut() = false;
        drop(guard);

        assert_eq!(
            clipboard.get_format(FMT_TEXT).unwrap(),
            b"original",
            "Drop must still attempt a restore after a failed restore_now, per the \
             doc comment's stated safety-net guarantee"
        );
    }

    // -- win32-only helpers: real formats and a real EDIT control ----------

    mod win32_tests {
        use super::super::*;
        use windows::core::w;
        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::Controls::EM_SETSEL;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, LoadCursorW,
            PeekMessageW, RegisterClassExW, SendMessageW, ShowWindow, TranslateMessage, CS_HREDRAW,
            CS_VREDRAW, IDC_ARROW, MSG, PM_REMOVE, SW_SHOWNOACTIVATE, WNDCLASSEXW, WS_CHILD,
            WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
        };

        fn pump_pending_messages() {
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }

        const CLASS_NAME: windows::core::PCWSTR =
            w!("Wingman.Inputs.SelectionTestWindow.test.a11c7e");

        static CLASS_INIT: std::sync::Once = std::sync::Once::new();
        static CLASS_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

        fn instance() -> HINSTANCE {
            let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
            HINSTANCE(h.0)
        }

        unsafe extern "system" fn test_wndproc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        fn ensure_class_registered(hinstance: HINSTANCE) -> bool {
            CLASS_INIT.call_once(|| {
                let ok = unsafe {
                    let wc = WNDCLASSEXW {
                        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                        style: CS_HREDRAW | CS_VREDRAW,
                        lpfnWndProc: Some(test_wndproc),
                        hInstance: hinstance,
                        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                        lpszClassName: CLASS_NAME,
                        ..Default::default()
                    };
                    RegisterClassExW(&wc) != 0
                };
                let _ = CLASS_OK.set(ok);
            });
            CLASS_OK.get().copied().unwrap_or(false)
        }

        /// Serializes this module's three tests against each other.
        /// MEASURED 2026-09-17: concurrent `CoCreateInstance(CUIAutomation8,
        /// ...)` calls from multiple threads in the same process
        /// intermittently fail with E_FAIL (`0x80004005`) from
        /// `com::probe_element` -- reproduced repeatedly with `cargo test
        /// inputs::` (default parallel test threads); `--test-threads=1`
        /// made the failure disappear across 10+ runs. The same race also
        /// hits `inputs::uia`'s own pre-existing integration test when run
        /// alongside these (filed as a follow-up finding; not fixed here,
        /// `uia.rs` is out of this task's scope) -- this lock only
        /// serializes the three tests below against each other, not against
        /// `uia.rs`'s test.
        use crate::inputs::lock_uia_test;

        /// The task brief's required integration test: a real EDIT control
        /// with known text and a programmatic `EM_SETSEL` selection,
        /// asserting the UIA path returns exactly the selected text. Uses
        /// `probe_element` (by handle), not `probe_focused` (real desktop
        /// focus) -- see `com::probe_element`'s doc comment and
        /// `inputs::uia`'s own integration test for the identical, already
        /// established rationale: this way the assertion does not depend on
        /// the test window actually holding desktop focus under an
        /// unattended agent.
        #[test]
        fn uia_selection_matches_a_real_em_setsel_selection() {
            let _lock = lock_uia_test();
            let hinstance = instance();
            assert!(
                ensure_class_registered(hinstance),
                "RegisterClassExW for the test window class"
            );

            let frame = unsafe {
                CreateWindowExW(
                    Default::default(),
                    CLASS_NAME,
                    w!("Wingman selection test window"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    0,
                    0,
                    320,
                    120,
                    None,
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (frame)");
            unsafe {
                let _ = ShowWindow(frame, SW_SHOWNOACTIVATE);
            }
            pump_pending_messages();

            let edit = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("EDIT"),
                    w!("Hello, selection world"),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    10,
                    10,
                    280,
                    20,
                    Some(frame),
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (edit)");
            pump_pending_messages();

            // Select "selection" (chars 7..16 of "Hello, selection world").
            unsafe {
                SendMessageW(edit, EM_SETSEL, Some(WPARAM(7)), Some(LPARAM(16)));
            }
            pump_pending_messages();

            let start = std::time::Instant::now();
            let probe = com::probe_element(edit).expect("com::probe_element");
            let elapsed = start.elapsed();
            // MEASURED (recorded verbatim in the commit message): printed
            // here so a `-- --nocapture` run shows the real number this
            // test observed on this machine, matching `inputs::uia`'s own
            // integration test's convention.
            eprintln!("selection::com::probe_element took {elapsed:?} for an EDIT control");

            // #219: a single contiguous real EDIT-control selection must
            // also carry its owning element's identity and the selection's
            // own UTF-16 offsets into the control's whole current text --
            // the observable that would differ if `SelectionTarget`
            // capturing were wired to nothing.
            match probe {
                UiaProbe::Selection { text, identity } => {
                    assert_eq!(text, "selection");
                    let identity = identity.expect(
                        "a single contiguous EDIT-control selection must carry its identity",
                    );
                    assert_eq!(identity.control_type, "Edit");
                    assert_eq!(identity.full_text, "Hello, selection world");
                    assert_eq!(identity.start, 7);
                    assert_eq!(identity.end, 16);
                    assert!(
                        !identity.runtime_id.is_empty(),
                        "a real UIA element must report a non-empty runtime id"
                    );
                }
                other => panic!("expected UiaProbe::Selection, got {other:?}"),
            }

            unsafe {
                let _ = DestroyWindow(frame);
            }
        }

        /// #219: the selection's own UTF-16 offsets must round-trip a
        /// non-BMP character (a surrogate pair) correctly -- a char-count or
        /// byte-count offset would both be wrong here. "Hi <emoji> team":
        /// "Hi " is 3 UTF-16 units, the emoji is 2 (a high + low surrogate),
        /// then " team" is 5 more (10 units total). Selecting just the
        /// emoji is `EM_SETSEL(3, 5)`.
        #[test]
        fn uia_selection_offsets_round_trip_a_non_bmp_character() {
            let _lock = lock_uia_test();
            let hinstance = instance();
            assert!(ensure_class_registered(hinstance));

            let frame = unsafe {
                CreateWindowExW(
                    Default::default(),
                    CLASS_NAME,
                    w!("Wingman selection test window (emoji)"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    0,
                    0,
                    320,
                    120,
                    None,
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (frame)");
            unsafe {
                let _ = ShowWindow(frame, SW_SHOWNOACTIVATE);
            }
            pump_pending_messages();

            let text = "Hi \u{1F600} team";
            assert_eq!(text.encode_utf16().count(), 10);

            let text_w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let edit = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("EDIT"),
                    windows::core::PCWSTR(text_w.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    10,
                    10,
                    280,
                    20,
                    Some(frame),
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (edit)");
            pump_pending_messages();

            // Select exactly the emoji (UTF-16 offsets 3..5), never
            // splitting its surrogate pair.
            unsafe {
                SendMessageW(edit, EM_SETSEL, Some(WPARAM(3)), Some(LPARAM(5)));
            }
            pump_pending_messages();

            let probe = com::probe_element(edit).expect("com::probe_element");
            match probe {
                UiaProbe::Selection {
                    text: selected_text,
                    identity,
                } => {
                    assert_eq!(selected_text, "\u{1F600}");
                    let identity = identity
                        .expect("a single contiguous emoji selection must carry its identity");
                    assert_eq!(identity.full_text, text);
                    assert_eq!(identity.start, 3);
                    assert_eq!(identity.end, 5);
                }
                other => panic!("expected UiaProbe::Selection, got {other:?}"),
            }

            unsafe {
                let _ = DestroyWindow(frame);
            }
        }

        #[test]
        fn no_selection_reports_an_empty_uia_selection_not_no_text_pattern() {
            let _lock = lock_uia_test();
            let hinstance = instance();
            assert!(ensure_class_registered(hinstance));

            let frame = unsafe {
                CreateWindowExW(
                    Default::default(),
                    CLASS_NAME,
                    w!("Wingman selection test window (empty)"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    0,
                    0,
                    320,
                    120,
                    None,
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (frame)");
            unsafe {
                let _ = ShowWindow(frame, SW_SHOWNOACTIVATE);
            }
            pump_pending_messages();

            let edit = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("EDIT"),
                    w!("no selection here"),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    10,
                    10,
                    280,
                    20,
                    Some(frame),
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (edit)");
            pump_pending_messages();

            // An explicit zero-length selection (caret placed, nothing
            // selected), not "never called EM_SETSEL at all".
            unsafe {
                SendMessageW(edit, EM_SETSEL, Some(WPARAM(3)), Some(LPARAM(3)));
            }
            pump_pending_messages();

            let probe = com::probe_element(edit).expect("com::probe_element");
            assert_eq!(
                probe,
                UiaProbe::Selection {
                    text: String::new(),
                    identity: None
                }
            );
            assert_eq!(
                plan_from_probe(probe, DEFAULT_MAX_CHARS),
                SelectionPlan::Fallback,
                "an empty UIA selection must still fall back to the clipboard path"
            );

            unsafe {
                let _ = DestroyWindow(frame);
            }
        }

        #[test]
        fn password_edit_control_is_reported_as_focused_is_password() {
            use windows::Win32::UI::WindowsAndMessaging::ES_PASSWORD;

            let _lock = lock_uia_test();
            let hinstance = instance();
            assert!(ensure_class_registered(hinstance));

            let frame = unsafe {
                CreateWindowExW(
                    Default::default(),
                    CLASS_NAME,
                    w!("Wingman selection test window (password)"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    0,
                    0,
                    320,
                    120,
                    None,
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (frame)");
            unsafe {
                let _ = ShowWindow(frame, SW_SHOWNOACTIVATE);
            }
            pump_pending_messages();

            let edit = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("EDIT"),
                    w!("hunter2"),
                    WS_CHILD
                        | WS_VISIBLE
                        | WS_TABSTOP
                        | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_PASSWORD as u32),
                    10,
                    10,
                    280,
                    20,
                    Some(frame),
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (password edit)");
            pump_pending_messages();

            unsafe {
                SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(7)));
            }
            pump_pending_messages();

            let probe = com::probe_element(edit).expect("com::probe_element");
            assert_eq!(probe, UiaProbe::FocusedIsPassword);
            assert_eq!(
                plan_from_probe(probe, DEFAULT_MAX_CHARS),
                SelectionPlan::SkipPasswordField
            );

            unsafe {
                let _ = DestroyWindow(frame);
            }
        }
    }
}
