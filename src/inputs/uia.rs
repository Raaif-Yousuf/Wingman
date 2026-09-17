//! A flat snapshot of the foreground window's editable controls via UI
//! Automation (#27, expansion plan §4's `inputs/uia.rs` row: "the foreground
//! window's editable controls as a flat list: automation id, name, label,
//! control type, current value, bounding rect" / "must not: write
//! anything"). The future consumer is `executors::uia_guard::is_forbidden_target`
//! (#32/#33): this module only reads and reports, it never targets or
//! clicks anything.
//!
//! Two layers, deliberately kept apart so the interesting logic is
//! unit-testable without a live window or a COM apartment:
//!
//! - **Pure** (top of this file): [`RawElement`] is a plain description of
//!   one element observed while walking the tree in document order --
//!   label resolution ([`build_snapshot`]), the truncation boundary
//!   ([`bounded_count`]) and the compact text serialization
//!   ([`format_compact`]) all operate on `&[RawElement]` /
//!   `&[FieldSnapshot]`, no `windows` COM types anywhere in their
//!   signatures. Tested with synthetic data in `mod tests`.
//! - **Win32** ([`com`]): `CoCreateInstance(CUIAutomation8)`, a single
//!   `CacheRequest` + `FindAllBuildCache` over one condition (not a
//!   per-element property round trip -- see `com::walk`'s doc comment for
//!   why the condition is `True`/`Descendants`, not a control-type filter),
//!   and the property extraction that turns one `IUIAutomationElement`
//!   into a [`RawElement`]. Exercised by the real-window integration test
//!   at the bottom of `mod tests`.
//!
//! # Threading (CLAUDE.md rule 8, the task brief)
//!
//! [`snapshot_hwnd`] and [`snapshot_foreground`] initialize COM
//! apartment-threaded (`COINIT_APARTMENTTHREADED`) and uninitialize it
//! before returning, via the RAII guard `com::ComApartment`. **Call these
//! from a dedicated worker thread, never from the low-level keyboard hook's
//! thread**: a blocking COM call into another process's UI Automation
//! provider (a hung app, a slow browser) can take seconds, and the hook
//! thread must never block or every application on the desktop stops
//! receiving keyboard input for as long as the call takes.
//!
//! Nothing in `app.rs` calls anything in this module yet -- wiring a real
//! key press or palette action to `snapshot_foreground` is a later issue
//! (the same status `executors/` had until #31's registry landed, and
//! `executors::uia_guard::is_forbidden_target` still has today). Every
//! public item below carries `#[allow(dead_code)]` for that reason; they
//! are all exercised by this file's own tests.

use std::time::Duration;

use windows::Win32::Foundation::HWND;

// ---------------------------------------------------------------------------
// Pure types
// ---------------------------------------------------------------------------

/// The six control types the task names. Only elements of one of these
/// kinds are emitted as a [`FieldSnapshot`]; everything else (buttons,
/// text/label elements, panes, ...) is walked only so it can serve as a
/// label source for [`build_snapshot`]'s "nearest preceding Text element"
/// fallback, never emitted itself.
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    Edit,
    ComboBox,
    Document,
    CheckBox,
    RadioButton,
    List,
}

#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
impl ControlKind {
    fn from_raw(raw: RawControlType) -> Option<Self> {
        match raw {
            RawControlType::Edit => Some(ControlKind::Edit),
            RawControlType::ComboBox => Some(ControlKind::ComboBox),
            RawControlType::Document => Some(ControlKind::Document),
            RawControlType::CheckBox => Some(ControlKind::CheckBox),
            RawControlType::RadioButton => Some(ControlKind::RadioButton),
            RawControlType::List => Some(ControlKind::List),
            RawControlType::Text | RawControlType::Other => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ControlKind::Edit => "Edit",
            ControlKind::ComboBox => "ComboBox",
            ControlKind::Document => "Document",
            ControlKind::CheckBox => "CheckBox",
            ControlKind::RadioButton => "RadioButton",
            ControlKind::List => "List",
        }
    }
}

/// Every UIA control type this module distinguishes while walking the tree.
/// Broader than [`ControlKind`] on purpose: `Text` is not one of the six
/// fields the task asks for, but it is exactly what "nearest preceding Text
/// element" means, so the walk has to recognize it too. `Other` covers
/// everything else (buttons, panes, ...), walked past but never labeled
/// from and never emitted.
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RawControlType {
    Edit,
    ComboBox,
    Document,
    CheckBox,
    RadioButton,
    List,
    Text,
    #[default]
    Other,
}

/// One field's current value. A separate `Redacted` variant rather than a
/// `String` containing a placeholder: it is a type error to accidentally
/// serialize a redacted field's *real* text, because there is no real text
/// stored in this variant to serialize.
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Text(String),
    /// `IsPassword` was true. The real value was never read into this
    /// process for this element -- see [`com::extract`]'s doc comment.
    Redacted,
    Empty,
}

#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl From<windows::Win32::Foundation::RECT> for Rect {
    fn from(r: windows::Win32::Foundation::RECT) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSnapshot {
    pub automation_id: String,
    pub name: String,
    /// Resolved by [`build_snapshot`]: `LabeledBy`'s name, else the nearest
    /// preceding Text element's name in tree order, else `HelpText`, else
    /// empty.
    pub label: String,
    pub control_type: ControlKind,
    pub value: FieldValue,
    pub rect: Rect,
    pub enabled: bool,
    pub focusable: bool,
}

/// The result of one walk: whatever fields were found within the element
/// and time budget, plus whether the budget cut the walk short.
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub fields: Vec<FieldSnapshot>,
    pub truncated: bool,
}

/// One element observed while walking the tree, in document (tree) order,
/// before any Windows-specific type appears -- what [`com::extract`]
/// produces from a real `IUIAutomationElement`, and what the tests in this
/// file construct directly to exercise [`build_snapshot`] without any COM
/// call.
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
#[derive(Debug, Clone, Default)]
struct RawElement {
    automation_id: String,
    name: String,
    help_text: String,
    control_type: RawControlType,
    /// The name of the element `LabeledBy` points at, already resolved to a
    /// plain string (empty/absent filtered out) -- see [`com::extract`]'s
    /// null-element guard for why this can never be the literal text of an
    /// unset relationship.
    labeled_by_name: Option<String>,
    is_password: bool,
    /// `None` for a control with no supported `ValuePattern` (or a
    /// password field, which never even attempts to read it -- see
    /// [`com::extract`]).
    value: Option<String>,
    rect: Rect,
    enabled: bool,
    focusable: bool,
}

#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
pub const DEFAULT_MAX_ELEMENTS: usize = 500;
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
pub const DEFAULT_BUDGET: Duration = Duration::from_millis(750);

// ---------------------------------------------------------------------------
// Pure logic: label resolution, truncation boundary, serialization
// ---------------------------------------------------------------------------

/// Turns a document-order walk of the tree into the flat list the task asks
/// for. Pure: no I/O, no COM, nothing but a single forward pass.
///
/// Label resolution per element, in priority order: `LabeledBy`'s name (if
/// non-empty), else the **nearest** preceding `Text`-kind element's name
/// (not the first -- `last_text` is overwritten every time a new Text
/// element with a non-empty name is walked), else `HelpText`, else empty.
/// Non-editable elements (including Text elements themselves) are never
/// emitted as fields; they only ever update `last_text`.
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
fn build_snapshot(elements: &[RawElement]) -> Vec<FieldSnapshot> {
    let mut last_text: Option<&str> = None;
    let mut fields = Vec::new();

    for el in elements {
        if el.control_type == RawControlType::Text {
            if !el.name.is_empty() {
                last_text = Some(el.name.as_str());
            }
            continue;
        }

        let Some(control_type) = ControlKind::from_raw(el.control_type) else {
            continue;
        };

        let label = el
            .labeled_by_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(last_text)
            .filter(|s| !s.is_empty())
            .or_else(|| Some(el.help_text.as_str()).filter(|s| !s.is_empty()))
            .unwrap_or("")
            .to_string();

        // Defense in depth: even if a `RawElement` somehow carried both
        // `is_password: true` and a real `value` (it never does today --
        // `com::extract` never reads the pattern for a password field --
        // but this function has no way to know that from its own inputs),
        // redaction wins. There is exactly one place in this file that can
        // produce `FieldValue::Text` for a password field, and it is not
        // here.
        let value = if el.is_password {
            FieldValue::Redacted
        } else {
            match &el.value {
                Some(v) if !v.is_empty() => FieldValue::Text(v.clone()),
                _ => FieldValue::Empty,
            }
        };

        fields.push(FieldSnapshot {
            automation_id: el.automation_id.clone(),
            name: el.name.clone(),
            label,
            control_type,
            value,
            rect: el.rect,
            enabled: el.enabled,
            focusable: el.focusable,
        });
    }

    fields
}

/// How many of `total` walked elements to keep, and whether that is fewer
/// than `total` (the caller must then report `truncated: true`). Pure so
/// the boundary is unit-tested without a COM call: `total == max_elements`
/// keeps everything and is NOT truncated (the cap is inclusive), one more
/// than that truncates.
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
fn bounded_count(total: usize, max_elements: usize) -> (usize, bool) {
    if total > max_elements {
        (max_elements, true)
    } else {
        (total, false)
    }
}

/// One line per field, token-efficient for a model prompt: empty label and
/// empty value are both omitted rather than printed as `""`. A redacted
/// field never prints the real text, only the `[redacted]` marker.
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
pub fn format_compact(fields: &[FieldSnapshot]) -> String {
    fields
        .iter()
        .map(format_line)
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
fn format_line(field: &FieldSnapshot) -> String {
    let mut line = field.control_type.as_str().to_string();

    if !field.label.is_empty() {
        line.push_str(&format!(" \"{}\"", field.label));
    }

    match &field.value {
        FieldValue::Text(v) if !v.is_empty() => {
            line.push_str(": ");
            line.push_str(v);
        }
        FieldValue::Redacted => line.push_str(": [redacted]"),
        FieldValue::Text(_) | FieldValue::Empty => {}
    }

    if !field.enabled {
        line.push_str(" (disabled)");
    }

    line
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Snapshots one window's editable controls by handle. This is the entry
/// point the integration test calls directly (see the task brief: "or
/// snapshot by HWND rather than foreground") so the assertions do not
/// depend on the test window actually holding desktop focus.
///
/// See the module doc comment for the threading requirement (worker thread
/// only).
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
pub fn snapshot_hwnd(
    hwnd: HWND,
    max_elements: usize,
    budget: Duration,
) -> anyhow::Result<Snapshot> {
    let (raw, truncated) = com::walk(hwnd, max_elements, budget)?;
    Ok(Snapshot {
        fields: build_snapshot(&raw),
        truncated,
    })
}

/// Snapshots the current foreground window. A thin wrapper: every
/// constraint on [`snapshot_hwnd`] (threading, budget, truncation) applies
/// unchanged.
#[allow(dead_code)] // see the module doc comment's "nothing calls this yet"
pub fn snapshot_foreground(max_elements: usize, budget: Duration) -> anyhow::Result<Snapshot> {
    let hwnd = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    anyhow::ensure!(!hwnd.0.is_null(), "no foreground window");
    snapshot_hwnd(hwnd, max_elements, budget)
}

// ---------------------------------------------------------------------------
// Win32 / UI Automation
// ---------------------------------------------------------------------------

mod com {
    // See the module doc comment's "nothing calls this yet" -- covers every
    // item in this submodule at once rather than one attribute apiece,
    // since all of it (the COM apartment guard, the walk, the extraction)
    // is reached only through `super::snapshot_hwnd`, itself unwired.
    #![allow(dead_code)]

    use super::{RawControlType, RawElement, Rect};
    use std::time::{Duration, Instant};
    use windows::core::Interface;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation8, IUIAutomation, IUIAutomationElement, IUIAutomationValuePattern,
        TreeScope_Descendants, UIA_AutomationIdPropertyId, UIA_BoundingRectanglePropertyId,
        UIA_ControlTypePropertyId, UIA_HelpTextPropertyId, UIA_IsEnabledPropertyId,
        UIA_IsKeyboardFocusablePropertyId, UIA_IsPasswordPropertyId, UIA_LabeledByPropertyId,
        UIA_NamePropertyId, UIA_ValuePatternId, UIA_ValueValuePropertyId,
    };

    /// RAII pairing of `CoInitializeEx(APARTMENTTHREADED)` with
    /// `CoUninitialize`, so every exit path (including an early `?`)
    /// uninitializes exactly once. Per the module doc comment, the CALLER
    /// is responsible for running on a dedicated worker thread; this guard
    /// only owns the init/uninit pairing itself.
    struct ComApartment;

    impl ComApartment {
        fn enter() -> anyhow::Result<Self> {
            // UI Automation returns S_OK for a fresh apartment and S_FALSE
            // when this thread is already STA-initialized -- both are
            // usable, and `HRESULT::is_ok()` (which `.ok()` uses) already
            // treats any non-negative code, S_FALSE included, as success.
            // Only a genuine failure (e.g. `RPC_E_CHANGED_MODE`: this
            // thread already runs MTA) becomes an `Err` here.
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

    /// Whether `i`'s underlying COM pointer is null. UI Automation returns
    /// a *successful* HRESULT with a null interface pointer for several
    /// "not present" cases (`LabeledBy` unset, a pattern the control does
    /// not support) rather than an error -- calling any method through a
    /// null vtable pointer is undefined behaviour, so every such result
    /// must be checked with this before use.
    fn is_null<I: Interface>(i: &I) -> bool {
        i.as_raw().is_null()
    }

    /// The single bulk COM call, plus turning its result into
    /// `RawElement`s. Returns the elements walked (up to `max_elements`,
    /// within `budget`) and whether the walk was cut short.
    pub(super) fn walk(
        hwnd: HWND,
        max_elements: usize,
        budget: Duration,
    ) -> anyhow::Result<(Vec<RawElement>, bool)> {
        let deadline = Instant::now() + budget;
        let _apartment = ComApartment::enter()?;

        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }?;

        let cache_request = unsafe { automation.CreateCacheRequest() }?;
        unsafe {
            cache_request.AddProperty(UIA_NamePropertyId)?;
            cache_request.AddProperty(UIA_AutomationIdPropertyId)?;
            cache_request.AddProperty(UIA_ControlTypePropertyId)?;
            cache_request.AddProperty(UIA_IsEnabledPropertyId)?;
            cache_request.AddProperty(UIA_IsKeyboardFocusablePropertyId)?;
            cache_request.AddProperty(UIA_BoundingRectanglePropertyId)?;
            cache_request.AddProperty(UIA_HelpTextPropertyId)?;
            cache_request.AddProperty(UIA_IsPasswordPropertyId)?;
            cache_request.AddProperty(UIA_LabeledByPropertyId)?;
            cache_request.AddPattern(UIA_ValuePatternId)?;
            // AddPattern alone caches the pattern *object*; reading a
            // cached property of that pattern (ValuePattern.CachedValue)
            // additionally needs its own property cached, or the call
            // fails with E_INVALIDARG ("Requested property was not in the
            // CacheRequest") -- MEASURED 2026-09-17 against this file's own
            // integration test below, which failed with exactly that HRESULT
            // until this line was added.
            cache_request.AddProperty(UIA_ValueValuePropertyId)?;
        }

        let root = unsafe { automation.ElementFromHandle(hwnd) }?;
        // A True condition over Descendants, not an OR of the six editable
        // control types: label resolution's "nearest preceding Text
        // element" fallback needs the Text (label) elements in the walk
        // too, and a control-type-filtered condition would exclude every
        // one of them along with everything else. This is still "a single
        // FindAllBuildCache over a condition, not a per-element property
        // round trip" (the task brief's performance requirement): the
        // filtering that the task's control-type list drives happens in
        // pure Rust afterwards (`ControlKind::from_raw`, called from
        // `build_snapshot`), over data this one call already brought back
        // cached, not via a second interop round trip per element.
        let condition = unsafe { automation.CreateTrueCondition() }?;
        let found =
            unsafe { root.FindAllBuildCache(TreeScope_Descendants, &condition, &cache_request) }?;

        let total = unsafe { found.Length() }?.max(0) as usize;
        let (keep, mut truncated) = super::bounded_count(total, max_elements);

        let mut raw = Vec::with_capacity(keep);
        for i in 0..keep {
            if Instant::now() >= deadline {
                truncated = true;
                break;
            }
            let element = unsafe { found.GetElement(i as i32) }?;
            raw.push(extract(&element)?);
        }

        Ok((raw, truncated))
    }

    fn extract(el: &IUIAutomationElement) -> anyhow::Result<RawElement> {
        let control_type = map_control_type(unsafe { el.CachedControlType() }?);
        let name = unsafe { el.CachedName() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let automation_id = unsafe { el.CachedAutomationId() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let help_text = unsafe { el.CachedHelpText() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let is_password = unsafe { el.CachedIsPassword() }
            .map(|b| b.as_bool())
            .unwrap_or(false);
        let enabled = unsafe { el.CachedIsEnabled() }
            .map(|b| b.as_bool())
            .unwrap_or(true);
        let focusable = unsafe { el.CachedIsKeyboardFocusable() }
            .map(|b| b.as_bool())
            .unwrap_or(false);
        let rect = unsafe { el.CachedBoundingRectangle() }
            .map(Rect::from)
            .unwrap_or_default();

        let labeled_by_name = unsafe { el.CachedLabeledBy() }.ok().and_then(|labeled_by| {
            if is_null(&labeled_by) {
                None
            } else {
                unsafe { labeled_by.CachedName() }
                    .ok()
                    .map(|b| b.to_string())
            }
        });

        // NEVER read ValuePattern.Value for a password field (task brief:
        // "NEVER the value of a password field"). `is_password` is checked
        // first, before any attempt to fetch or cast the pattern, so the
        // real text is never even transiently held as a Rust value for
        // this element -- redaction is not a display-time filter, the read
        // itself is skipped.
        let value = if is_password {
            None
        } else {
            unsafe { el.GetCachedPattern(UIA_ValuePatternId) }
                .ok()
                .and_then(|unknown| {
                    if is_null(&unknown) {
                        return None;
                    }
                    let pattern: IUIAutomationValuePattern = unknown.cast().ok()?;
                    unsafe { pattern.CachedValue() }.ok().map(|b| b.to_string())
                })
        };

        Ok(RawElement {
            automation_id,
            name,
            help_text,
            control_type,
            labeled_by_name,
            is_password,
            value,
            rect,
            enabled,
            focusable,
        })
    }

    fn map_control_type(
        id: windows::Win32::UI::Accessibility::UIA_CONTROLTYPE_ID,
    ) -> RawControlType {
        use windows::Win32::UI::Accessibility::{
            UIA_CheckBoxControlTypeId as CHECKBOX, UIA_ComboBoxControlTypeId as COMBOBOX,
            UIA_DocumentControlTypeId as DOCUMENT, UIA_EditControlTypeId as EDIT,
            UIA_ListControlTypeId as LIST, UIA_RadioButtonControlTypeId as RADIOBUTTON,
            UIA_TextControlTypeId as TEXT,
        };
        if id == EDIT {
            RawControlType::Edit
        } else if id == COMBOBOX {
            RawControlType::ComboBox
        } else if id == DOCUMENT {
            RawControlType::Document
        } else if id == CHECKBOX {
            RawControlType::CheckBox
        } else if id == RADIOBUTTON {
            RawControlType::RadioButton
        } else if id == LIST {
            RawControlType::List
        } else if id == TEXT {
            RawControlType::Text
        } else {
            RawControlType::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- synthetic-data helpers ---------------------------------------------

    fn text(name: &str) -> RawElement {
        RawElement {
            control_type: RawControlType::Text,
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn edit(name: &str) -> RawElement {
        RawElement {
            control_type: RawControlType::Edit,
            name: name.to_string(),
            enabled: true,
            ..Default::default()
        }
    }

    // -- label resolution: LabeledBy > nearest preceding Text > HelpText ---

    #[test]
    fn label_prefers_labeled_by_over_preceding_text_and_help_text() {
        let elements = vec![
            text("Nearby text"),
            RawElement {
                labeled_by_name: Some("Explicit label".to_string()),
                help_text: "Help text".to_string(),
                ..edit("firstName")
            },
        ];
        let fields = build_snapshot(&elements);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].label, "Explicit label");
    }

    #[test]
    fn label_falls_back_to_nearest_preceding_text_element() {
        let elements = vec![text("First name"), edit("firstName")];
        let fields = build_snapshot(&elements);
        assert_eq!(fields[0].label, "First name");
    }

    #[test]
    fn label_uses_nearest_not_first_preceding_text() {
        let elements = vec![text("Stale label"), text("Fresh label"), edit("field")];
        let fields = build_snapshot(&elements);
        assert_eq!(
            fields[0].label, "Fresh label",
            "the closer Text element must win, not the first one walked"
        );
    }

    #[test]
    fn label_falls_back_to_help_text_when_no_labeled_by_or_text() {
        let elements = vec![RawElement {
            help_text: "Enter your first name".to_string(),
            ..edit("firstName")
        }];
        let fields = build_snapshot(&elements);
        assert_eq!(fields[0].label, "Enter your first name");
    }

    #[test]
    fn label_is_empty_when_nothing_is_available() {
        let elements = vec![edit("firstName")];
        let fields = build_snapshot(&elements);
        assert_eq!(fields[0].label, "");
    }

    #[test]
    fn text_elements_with_empty_name_do_not_clear_a_prior_label() {
        let elements = vec![
            text("Real label"),
            text(""), // e.g. a decorative separator with no name
            edit("field"),
        ];
        let fields = build_snapshot(&elements);
        assert_eq!(fields[0].label, "Real label");
    }

    #[test]
    fn non_editable_elements_are_excluded_from_the_output_but_still_used_as_label_sources() {
        let elements = vec![
            text("Name"),
            RawElement {
                control_type: RawControlType::Other, // e.g. a button
                name: "Submit".to_string(),
                ..Default::default()
            },
            edit("field"),
        ];
        let fields = build_snapshot(&elements);
        // Only the Edit is emitted; the button is walked but never emitted,
        // and does not overwrite the preceding Text's label (only a Text
        // element updates `last_text`).
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].control_type, ControlKind::Edit);
        assert_eq!(fields[0].label, "Name");
    }

    #[test]
    fn all_six_editable_control_types_are_emitted() {
        let elements = vec![
            RawElement {
                control_type: RawControlType::Edit,
                ..Default::default()
            },
            RawElement {
                control_type: RawControlType::ComboBox,
                ..Default::default()
            },
            RawElement {
                control_type: RawControlType::Document,
                ..Default::default()
            },
            RawElement {
                control_type: RawControlType::CheckBox,
                ..Default::default()
            },
            RawElement {
                control_type: RawControlType::RadioButton,
                ..Default::default()
            },
            RawElement {
                control_type: RawControlType::List,
                ..Default::default()
            },
        ];
        let fields = build_snapshot(&elements);
        assert_eq!(fields.len(), 6);
        assert_eq!(
            fields.iter().map(|f| f.control_type).collect::<Vec<_>>(),
            vec![
                ControlKind::Edit,
                ControlKind::ComboBox,
                ControlKind::Document,
                ControlKind::CheckBox,
                ControlKind::RadioButton,
                ControlKind::List,
            ]
        );
    }

    // -- password redaction ---------------------------------------------------

    #[test]
    fn password_field_is_redacted_even_if_a_raw_value_was_supplied() {
        // Defense in depth (see build_snapshot's doc comment): even a
        // RawElement that -- incorrectly -- carries both `is_password:
        // true` and a real `value` must still come out redacted.
        let elements = vec![RawElement {
            is_password: true,
            value: Some("hunter2".to_string()),
            ..edit("password")
        }];
        let fields = build_snapshot(&elements);
        assert_eq!(fields[0].value, FieldValue::Redacted);
    }

    #[test]
    fn non_password_field_reports_its_real_value() {
        let elements = vec![RawElement {
            value: Some("Jane".to_string()),
            ..edit("firstName")
        }];
        let fields = build_snapshot(&elements);
        assert_eq!(fields[0].value, FieldValue::Text("Jane".to_string()));
    }

    #[test]
    fn missing_or_empty_value_is_recorded_as_empty_not_redacted() {
        let elements = vec![
            edit("a"),
            RawElement {
                value: Some(String::new()),
                ..edit("b")
            },
        ];
        let fields = build_snapshot(&elements);
        assert_eq!(fields[0].value, FieldValue::Empty);
        assert_eq!(fields[1].value, FieldValue::Empty);
    }

    // -- bounded_count: the truncation boundary --------------------------------

    #[test]
    fn keeps_everything_strictly_under_the_max() {
        assert_eq!(bounded_count(3, 500), (3, false));
    }

    #[test]
    fn keeps_exactly_the_max_at_the_boundary_without_truncating() {
        assert_eq!(bounded_count(500, 500), (500, false));
    }

    #[test]
    fn truncates_one_element_over_the_max() {
        assert_eq!(bounded_count(501, 500), (500, true));
    }

    #[test]
    fn zero_total_is_never_truncated() {
        assert_eq!(bounded_count(0, 500), (0, false));
    }

    #[test]
    fn max_zero_keeps_nothing_and_truncates_whenever_anything_exists() {
        assert_eq!(bounded_count(1, 0), (0, true));
        assert_eq!(bounded_count(0, 0), (0, false));
    }

    // -- format_compact: token-efficient serialization -------------------------

    fn field(label: &str, value: FieldValue) -> FieldSnapshot {
        FieldSnapshot {
            automation_id: String::new(),
            name: String::new(),
            label: label.to_string(),
            control_type: ControlKind::Edit,
            value,
            rect: Rect::default(),
            enabled: true,
            focusable: true,
        }
    }

    #[test]
    fn formats_label_and_value() {
        let f = field("First name", FieldValue::Text("Jane".to_string()));
        assert_eq!(format_compact(&[f]), "Edit \"First name\": Jane");
    }

    #[test]
    fn omits_the_value_when_empty() {
        let f = field("Country", FieldValue::Empty);
        assert_eq!(format_compact(&[f]), "Edit \"Country\"");
    }

    #[test]
    fn omits_the_label_when_empty() {
        let f = field("", FieldValue::Text("Jane".to_string()));
        assert_eq!(format_compact(&[f]), "Edit: Jane");
    }

    #[test]
    fn bare_kind_when_neither_label_nor_value_is_present() {
        let f = field("", FieldValue::Empty);
        assert_eq!(format_compact(&[f]), "Edit");
    }

    #[test]
    fn redacted_value_shows_the_marker_never_the_real_text() {
        let f = field("Password", FieldValue::Redacted);
        let line = format_compact(&[f]);
        assert_eq!(line, "Edit \"Password\": [redacted]");
        assert!(!line.contains("hunter2"));
    }

    #[test]
    fn disabled_field_gets_a_suffix() {
        let mut f = field("Promo code", FieldValue::Empty);
        f.enabled = false;
        assert_eq!(format_compact(&[f]), "Edit \"Promo code\" (disabled)");
    }

    #[test]
    fn multiple_fields_join_with_one_line_each() {
        let a = field("First name", FieldValue::Text("Jane".to_string()));
        let b = field("Last name", FieldValue::Text("Doe".to_string()));
        assert_eq!(
            format_compact(&[a, b]),
            "Edit \"First name\": Jane\nEdit \"Last name\": Doe"
        );
    }

    #[test]
    fn empty_field_list_formats_as_the_empty_string() {
        assert_eq!(format_compact(&[]), "");
    }

    // -- real Win32 window: label resolution and password redaction, for
    // real, via snapshot_hwnd -----------------------------------------------
    //
    // A test-only window class (CLAUDE.md rule 9): two labelled EDIT
    // controls (one plain, one ES_PASSWORD), each preceded by its own
    // STATIC label, built and torn down by this test alone. Z-order is
    // pinned explicitly (see `stack_in_creation_order`) rather than relied
    // on implicitly: CreateWindowExW places each new sibling at the TOP of
    // the Z-order by default, so without this the UIA walk would observe
    // the four controls in the *reverse* of creation order and the
    // "nearest preceding Text" pairing below would silently pair each edit
    // with the wrong label.
    mod win32 {
        use super::*;
        use std::sync::{Once, OnceLock};
        use windows::core::w;
        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, LoadCursorW,
            PeekMessageW, RegisterClassExW, SetForegroundWindow, SetWindowPos, ShowWindow,
            TranslateMessage, CS_HREDRAW, CS_VREDRAW, ES_AUTOHSCROLL, ES_PASSWORD, HWND_BOTTOM,
            IDC_ARROW, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE,
            WNDCLASSEXW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
        };

        /// Drains the calling thread's message queue without blocking.
        /// Some UIA proxies build (or refresh) a native window's element
        /// tree lazily around paint/creation messages, so the test pumps a
        /// few before snapshotting rather than assuming the tree is
        /// already populated the instant `CreateWindowExW` returns.
        fn pump_pending_messages() {
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }

        const CLASS_NAME: windows::core::PCWSTR = w!("Wingman.Inputs.UiaTestWindow.test.4f2a91");

        static CLASS_INIT: Once = Once::new();
        static CLASS_OK: OnceLock<bool> = OnceLock::new();

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

        /// Sends each handle to the bottom of the Z-order, in the order
        /// given -- the standard trick to make child enumeration order
        /// deterministic instead of "whatever order Win32 happened to
        /// stack them in". See the module doc comment above for why this
        /// matters for this specific test.
        fn stack_in_creation_order(children: &[HWND]) {
            for &child in children {
                unsafe {
                    let _ = SetWindowPos(
                        child,
                        Some(HWND_BOTTOM),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
        }

        #[test]
        fn snapshot_hwnd_resolves_labels_and_redacts_the_password_field() {
            let hinstance = instance();
            assert!(
                ensure_class_registered(hinstance),
                "RegisterClassExW for the test window class"
            );

            let frame = unsafe {
                CreateWindowExW(
                    Default::default(),
                    CLASS_NAME,
                    w!("Wingman UIA test window"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    0,
                    0,
                    320,
                    240,
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

            let child = |class: windows::core::PCWSTR,
                         text: windows::core::PCWSTR,
                         style: windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE,
                         y: i32|
             -> HWND {
                unsafe {
                    CreateWindowExW(
                        Default::default(),
                        class,
                        text,
                        WS_CHILD | WS_VISIBLE | style,
                        10,
                        y,
                        200,
                        20,
                        Some(frame),
                        None,
                        Some(hinstance),
                        None,
                    )
                }
                .expect("CreateWindowExW (child)")
            };

            let name_label = child(w!("STATIC"), w!("First name"), Default::default(), 10);
            let name_edit = child(
                w!("EDIT"),
                w!("Jane"),
                WS_TABSTOP
                    | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                30,
            );
            let password_label = child(w!("STATIC"), w!("Password"), Default::default(), 60);
            let password_edit = child(
                w!("EDIT"),
                w!("hunter2"),
                WS_TABSTOP
                    | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                        (ES_AUTOHSCROLL | ES_PASSWORD) as u32,
                    ),
                80,
            );

            // Creation order IS the intended label/field pairing order; pin
            // the Z-order to match it (see `stack_in_creation_order`'s doc
            // comment) rather than trusting the default.
            stack_in_creation_order(&[name_label, name_edit, password_label, password_edit]);
            pump_pending_messages();

            // Best-effort; the test asserts via `snapshot_hwnd` (by handle),
            // not `snapshot_foreground`, precisely so the result does not
            // depend on this succeeding under an unattended agent with no
            // interactive desktop session.
            unsafe {
                let _ = SetForegroundWindow(frame);
            }

            let start = std::time::Instant::now();
            let snapshot = crate::inputs::uia::snapshot_hwnd(
                frame,
                crate::inputs::uia::DEFAULT_MAX_ELEMENTS,
                crate::inputs::uia::DEFAULT_BUDGET,
            )
            .expect("snapshot_hwnd");
            let elapsed = start.elapsed();
            // MEASURED (recorded verbatim in the commit message): printed
            // here so a `-- --nocapture` run shows the real number this
            // test observed on this machine, not a guess.
            eprintln!("uia::snapshot_hwnd took {elapsed:?} for a 2-field test window");

            assert!(
                !snapshot.truncated,
                "a 4-element test window must never hit the 500-element / 750ms budget"
            );

            let first_name = snapshot
                .fields
                .iter()
                .find(|f| f.value == FieldValue::Text("Jane".to_string()))
                .expect("the first-name field is present with its real value");
            assert_eq!(first_name.label, "First name");
            assert_eq!(first_name.control_type, ControlKind::Edit);

            let password = snapshot
                .fields
                .iter()
                .find(|f| f.value == FieldValue::Redacted)
                .expect("the password field is present and redacted");
            assert_eq!(password.label, "Password");
            assert_ne!(
                password.value,
                FieldValue::Text("hunter2".to_string()),
                "the password field's real text must never appear in the snapshot"
            );

            unsafe {
                let _ = DestroyWindow(frame);
            }
        }
    }
}
