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
//! exposes no `ValuePattern` and is editable ([`is_editable_control_type`]).
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
}

/// Result of matching a [`TargetRef`] against a fresh walk of candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOutcome {
    Found(usize),
    NotFound,
    Ambiguous,
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

/// Whether the element's current text has drifted from what the preview
/// showed -- the stale-target check both callers need ("someone typed in
/// between").
pub fn is_stale(expected_current_text: &str, actual_current_text: &str) -> bool {
    expected_current_text != actual_current_text
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

/// Whether a UIA control type is the kind of thing typed text can go into --
/// used only to decide whether [`TextElementAccess::write_with_fallback`]'s
/// typed-input fallback is even worth attempting for a control with no
/// `ValuePattern`. Deliberately narrow (the three control types this crate's
/// `control_type_to_string` maps to something text-editable); a control type
/// not in this list refuses rather than guessing.
pub fn is_editable_control_type(control_type: &str) -> bool {
    matches!(control_type, "Edit" | "Document" | "ComboBox")
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
    /// doc comment) and writes `new_text` via `ValuePattern.SetValue`. Errs
    /// if the element cannot be found, is a password field, or does not
    /// support `ValuePattern`.
    fn write(&self, target: &TargetRef, new_text: &str) -> Result<()>;

    /// Same as [`write`](Self::write), but for `fill_form` (#33) only: when
    /// the real implementation finds no `ValuePattern` and the control is
    /// editable, falls back to focus plus typed Unicode input instead of
    /// refusing outright. The default implementation is plain `write` --
    /// every fake gets this for free, and `replace_text` never calls this
    /// method at all (see the module doc comment).
    fn write_with_fallback(&self, target: &TargetRef, new_text: &str) -> Result<()> {
        self.write(target, new_text)
    }
}

// ---------------------------------------------------------------------------
// Win32 / UI Automation
// ---------------------------------------------------------------------------

pub mod com {
    #![allow(dead_code)]

    use super::{
        is_editable_control_type, plan_typed_input, resolve_index, MatchOutcome, ResolvedElement,
        TargetRef, TextElementAccess, UnicodeKeyEvent,
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
        VIRTUAL_KEY,
    };

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
    }

    /// One walked candidate's non-identity extras (password/value), kept
    /// parallel to `walk`'s `Vec<TargetRef>`/`Vec<IUIAutomationElement>`
    /// rather than folded into `TargetRef` itself, since `TargetRef` is pure
    /// identity data shared with the JSON proposal shape.
    type WalkExtra = (bool, Option<String>);

    /// `walk`'s result: parallel `TargetRef`/element/extra vectors, one
    /// entry per candidate, in the same order.
    type WalkResult = (Vec<TargetRef>, Vec<IUIAutomationElement>, Vec<WalkExtra>);

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

        for i in 0..total {
            let element = unsafe { found.GetElement(i as i32) }?;
            let name = unsafe { element.CachedName() }
                .map(|b| b.to_string())
                .unwrap_or_default();
            let automation_id = unsafe { element.CachedAutomationId() }
                .map(|b| b.to_string())
                .unwrap_or_default();
            let control_type = control_type_to_string(unsafe { element.CachedControlType() }?);
            let is_password = unsafe { element.CachedIsPassword() }
                .map(|b| b.as_bool())
                .unwrap_or(false);

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

        Ok((candidates, elements, extra))
    }

    /// Walks, then matches `target` against the walk via the pure
    /// [`resolve_index`] -- errs with a clear, no-em-dash message for "not
    /// found" and "ambiguous", never guesses.
    fn find_and_match(
        automation: &IUIAutomation,
        hwnd: HWND,
        target: &TargetRef,
    ) -> Result<MatchedElement> {
        let (candidates, elements, extra) = walk(automation, hwnd)?;

        match resolve_index(&candidates, target) {
            MatchOutcome::Found(i) => {
                let (is_password, value) = extra[i].clone();
                Ok(MatchedElement {
                    element: elements[i].clone(),
                    name: candidates[i].name.clone(),
                    automation_id: candidates[i].automation_id.clone(),
                    control_type: candidates[i].control_type.clone(),
                    is_password,
                    current_text: value.unwrap_or_default(),
                })
            }
            MatchOutcome::NotFound => anyhow::bail!(
                "the target element could not be found; it may have closed, moved, or changed"
            ),
            MatchOutcome::Ambiguous => anyhow::bail!(
                "more than one element matches the target; refusing to guess which one to write to"
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

    /// `SendInput`s the typed-input fallback plan. Never called by this
    /// module's own automated tests -- see the module doc comment's "The
    /// typed-input fallback" section and `fill_form.rs`'s tests, none of
    /// which target a control lacking `ValuePattern`, so this function is
    /// only ever reached in production. Covered by [`plan_typed_input`]'s
    /// own pure tests plus the manual check filed to #166. `dwExtraInfo` is
    /// tagged with `crate::hotkey::INJECTED_MARKER` (issue #209: the single
    /// constant every `SendInput` call site in the crate now shares) so
    /// `hook_proc` never treats this typed fallback's own keystrokes as a
    /// real hotkey press.
    fn inject_unicode_events(events: &[UnicodeKeyEvent]) {
        let inputs: Vec<INPUT> = events.iter().map(to_unicode_input).collect();
        if inputs.is_empty() {
            return;
        }
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
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
            })
        }

        fn write(&self, target: &TargetRef, new_text: &str) -> Result<()> {
            let _apartment = ComApartment::enter()?;
            let automation: IUIAutomation =
                unsafe { crate::inputs::uia_automation::create_automation() }?;
            let hwnd = HWND(target.hwnd as *mut core::ffi::c_void);

            let matched = find_and_match(&automation, hwnd, target)?;
            anyhow::ensure!(
                !matched.is_password,
                "refusing to write to a password field"
            );
            let pattern = value_pattern_of(&matched.element)?;
            unsafe { pattern.SetValue(&bstr_for_value(new_text)) }?;
            Ok(())
        }

        fn write_with_fallback(&self, target: &TargetRef, new_text: &str) -> Result<()> {
            let _apartment = ComApartment::enter()?;
            let automation: IUIAutomation =
                unsafe { crate::inputs::uia_automation::create_automation() }?;
            let hwnd = HWND(target.hwnd as *mut core::ffi::c_void);

            let matched = find_and_match(&automation, hwnd, target)?;
            anyhow::ensure!(
                !matched.is_password,
                "refusing to write to a password field"
            );

            match value_pattern_of(&matched.element) {
                Ok(pattern) => {
                    unsafe { pattern.SetValue(&bstr_for_value(new_text)) }?;
                    Ok(())
                }
                Err(e) => {
                    anyhow::ensure!(
                        is_editable_control_type(&matched.control_type),
                        "the target element does not support ValuePattern and is not a known editable control type; cannot write to it"
                    );
                    // Focus, then type via SendInput Unicode injection.
                    // plan_typed_input never emits Enter/Tab, so this can
                    // never submit or focus-advance out of the field.
                    unsafe { matched.element.SetFocus() }.map_err(|_| e)?;
                    let events = plan_typed_input(new_text);
                    inject_unicode_events(&events);
                    Ok(())
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
        let (candidates, _elements, _extra) = walk(&automation, hwnd)?;
        Ok(candidates)
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

    // -- is_editable_control_type --------------------------------------

    #[test]
    fn edit_document_combobox_are_editable() {
        for ct in ["Edit", "Document", "ComboBox"] {
            assert!(is_editable_control_type(ct), "{ct} should be editable");
        }
    }

    #[test]
    fn button_and_other_are_not_editable() {
        for ct in ["Button", "CheckBox", "Text", "Other", ""] {
            assert!(!is_editable_control_type(ct), "{ct} should not be editable");
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
}
