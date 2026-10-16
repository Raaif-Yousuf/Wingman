//! Shared UIA target-identity, re-resolution and live-access plumbing for
//! executors that write to a UIA element captured at "Look" time:
//! `replace_text` (#32) and `fill_form` (#33). Extracted here (#33) so
//! neither executor duplicates the walk/match/read/write machinery -- the
//! only thing that differs between them is what they do with a resolved
//! element and what they write. See `replace_text.rs`'s module doc comment
//! for the two ways a [`TargetRef`] can fail to re-resolve (not found,
//! ambiguous) and why both are a refusal, never a silent write.
//!
//! # The typed-input fallback
//!
//! [`TextElementAccess::write_with_fallback`] is the one thing that differs
//! per caller: `replace_text` never calls it (its own design doc: "no
//! keystroke synthesis is attempted here, per the task's scope") and always
//! uses [`TextElementAccess::write`], which only ever calls
//! `ValuePattern.SetValue` and refuses outright when a control does not
//! support it. `fill_form` (#33) calls `write_with_fallback`, whose real
//! (`com::UiaTextElementAccess`) implementation additionally falls back to
//! focus plus typed Unicode input ([`plan_typed_input`]) for a control that
//! exposes no `ValuePattern` and is an `Edit` ([`plan_typed_fallback`]).
//! The default trait implementation is just `write` -- only the real,
//! UIA-backed implementation overrides it, so every fake used by this
//! file's and `fill_form.rs`'s tests gets plain `write` semantics with no
//! extra code, and [`plan_typed_input`] is exercised by this module's own
//! pure tests, never by `SendInput` in an automated test (task brief:
//! "document and test the fallback planning purely, and do not exercise
//! SendInput in automated tests" -- same shape as
//! `inputs::selection::win32::inject_events`, whose own doc comment gives
//! the same reason: it would type into whatever real window has focus when
//! the test runs).

use anyhow::{anyhow, Result};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Pure types
// ---------------------------------------------------------------------------

/// Identifies one UIA element, captured at "Look" time and re-resolved at
/// "Do" time. `hwnd` is the window whose descendants are walked to find the
/// match (a raw `HWND`'s pointer value, stored as `isize` rather than the
/// `windows` crate's `HWND` type so this struct stays plain data -- `Send`,
/// `Eq`, constructible from JSON with no Win32 dependency).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetRef {
    pub hwnd: isize,
    pub runtime_id: Vec<i32>,
    pub automation_id: String,
    pub name: String,
    pub control_type: String,
}

/// The live state of one re-resolved element, as read by a
/// [`TextElementAccess`] implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedElement {
    pub name: String,
    pub automation_id: String,
    pub control_type: String,
    pub is_password: bool,
    /// Empty for a password field -- its real value is never read (see the
    /// module doc comment on both callers' password guard).
    pub current_text: String,
    /// False when the control exposes no `ValuePattern` (or is a password
    /// field), so `current_text` is "" whatever the field really holds. A
    /// write to such a control goes through the typed fallback, and its old
    /// text is neither known nor restorable (#645).
    pub value_readable: bool,
}

/// Result of matching a [`TargetRef`] against a fresh walk of candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOutcome {
    Found(usize),
    NotFound,
    Ambiguous,
    /// A single match, but some elements were skipped as unreadable (#590),
    /// so it cannot be shown to be the only one.
    Unconfirmed,
}

/// The single decision point for "which candidate is the target, if any".
/// Pure: exact equality on every [`TargetRef`] field. More than one exact
/// match is [`MatchOutcome::Ambiguous`] -- this function never guesses.
pub fn resolve_index(candidates: &[TargetRef], target: &TargetRef) -> MatchOutcome {
    let mut found = None;
    for (i, candidate) in candidates.iter().enumerate() {
        if candidate == target {
            if found.is_some() {
                return MatchOutcome::Ambiguous;
            }
            found = Some(i);
        }
    }
    match found {
        Some(i) => MatchOutcome::Found(i),
        None => MatchOutcome::NotFound,
    }
}

/// #590: a control type that could not be read degrades to `"Other"` (the
/// bucket `control_type_to_string` already uses for unknown ids), so one
/// non-conformant element costs only itself. `"Other"` is never a type a
/// proposal targets for writing, so a degraded element cannot become a write
/// target by accident.
pub fn degraded_control_type<E>(read: Result<String, E>) -> String {
    read.unwrap_or_else(|_| "Other".to_string())
}

/// #590: whether a resolved control type is the degraded `"Other"` (an
/// unreadable type, or a type this crate does not map). Writers refuse it: a
/// proposal that echoes `"Other"` could otherwise match a degraded element and
/// write into something whose real kind is unknown.
pub fn is_unknown_control_type(control_type: &str) -> bool {
    control_type == "Other"
}

/// #590: an `IsPassword` read that fails is treated as a password field
/// (fails CLOSED): its value is not read and it is refused for writing.
/// Same rule as `inputs::uia`'s `resolve_is_password`.
pub fn resolve_password_read<E>(read: Result<bool, E>) -> bool {
    read.unwrap_or(true)
}

/// #590: elements whose fetch failed were left out of the walk. Their
/// identity is unknown, so a target that has no runtime id (identity is only
/// name, automation id and type) can no longer be shown unique and a `Found`
/// becomes `Ambiguous`. A target with a runtime id is unique per element, so
/// skips cannot hide a twin. `NotFound` and `Ambiguous` pass through.
pub fn guard_skipped(outcome: MatchOutcome, target: &TargetRef, skipped: usize) -> MatchOutcome {
    match outcome {
        MatchOutcome::Found(_) if skipped > 0 && target.runtime_id.is_empty() => {
            MatchOutcome::Unconfirmed
        }
        other => other,
    }
}

/// Whether the element's current text has drifted from what the preview
/// showed -- the stale-target check both callers need ("someone typed in
/// between").
pub fn is_stale(expected_current_text: &str, actual_current_text: &str) -> bool {
    expected_current_text != actual_current_text
}

/// The error a [`TextElementAccess::write`] returns when the element's text no
/// longer equals the text the caller expected (#432). A distinct type so a
/// caller can tell "someone typed in between" from any other write failure
/// (`anyhow::Error::downcast_ref::<StaleTarget>`) and word its own card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleTarget;

impl std::fmt::Display for StaleTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the text has changed since the preview was shown; refusing to overwrite it"
        )
    }
}

impl std::error::Error for StaleTarget {}

/// Whether `err` (at any depth of its context chain) is a [`StaleTarget`].
pub fn is_stale_target_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<StaleTarget>().is_some()
}

/// `Ok` when `actual_current_text` still equals what the caller expected,
/// otherwise a [`StaleTarget`] error. The one compare every
/// [`TextElementAccess::write`] implementation runs on the SAME element it is
/// about to write (#432).
pub fn ensure_unchanged(expected_current_text: &str, actual_current_text: &str) -> Result<()> {
    if is_stale(expected_current_text, actual_current_text) {
        return Err(StaleTarget.into());
    }
    Ok(())
}

/// Parses one `TargetRef` out of a proposal's `"target"` object. Shared by
/// `replace_text::parse_replace_text` and `fill_form::parse_field_fill` --
/// every proposal that names a UIA element uses this exact shape (`hwnd`,
/// `runtime_id`, `automation_id`, `name`, `control_type`). Only `hwnd` is
/// required; the rest default to empty, matching a target whose walk found
/// no automation id or name.
pub fn parse_target_ref(value: &Value) -> Result<TargetRef> {
    let hwnd = value
        .get("hwnd")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("a target is missing its \"hwnd\" field"))? as isize;
    let runtime_id = value
        .get("runtime_id")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_i64)
                .map(|n| n as i32)
                .collect()
        })
        .unwrap_or_default();
    let automation_id = value
        .get("automation_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let control_type = value
        .get("control_type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    Ok(TargetRef {
        hwnd,
        runtime_id,
        automation_id,
        name,
        control_type,
    })
}

/// Whether a UIA control type is invokable in the "pressing it does
/// something" sense -- a button and its near relatives. `fill_form` (#33)
/// refuses to target any of these regardless of label, on top of the
/// name/automation-id deny-list in [`super::uia_guard`]: a button with an
/// innocuous label ("Continue", "Next") is still never a form field.
/// `replace_text` does not call this -- it only ever targets an editable
/// field by construction, and relies on [`super::uia_guard::is_forbidden_target`]
/// as its own second line of defense (see its module doc comment).
pub fn is_invokable_control_type(control_type: &str) -> bool {
    matches!(
        control_type,
        "Button" | "Hyperlink" | "MenuItem" | "SplitButton"
    )
}

/// One `SendInput` Unicode key event: a UTF-16 code unit plus whether this is
/// the key-down or key-up half. Deliberately not a `windows::Win32::...`
/// type -- this stays plain data so [`plan_typed_input`] is testable with no
/// Win32 dependency, the same split `inputs::selection`'s
/// `SyntheticKeyEvent` makes for its own Ctrl+C injection plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnicodeKeyEvent {
    pub code_unit: u16,
    pub key_up: bool,
}

/// Plans the `SendInput` Unicode key events that would type `text` into a
/// focused control, one down/up pair per UTF-16 code unit, in order. Never
/// plans an Enter, Tab or other control-key event: `\n`, `\r` and `\t` are
/// filtered out of `text` before planning, because the fallback's whole
/// point is entering a value, never a submit or focus-advance sequence (task
/// brief: "must never send Enter/Tab-submit sequences"). Pure: this is the
/// planning step the task brief asks to test without ever calling
/// `SendInput` -- see the module doc comment.
pub fn plan_typed_input(text: &str) -> Vec<UnicodeKeyEvent> {
    let mut events = Vec::new();
    for c in text.chars().filter(|c| !matches!(c, '\n' | '\r' | '\t')) {
        let mut buf = [0u16; 2];
        for unit in c.encode_utf16(&mut buf) {
            events.push(UnicodeKeyEvent {
                code_unit: *unit,
                key_up: false,
            });
            events.push(UnicodeKeyEvent {
                code_unit: *unit,
                key_up: true,
            });
        }
    }
    events
}

/// What the typed-input fallback does to a field, in order (#592): select
/// everything, delete the selection when there is nothing to type, then type
/// `text`. Typing over a selection replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedReplacePlan {
    pub select_all: bool,
    pub delete_selection: bool,
    pub text: Vec<UnicodeKeyEvent>,
}

/// The fallback's whole decision: refuse, or a REPLACE plan (never an append).
/// The fallback only runs for a control with no `ValuePattern`, and the walk
/// reads `current_text` only from a `ValuePattern`, so the expected text is
/// always empty here even when the field holds text. So this never looks at
/// it: select-all is unconditional (harmless on an empty field).
/// Only `Edit` is allowed. Ctrl+A in a `Document` (a browser page, a Word
/// body) selects the whole document and typing would wipe it. A `ComboBox`
/// with no `ValuePattern` cannot be told apart from a drop-down list, where
/// Ctrl+A does nothing and typing is only type-ahead, so the card would say
/// Filled when nothing was filled. Both refuse, as does any other type.
pub fn plan_typed_fallback(control_type: &str, new_text: &str) -> Result<TypedReplacePlan> {
    if control_type != "Edit" {
        return Err(anyhow!(
            "Wingman cannot safely type into this field ({control_type}): the app does not let it set the text directly, and typing could overwrite the wrong thing. Nothing was written. Click the field and type the value yourself."
        ));
    }
    let text = plan_typed_input(new_text);
    Ok(TypedReplacePlan {
        select_all: true,
        delete_selection: text.is_empty(),
        text,
    })
}
/// The go/no-go for sending keystrokes into a field (#676). Select-all plus
/// typed text is the most destructive thing Wingman sends, and it goes to
/// whatever has keyboard focus, so it is sent only when every check ran and
/// agrees: the focused UIA element is the target (`same_element`), and the
/// foreground window's top-level window is the target's (`foreground_root`
/// equals `target_root`, neither null). A check that could not run is `None`,
/// and that refuses too: when in doubt, nothing is typed.
pub fn confirm_typed_focus(
    same_element: Option<bool>,
    foreground_root: Option<isize>,
    target_root: Option<isize>,
) -> Result<()> {
    let in_front = matches!(
        (foreground_root, target_root),
        (Some(f), Some(t)) if f != 0 && f == t
    );
    if same_element == Some(true) && in_front {
        return Ok(());
    }
    let wrong_window = matches!(
        (foreground_root, target_root),
        (Some(f), Some(t)) if f != t
    );
    if wrong_window {
        return Err(anyhow!(
            "the window with this field is not in front, so Wingman typed nothing into it. Click the field and type the value yourself."
        ));
    }
    Err(anyhow!(
        "Wingman could not confirm the cursor is in this field, so it typed nothing into it. Click the field and type the value yourself."
    ))
}

// ---------------------------------------------------------------------------
// The injectable seam (mirrors executors::clipboard::ClipboardAccess)
// ---------------------------------------------------------------------------

/// Abstracts "re-resolve a target and read its live state" / "write a
/// target's value" so refusal logic (stale-target, password, forbidden
/// target, undo/restore round trips) is tested against a fake element model
/// with no live UIA element. The real implementation is
/// [`com::UiaTextElementAccess`].
pub trait TextElementAccess: Send + Sync {
    /// Re-resolves `target` (a fresh walk of its window's descendants, an
    /// exact match on every `TargetRef` field) and returns its live state.
    /// Errs if the element cannot be found again or more than one element
    /// now matches -- never guesses.
    fn resolve(&self, target: &TargetRef) -> Result<ResolvedElement>;

    /// Re-resolves `target` again (independently of any earlier `resolve`
    /// call -- each call is self-contained, see `com::UiaTextElementAccess`'s
    /// doc comment) and writes `new_text` via `ValuePattern.SetValue`, but
    /// only if THAT walk's element still reads `expected_current_text`: the
    /// staleness check and the write use the same element, so text typed
    /// between a caller's earlier `resolve` and this call is caught (#432).
    /// Errs with a [`StaleTarget`] when the text differs; also errs if the
    /// element cannot be found, is a password field, or does not support
    /// `ValuePattern`. Nothing is written on any error.
    fn write(&self, target: &TargetRef, expected_current_text: &str, new_text: &str) -> Result<()>;

    /// Same as [`write`](Self::write), but for `fill_form` (#33) only: when
    /// the real implementation finds no `ValuePattern` and the control is
    /// editable, falls back to focus plus typed Unicode input instead of
    /// refusing outright. The default implementation is plain `write` --
    /// every fake gets this for free, and `replace_text` never calls this
    /// method at all (see the module doc comment).
    fn write_with_fallback(
        &self,
        target: &TargetRef,
        expected_current_text: &str,
        new_text: &str,
    ) -> Result<()> {
        self.write(target, expected_current_text, new_text)
    }
}

// ---------------------------------------------------------------------------
// Win32 / UI Automation
// ---------------------------------------------------------------------------

pub mod com {
    #![allow(dead_code)]

    use super::{
        confirm_typed_focus, degraded_control_type, ensure_unchanged, guard_skipped,
        plan_typed_fallback, resolve_index, resolve_password_read, MatchOutcome, ResolvedElement,
        TargetRef, TextElementAccess, TypedReplacePlan, UnicodeKeyEvent,
    };
    use anyhow::Result;
    use windows::core::{Interface, BSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, SAFEARRAY,
    };
    use windows::Win32::System::Ole::{
        SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
        SafeArrayUnaccessData,
    };
    use windows::Win32::UI::Accessibility::{
        IUIAutomation, IUIAutomationElement, IUIAutomationValuePattern, TreeScope_Descendants,
        UIA_AutomationIdPropertyId, UIA_ButtonControlTypeId, UIA_ControlTypePropertyId,
        UIA_HyperlinkControlTypeId, UIA_IsPasswordPropertyId, UIA_MenuItemControlTypeId,
        UIA_NamePropertyId, UIA_SplitButtonControlTypeId, UIA_ValuePatternId,
        UIA_ValueValuePropertyId,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        VIRTUAL_KEY, VK_A, VK_BACK, VK_CONTROL,
    };

    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetForegroundWindow, GA_ROOT};

    use crate::hotkey::INJECTED_MARKER;

    /// Same RAII pairing as `inputs::uia::com::ComApartment` /
    /// `inputs::selection::com::ComApartment`, duplicated rather than shared
    /// -- those are private to their own files.
    struct ComApartment;

    impl ComApartment {
        fn enter() -> Result<Self> {
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

    /// One element found while walking a window's descendants, plus enough
    /// state to build both a [`TargetRef`] (for matching) and a
    /// [`ResolvedElement`] (for the caller) without a second COM round trip.
    struct MatchedElement {
        element: IUIAutomationElement,
        name: String,
        automation_id: String,
        control_type: String,
        is_password: bool,
        current_text: String,
        value_readable: bool,
    }

    /// One walked candidate's non-identity extras (password/value), kept
    /// parallel to `walk`'s `Vec<TargetRef>`/`Vec<IUIAutomationElement>`
    /// rather than folded into `TargetRef` itself, since `TargetRef` is pure
    /// identity data shared with the JSON proposal shape.
    type WalkExtra = (bool, Option<String>);

    /// `walk`'s result: parallel `TargetRef`/element/extra vectors, one
    /// entry per candidate, in the same order, plus how many elements were
    /// skipped because they could not be fetched (#590). A skipped element
    /// is pushed to none of the three vectors, so they stay aligned.
    type WalkResult = (
        Vec<TargetRef>,
        Vec<IUIAutomationElement>,
        Vec<WalkExtra>,
        usize,
    );

    /// The one bulk COM call this module makes: walks `hwnd`'s descendants
    /// with a single `FindAllBuildCache`, same shape as
    /// `inputs::uia::com::walk` (a `True` condition over `Descendants`, one
    /// cache request, no per-element property round trip except
    /// `GetRuntimeId`, which has no cached form -- see
    /// [`runtime_id_from_safearray`]'s call site).
    fn walk(automation: &IUIAutomation, hwnd: HWND) -> Result<WalkResult> {
        let cache_request = unsafe { automation.CreateCacheRequest() }?;
        unsafe {
            cache_request.AddProperty(UIA_NamePropertyId)?;
            cache_request.AddProperty(UIA_AutomationIdPropertyId)?;
            cache_request.AddProperty(UIA_ControlTypePropertyId)?;
            cache_request.AddProperty(UIA_IsPasswordPropertyId)?;
            cache_request.AddPattern(UIA_ValuePatternId)?;
            // A cached pattern's own cached property additionally needs its
            // property cached, or reading it fails with E_INVALIDARG --
            // MEASURED 2026-09-17 in `inputs::uia::com::walk` against this
            // exact call shape; carried over here rather than re-measured.
            cache_request.AddProperty(UIA_ValueValuePropertyId)?;
        }

        let root = crate::inputs::uia_automation::describe_timeout(
            unsafe { automation.ElementFromHandle(hwnd) },
            "ElementFromHandle",
        )?;
        let condition = unsafe { automation.CreateTrueCondition() }?;
        let found = crate::inputs::uia_automation::describe_timeout(
            unsafe { root.FindAllBuildCache(TreeScope_Descendants, &condition, &cache_request) },
            "FindAllBuildCache",
        )?;
        let total = unsafe { found.Length() }?.max(0) as usize;

        let mut candidates = Vec::with_capacity(total);
        let mut elements = Vec::with_capacity(total);
        let mut extra = Vec::with_capacity(total);
        let mut skipped = 0usize;

        for i in 0..total {
            // #590: an element that cannot be fetched is skipped, not fatal.
            let Ok(element) = (unsafe { found.GetElement(i as i32) }) else {
                skipped += 1;
                continue;
            };
            let name = unsafe { element.CachedName() }
                .map(|b| b.to_string())
                .unwrap_or_default();
            let automation_id = unsafe { element.CachedAutomationId() }
                .map(|b| b.to_string())
                .unwrap_or_default();
            let control_type = degraded_control_type(
                unsafe { element.CachedControlType() }.map(control_type_to_string),
            );
            let is_password =
                resolve_password_read(unsafe { element.CachedIsPassword() }.map(|b| b.as_bool()));

            // NEVER read the value of a password field -- checked before any
            // pattern lookup, same guard shape as `inputs::uia::com::extract`.
            let value = if is_password {
                None
            } else {
                unsafe { element.GetCachedPattern(UIA_ValuePatternId) }
                    .ok()
                    .and_then(|unknown| {
                        if is_null(&unknown) {
                            return None;
                        }
                        let pattern: IUIAutomationValuePattern = unknown.cast().ok()?;
                        unsafe { pattern.CachedValue() }.ok().map(|b| b.to_string())
                    })
            };

            // `GetRuntimeId` has no cached form on `IUIAutomationElement`
            // (only a non-cached method exists in the UIA COM interface), so
            // this is one extra round trip per element -- acceptable at this
            // scale (one target window, not a bulk snapshot).
            let runtime_id = unsafe { element.GetRuntimeId() }
                .ok()
                .and_then(|psa| unsafe { runtime_id_from_safearray(psa) }.ok())
                .unwrap_or_default();

            candidates.push(TargetRef {
                hwnd: hwnd.0 as isize,
                runtime_id,
                automation_id,
                name,
                control_type,
            });
            elements.push(element);
            extra.push((is_password, value));
        }

        Ok((candidates, elements, extra, skipped))
    }

    /// Walks, then matches `target` against the walk via the pure
    /// [`resolve_index`] -- errs with a clear, no-em-dash message for "not
    /// found" and "ambiguous", never guesses.
    fn find_and_match(
        automation: &IUIAutomation,
        hwnd: HWND,
        target: &TargetRef,
    ) -> Result<MatchedElement> {
        let (candidates, elements, extra, skipped) = walk(automation, hwnd)?;

        match guard_skipped(resolve_index(&candidates, target), target, skipped) {
            MatchOutcome::Found(i) => {
                let (is_password, value) = extra[i].clone();
                Ok(MatchedElement {
                    element: elements[i].clone(),
                    name: candidates[i].name.clone(),
                    automation_id: candidates[i].automation_id.clone(),
                    control_type: candidates[i].control_type.clone(),
                    is_password,
                    value_readable: value.is_some(),
                    current_text: value.unwrap_or_default(),
                })
            }
            MatchOutcome::NotFound => anyhow::bail!(
                "the target element could not be found; it may have closed, moved, or changed"
            ),
            MatchOutcome::Ambiguous => anyhow::bail!(
                "more than one element matches the target; refusing to guess which one to write to"
            ),
            MatchOutcome::Unconfirmed => anyhow::bail!(
                "some elements in the window could not be read, so the target cannot be confirmed; nothing was written"
            ),
        }
    }

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
        } else if id == UIA_ButtonControlTypeId {
            "Button"
        } else if id == UIA_HyperlinkControlTypeId {
            "Hyperlink"
        } else if id == UIA_MenuItemControlTypeId {
            "MenuItem"
        } else if id == UIA_SplitButtonControlTypeId {
            "SplitButton"
        } else {
            "Other"
        }
        .to_string()
    }

    /// Reads a `GetRuntimeId()` result (an owned `SAFEARRAY` of `VT_I4`
    /// elements) into a plain `Vec<i32>`, freeing the array on every exit
    /// path via `SafeArrayGuard`'s `Drop`.
    unsafe fn runtime_id_from_safearray(psa: *mut SAFEARRAY) -> Result<Vec<i32>> {
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

    fn value_pattern_of(element: &IUIAutomationElement) -> Result<IUIAutomationValuePattern> {
        let unknown = unsafe { element.GetCurrentPattern(UIA_ValuePatternId) }?;
        anyhow::ensure!(
            !is_null(&unknown),
            "the target element does not support ValuePattern; cannot write to it"
        );
        Ok(unknown.cast()?)
    }

    /// One more compare right before `SetValue`, against the control's LIVE
    /// value rather than the walk's cached one (#432): the walk's cache was
    /// filled before the pattern lookup, so this shrinks the remaining gap to
    /// the two adjacent COM calls. A failed live read is not a refusal: the
    /// cached compare in the caller already passed, and a provider that
    /// cannot report a current value would otherwise never be writable.
    fn recheck_live_value(
        pattern: &IUIAutomationValuePattern,
        expected_current_text: &str,
    ) -> Result<()> {
        if let Ok(live) = unsafe { pattern.CurrentValue() } {
            ensure_unchanged(expected_current_text, &live.to_string())?;
        }
        Ok(())
    }

    /// `BSTR::from("")` allocates nothing and yields a raw NULL pointer
    /// (`windows-strings`' own optimization for the empty case -- it never
    /// calls the OS allocator at all when the input slice is empty).
    /// MEASURED 2026-09-17 (`fill_form`'s win32 integration test, restoring
    /// a field back to its original empty text): a plain Win32 EDIT
    /// control's UIA `ValuePattern.SetValue` rejects that NULL BSTR with
    /// E_POINTER (0x80004003), even though NULL is a semantically valid
    /// "empty string" BSTR by the OLE Automation convention -- this
    /// specific provider dereferences it unconditionally. Calling the real
    /// `SysAllocStringLen` with a valid (non-null, zero-length) slice, as
    /// this function does only for the empty case, still asks the OS
    /// allocator for a genuine non-null empty BSTR, which `SetValue`
    /// accepts. Shared by `write` and `write_with_fallback` -- both
    /// `replace_text` (clearing a field, or `Undo` restoring one that
    /// started empty) and `fill_form` (this file's own restore path) can
    /// write an empty string.
    fn bstr_for_value(text: &str) -> BSTR {
        if text.is_empty() {
            unsafe { windows::Win32::Foundation::SysAllocStringLen(Some(&[])) }
        } else {
            BSTR::from(text)
        }
    }

    fn to_unicode_input(ev: &UnicodeKeyEvent) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: ev.code_unit,
                    dwFlags: if ev.key_up {
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                    } else {
                        KEYEVENTF_UNICODE
                    },
                    time: 0,
                    dwExtraInfo: INJECTED_MARKER,
                },
            },
        }
    }

    /// The one real `SendInput` call, returning how many events were
    /// accepted. Never called by this module's own automated tests (tests use
    /// a fake sender): it would type into whatever window has focus.
    fn real_sender(inputs: &[INPUT]) -> usize {
        unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) as usize }
    }

    /// Types the planned Unicode events through the real sender. Never called
    /// by this module's own automated tests -- see the module doc comment's
    /// "The typed-input fallback" section; covered by [`plan_typed_input`]'s
    /// pure tests plus the manual check filed to #166. `dwExtraInfo` is tagged
    /// with `crate::hotkey::INJECTED_MARKER` (issue #209: the one constant
    /// every `SendInput` call site shares) so `hook_proc` never treats these
    /// keystrokes as a real hotkey press.
    fn inject_unicode_events(events: &[UnicodeKeyEvent]) -> Result<()> {
        let inputs: Vec<INPUT> = events.iter().map(to_unicode_input).collect();
        send_all(&inputs, &real_sender).map_err(|e| {
            anyhow::anyhow!("typing was cut short and the field may hold partial text: {e}")
        })
    }

    /// Sends a batch, treating a short count (blocked by UIPI, another
    /// desktop, a full queue) as an error instead of ignoring it. `sender`
    /// returns how many events were accepted.
    fn send_all(inputs: &[INPUT], sender: &dyn Fn(&[INPUT]) -> usize) -> Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        let sent = sender(inputs);
        anyhow::ensure!(
            sent == inputs.len(),
            "the keystrokes could not be sent ({sent} of {})",
            inputs.len()
        );
        Ok(())
    }

    /// Ctrl+A. On a short count A or Ctrl may be left down, so key-ups for
    /// both are sent best-effort before reporting the error.
    fn send_select_all(sender: &dyn Fn(&[INPUT]) -> usize) -> Result<()> {
        let r = send_all(
            &[
                vk_input(VK_CONTROL, false),
                vk_input(VK_A, false),
                vk_input(VK_A, true),
                vk_input(VK_CONTROL, true),
            ],
            sender,
        );
        if r.is_err() {
            sender(&[vk_input(VK_A, true), vk_input(VK_CONTROL, true)]);
        }
        r
    }
    fn vk_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if key_up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    time: 0,
                    dwExtraInfo: INJECTED_MARKER,
                },
            },
        }
    }

    /// Reads the live focus facts and hands them to the pure
    /// [`confirm_typed_focus`]. Any read that fails becomes `None`, which
    /// refuses.
    ///
    /// Focus arrives asynchronously after `SetFocus` (slow apps such as
    /// browsers), so a failed probe is retried once after a short one-off
    /// wait before refusing.
    fn probe_typed_focus(
        automation: &IUIAutomation,
        element: &IUIAutomationElement,
        hwnd: HWND,
    ) -> Result<()> {
        if probe_typed_focus_once(automation, element, hwnd).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(75));
        probe_typed_focus_once(automation, element, hwnd)
    }

    fn probe_typed_focus_once(
        automation: &IUIAutomation,
        element: &IUIAutomationElement,
        hwnd: HWND,
    ) -> Result<()> {
        let same = unsafe { automation.GetFocusedElement() }
            .ok()
            .and_then(|focused| unsafe { automation.CompareElements(&focused, element) }.ok())
            .map(|b| b.as_bool());
        let root_of = |w: HWND| -> Option<isize> {
            if w.0.is_null() {
                return None;
            }
            let root = unsafe { GetAncestor(w, GA_ROOT) };
            (!root.0.is_null()).then_some(root.0 as isize)
        };
        let foreground = root_of(unsafe { GetForegroundWindow() });
        confirm_typed_focus(same, foreground, root_of(hwnd))
    }

    /// Runs a [`TypedReplacePlan`]. Select-all (and delete) is sent first and
    /// on its own: if it cannot be sent, nothing is typed and this errs, so a
    /// failed clear never turns into an append (#592).
    ///
    /// `still_focused` is checked again after the select-all and before
    /// anything is deleted or typed (#676): focus can move in between.
    fn inject_typed_replace(
        plan: &TypedReplacePlan,
        still_focused: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        if plan.select_all {
            send_select_all(&real_sender).map_err(|e| {
                anyhow::anyhow!("could not select the existing text, so nothing was typed: {e}")
            })?;
            still_focused()?;
        }
        if plan.delete_selection {
            send_all(
                &[vk_input(VK_BACK, false), vk_input(VK_BACK, true)],
                &real_sender,
            )?;
        }
        inject_unicode_events(&plan.text)
    }

    /// The real, UIA-backed [`TextElementAccess`]. Each call is a
    /// self-contained COM apartment enter/walk/exit -- never shares a live
    /// COM pointer across two calls, so `resolve`/`write`/`write_with_fallback`
    /// are each independently safe to call at any time.
    pub struct UiaTextElementAccess;

    impl TextElementAccess for UiaTextElementAccess {
        fn resolve(&self, target: &TargetRef) -> Result<ResolvedElement> {
            let _apartment = ComApartment::enter()?;
            let automation: IUIAutomation =
                unsafe { crate::inputs::uia_automation::create_automation() }?;
            let hwnd = HWND(target.hwnd as *mut core::ffi::c_void);

            let matched = find_and_match(&automation, hwnd, target)?;
            Ok(ResolvedElement {
                name: matched.name,
                automation_id: matched.automation_id,
                control_type: matched.control_type,
                is_password: matched.is_password,
                current_text: matched.current_text,
                value_readable: matched.value_readable,
            })
        }

        fn write(
            &self,
            target: &TargetRef,
            expected_current_text: &str,
            new_text: &str,
        ) -> Result<()> {
            let _apartment = ComApartment::enter()?;
            let automation: IUIAutomation =
                unsafe { crate::inputs::uia_automation::create_automation() }?;
            let hwnd = HWND(target.hwnd as *mut core::ffi::c_void);

            let matched = find_and_match(&automation, hwnd, target)?;
            anyhow::ensure!(
                !matched.is_password,
                "refusing to write to a password field"
            );
            ensure_unchanged(expected_current_text, &matched.current_text)?;
            let pattern = value_pattern_of(&matched.element)?;
            recheck_live_value(&pattern, expected_current_text)?;
            unsafe { pattern.SetValue(&bstr_for_value(new_text)) }?;
            Ok(())
        }

        fn write_with_fallback(
            &self,
            target: &TargetRef,
            expected_current_text: &str,
            new_text: &str,
        ) -> Result<()> {
            let _apartment = ComApartment::enter()?;
            let automation: IUIAutomation =
                unsafe { crate::inputs::uia_automation::create_automation() }?;
            let hwnd = HWND(target.hwnd as *mut core::ffi::c_void);

            let matched = find_and_match(&automation, hwnd, target)?;
            anyhow::ensure!(
                !matched.is_password,
                "refusing to write to a password field"
            );
            ensure_unchanged(expected_current_text, &matched.current_text)?;

            match value_pattern_of(&matched.element) {
                Ok(pattern) => {
                    recheck_live_value(&pattern, expected_current_text)?;
                    unsafe { pattern.SetValue(&bstr_for_value(new_text)) }?;
                    Ok(())
                }
                Err(e) => {
                    let plan = plan_typed_fallback(&matched.control_type, new_text)?;
                    // Focus, then type via SendInput Unicode injection.
                    // plan_typed_input never emits Enter/Tab, so this can
                    // never submit or focus-advance out of the field.
                    unsafe { matched.element.SetFocus() }.map_err(|_| e)?;
                    // #676: SetFocus returning Ok does not mean the keyboard
                    // is in this field or that its window is in front.
                    let focused = || probe_typed_focus(&automation, &matched.element, hwnd);
                    focused()?;
                    inject_typed_replace(&plan, &focused)
                }
            }
        }
    }

    /// Test-only seam: the full candidate list for a window, so an
    /// integration test can capture a real, exact [`TargetRef`] (including
    /// its real `runtime_id`) the same way a future "Look" step would,
    /// instead of hand-constructing one. Shared by `replace_text.rs`'s and
    /// `fill_form.rs`'s win32 integration tests.
    #[cfg(test)]
    pub fn list_all_for_test(hwnd: HWND) -> Result<Vec<TargetRef>> {
        let _apartment = ComApartment::enter()?;
        let automation: IUIAutomation =
            unsafe { crate::inputs::uia_automation::create_automation() }?;
        let (candidates, _elements, _extra, _skipped) = walk(&automation, hwnd)?;
        Ok(candidates)
    }

    /// Test-only: drives `send_select_all` with a fake sender, never
    /// `SendInput`. Each call is recorded as (event count, is a Ctrl key-up).
    #[cfg(test)]
    pub mod test_support {
        use super::*;
        use std::cell::RefCell;

        fn run(accept: usize) -> (Result<()>, Vec<(usize, bool)>) {
            let calls = RefCell::new(Vec::new());
            let sender = |inputs: &[INPUT]| {
                let is_up = |i: &INPUT, vk: VIRTUAL_KEY| unsafe {
                    i.Anonymous.ki.wVk == vk
                        && i.Anonymous.ki.dwFlags == KEYEVENTF_KEYUP
                        && i.Anonymous.ki.dwExtraInfo == INJECTED_MARKER
                };
                let up =
                    inputs.len() == 2 && is_up(&inputs[0], VK_A) && is_up(&inputs[1], VK_CONTROL);
                calls.borrow_mut().push((inputs.len(), up));
                accept.min(inputs.len())
            };
            let r = send_select_all(&sender);
            (r, calls.into_inner())
        }

        pub fn select_all_with_short_count() -> (Result<()>, Vec<(usize, bool)>) {
            run(2)
        }

        pub fn select_all_with_full_count() -> (Result<()>, Vec<(usize, bool)>) {
            run(4)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_target() -> TargetRef {
        TargetRef {
            hwnd: 12345,
            runtime_id: vec![1, 2, 3],
            automation_id: "editField".to_string(),
            name: "Notes".to_string(),
            control_type: "Edit".to_string(),
        }
    }

    // -- resolve_index ------------------------------------------------------

    #[test]
    fn finds_the_single_exact_match() {
        let target = sample_target();
        let other = TargetRef {
            name: "Other field".to_string(),
            ..target.clone()
        };
        let candidates = vec![other, target.clone()];
        assert_eq!(resolve_index(&candidates, &target), MatchOutcome::Found(1));
    }

    #[test]
    fn reports_not_found_when_nothing_matches() {
        let target = sample_target();
        let candidates = vec![TargetRef {
            name: "Different".to_string(),
            ..target.clone()
        }];
        assert_eq!(resolve_index(&candidates, &target), MatchOutcome::NotFound);
    }

    #[test]
    fn reports_ambiguous_when_more_than_one_candidate_matches() {
        let target = sample_target();
        let candidates = vec![target.clone(), target.clone()];
        assert_eq!(resolve_index(&candidates, &target), MatchOutcome::Ambiguous);
    }

    // -- ensure_unchanged / StaleTarget (#432) --------------------------

    #[test]
    fn ensure_unchanged_accepts_equal_text() {
        assert!(ensure_unchanged("same", "same").is_ok());
        assert!(ensure_unchanged("", "").is_ok());
    }

    #[test]
    fn ensure_unchanged_refuses_changed_text_with_a_typed_error() {
        let err = ensure_unchanged("before", "after").expect_err("must refuse");
        assert!(is_stale_target_error(&err));
        assert!(err.to_string().contains("changed"), "{err}");
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }

    #[test]
    fn a_stale_target_error_is_still_recognised_through_added_context() {
        use anyhow::Context;
        let err = ensure_unchanged("a", "b")
            .context("outer")
            .expect_err("must refuse");
        assert!(is_stale_target_error(&err));
        assert!(!is_stale_target_error(&anyhow!("some other failure")));
    }

    // -- is_stale -------------------------------------------------------

    #[test]
    fn identical_text_is_not_stale() {
        assert!(!is_stale("Hello world", "Hello world"));
    }

    #[test]
    fn different_text_is_stale() {
        assert!(is_stale("Hello world", "Hello there"));
    }

    // -- parse_target_ref -------------------------------------------------

    #[test]
    fn parses_a_full_target() {
        let target = sample_target();
        let value = serde_json::json!({
            "hwnd": target.hwnd,
            "runtime_id": target.runtime_id,
            "automation_id": target.automation_id,
            "name": target.name,
            "control_type": target.control_type,
        });
        assert_eq!(parse_target_ref(&value).unwrap(), target);
    }

    #[test]
    fn missing_hwnd_is_a_named_error_not_a_panic() {
        let value = serde_json::json!({"name": "Notes"});
        let err = parse_target_ref(&value).expect_err("no hwnd must error");
        assert!(err.to_string().contains("hwnd"));
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }

    #[test]
    fn missing_optional_fields_default_to_empty() {
        let value = serde_json::json!({"hwnd": 1});
        let target = parse_target_ref(&value).unwrap();
        assert_eq!(target.automation_id, "");
        assert_eq!(target.name, "");
        assert_eq!(target.control_type, "");
        assert!(target.runtime_id.is_empty());
    }

    // -- is_invokable_control_type -----------------------------------------

    #[test]
    fn button_like_control_types_are_invokable() {
        for ct in ["Button", "Hyperlink", "MenuItem", "SplitButton"] {
            assert!(is_invokable_control_type(ct), "{ct} should be invokable");
        }
    }

    #[test]
    fn editable_control_types_are_not_invokable() {
        for ct in ["Edit", "Document", "ComboBox", "CheckBox", "Text", "Other"] {
            assert!(
                !is_invokable_control_type(ct),
                "{ct} should not be invokable"
            );
        }
    }

    // -- plan_typed_input (SendInput fallback planning, purely) -------------

    #[test]
    fn plans_a_down_up_pair_per_character() {
        let events = plan_typed_input("ab");
        assert_eq!(
            events,
            vec![
                UnicodeKeyEvent {
                    code_unit: b'a' as u16,
                    key_up: false
                },
                UnicodeKeyEvent {
                    code_unit: b'a' as u16,
                    key_up: true
                },
                UnicodeKeyEvent {
                    code_unit: b'b' as u16,
                    key_up: false
                },
                UnicodeKeyEvent {
                    code_unit: b'b' as u16,
                    key_up: true
                },
            ]
        );
    }

    #[test]
    fn empty_text_plans_no_events() {
        assert!(plan_typed_input("").is_empty());
    }

    #[test]
    fn never_plans_enter_tab_or_carriage_return() {
        let events = plan_typed_input("a\nb\tc\rd");
        let code_units: Vec<u16> = events.iter().map(|e| e.code_unit).collect();
        assert!(!code_units.contains(&0x0A), "must never plan a newline");
        assert!(!code_units.contains(&0x09), "must never plan a tab");
        assert!(
            !code_units.contains(&0x0D),
            "must never plan a carriage return"
        );
        // The letters around the stripped control characters are still
        // planned, one down/up pair each -- this is a filter, not a
        // refusal of the whole string.
        let down_units: Vec<u16> = events
            .iter()
            .filter(|e| !e.key_up)
            .map(|e| e.code_unit)
            .collect();
        assert_eq!(
            down_units,
            vec![b'a' as u16, b'b' as u16, b'c' as u16, b'd' as u16]
        );
    }

    #[test]
    fn plans_both_utf16_code_units_of_a_surrogate_pair() {
        // A grinning-face emoji is one surrogate pair: two DIFFERENT code
        // units (a high surrogate, then a low surrogate), so four events --
        // a down/up pair for the high surrogate, then a down/up pair for
        // the low surrogate.
        let events = plan_typed_input("\u{1F600}");
        assert_eq!(events.len(), 4);
        // events[0] and [1] are the same (high-surrogate) unit's down/up.
        assert_eq!(events[0].code_unit, events[1].code_unit);
        assert!(!events[0].key_up && events[1].key_up);
        // events[2] and [3] are the same (low-surrogate) unit's down/up.
        assert_eq!(events[2].code_unit, events[3].code_unit);
        assert!(!events[2].key_up && events[3].key_up);
        // The two surrogate halves are different code units.
        assert_ne!(events[0].code_unit, events[2].code_unit);
    }

    // -- plan_typed_fallback (#592: the fallback replaces, never appends) ---

    #[test]
    fn fallback_always_selects_all_because_expected_is_empty_without_a_value_pattern() {
        // The real path: no ValuePattern means the walk reads "" for a field
        // that may still hold text, so the plan cannot depend on that text.
        let plan = plan_typed_fallback("Edit", "new").unwrap();
        assert!(plan.select_all, "must select existing text first");
        assert!(!plan.delete_selection);
        assert_eq!(plan.text, plan_typed_input("new"));
    }

    #[test]
    fn fallback_deletes_the_selection_when_the_new_value_is_empty() {
        let plan = plan_typed_fallback("Edit", "").unwrap();
        assert!(plan.select_all && plan.delete_selection && plan.text.is_empty());
    }

    #[test]
    fn fallback_refuses_document_and_other_control_types() {
        for ct in ["Document", "ComboBox", "Button", "Pane", ""] {
            let err = plan_typed_fallback(ct, "x").unwrap_err().to_string();
            assert!(err.contains("type the value yourself"), "{ct}: {err}");
            assert!(!err.contains('\u{2014}'), "no em dash in a card string");
        }
    }

    #[test]
    fn fallback_keeps_the_never_enter_or_tab_rule() {
        let plan = plan_typed_fallback("Edit", "a\nb\tc").unwrap();
        let units: Vec<u16> = plan.text.iter().map(|e| e.code_unit).collect();
        assert!(!units.contains(&0x0A) && !units.contains(&0x09));
    }

    // -- #590: one unreadable element costs only itself ---------------------

    #[test]
    fn focus_is_confirmed_only_when_every_check_ran_and_agrees() {
        assert!(confirm_typed_focus(Some(true), Some(7), Some(7)).is_ok());
        // The focused element is not the target.
        assert!(confirm_typed_focus(Some(false), Some(7), Some(7)).is_err());
        // Another window is in front.
        assert!(confirm_typed_focus(Some(true), Some(8), Some(7)).is_err());
        // No foreground window at all (0 is the null handle).
        assert!(confirm_typed_focus(Some(true), Some(0), Some(0)).is_err());
    }

    #[test]
    fn focus_check_that_could_not_run_fails_closed() {
        assert!(confirm_typed_focus(None, Some(7), Some(7)).is_err());
        assert!(confirm_typed_focus(Some(true), None, Some(7)).is_err());
        assert!(confirm_typed_focus(Some(true), Some(7), None).is_err());
        let msg = confirm_typed_focus(Some(false), Some(7), Some(7))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("typed nothing into it") && !msg.contains('\u{2014}'));
        let front = confirm_typed_focus(Some(true), Some(8), Some(7))
            .unwrap_err()
            .to_string();
        assert!(front.contains("not in front"), "{front}");
    }

    #[test]
    fn an_unreadable_control_type_degrades_to_other() {
        assert_eq!(degraded_control_type::<()>(Err(())), "Other");
        assert_eq!(degraded_control_type::<()>(Ok("Edit".to_string())), "Edit");
    }

    #[test]
    fn an_unreadable_password_flag_fails_closed() {
        assert!(resolve_password_read::<()>(Err(())));
        assert!(!resolve_password_read::<()>(Ok(false)));
        assert!(resolve_password_read::<()>(Ok(true)));
    }

    #[test]
    fn only_other_is_the_unknown_control_type() {
        assert!(is_unknown_control_type("Other"));
        assert!(!is_unknown_control_type("Edit"));
        assert!(!is_unknown_control_type(""));
    }

    #[test]
    fn a_degraded_element_never_matches_an_edit_target() {
        let target = sample_target(); // control_type "Edit"
        let degraded = TargetRef {
            control_type: degraded_control_type::<()>(Err(())),
            ..target.clone()
        };
        assert_eq!(
            resolve_index(&[degraded], &target),
            MatchOutcome::NotFound,
            "a degraded element must not become the write target by accident"
        );
    }

    #[test]
    fn skipping_an_element_does_not_change_which_element_is_matched() {
        // Matching is by identity, never by position: dropping an element
        // from the walk shifts indices, but the match is still the element
        // equal to the target, and the walk's parallel vectors stay aligned.
        let target = sample_target();
        let a = TargetRef {
            name: "A".to_string(),
            ..target.clone()
        };
        let b = TargetRef {
            name: "B".to_string(),
            ..target.clone()
        };
        let full = vec![a.clone(), b.clone(), target.clone()];
        let skipped_one = vec![a, target.clone()];
        let MatchOutcome::Found(i) = resolve_index(&full, &target) else {
            panic!("full walk must find the target");
        };
        let MatchOutcome::Found(j) = resolve_index(&skipped_one, &target) else {
            panic!("walk with a skipped element must find the target");
        };
        assert_ne!(i, j, "the slot did shift");
        assert_eq!(full[i], skipped_one[j]);
    }

    #[test]
    fn a_skipped_element_makes_an_identity_only_match_unconfirmed() {
        // A skipped element's identity is unknown: with no runtime id to
        // tell it apart, a unique match may only be unique because the twin
        // was skipped, so refuse.
        let no_rid = TargetRef {
            runtime_id: vec![],
            ..sample_target()
        };
        assert_eq!(
            guard_skipped(MatchOutcome::Found(0), &no_rid, 1),
            MatchOutcome::Unconfirmed
        );
        assert_eq!(
            guard_skipped(MatchOutcome::Found(0), &no_rid, 0),
            MatchOutcome::Found(0)
        );
        let with_rid = sample_target();
        assert_eq!(
            guard_skipped(MatchOutcome::Found(0), &with_rid, 3),
            MatchOutcome::Found(0),
            "a runtime id is unique per element, so skips cannot hide a twin"
        );
        assert_eq!(
            guard_skipped(MatchOutcome::NotFound, &no_rid, 2),
            MatchOutcome::NotFound
        );
    }
}

#[cfg(test)]
mod send_tests {
    use super::com::test_support::*;

    #[test]
    fn short_select_all_batch_releases_ctrl_and_errs() {
        let (r, calls) = select_all_with_short_count();
        assert!(r.is_err());
        // The 4-event batch, then key-ups for A and Ctrl.
        assert_eq!(calls, vec![(4, false), (2, true)]);
    }

    #[test]
    fn full_select_all_batch_sends_once() {
        let (r, calls) = select_all_with_full_count();
        assert!(r.is_ok());
        assert_eq!(calls, vec![(4, false)]);
    }
}
