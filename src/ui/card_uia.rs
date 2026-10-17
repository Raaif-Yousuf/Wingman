//! UI Automation for the expanded card's footer links (#654, #606).
//!
//! The card is one custom-drawn window: its footer links ("Copy details",
//! "Report a problem", "Read aloud", "Copy") are painted and hit-tested by
//! hand, so a screen reader sees one window and no links. This module gives
//! the window a small UIA tree: a fragment root (answering `WM_GETOBJECT`)
//! with one child per link, each with a name, a role (Hyperlink or Button),
//! a bounding rectangle, keyboard focus state and the Invoke pattern.
//!
//! Why this route: the card has no child windows to lean on, and the other
//! ways to describe a custom-drawn window to a screen reader are worse.
//! MSAA (`IAccessible` via `WM_GETOBJECT`) is the legacy path with the same
//! amount of code and a weaker role and focus story. Real child controls
//! would change the card's paint, focus and Tab model (#606) for no gain.
//! A hand-written provider with the `windows` crate's `#[implement]` needs
//! only `windows-core` (named directly in `Cargo.toml`, for the macro's
//! generated code) and a few KB of code.
//!
//! Threading: UIA calls a server-side provider from its own threads. So the
//! providers never touch `CardInner`. They read a [`Snapshot`] behind a
//! mutex, which the card's UI thread refreshes after it paints, and
//! Invoke or SetFocus only post `WM_CARD_UIA_LINK` to the card window,
//! where the UI thread runs the real action. Nothing here blocks the UI
//! thread, and nothing runs until a client first sends `WM_GETOBJECT`.

use std::ffi::c_void;

use crate::ui::card::WM_CARD_UIA_LINK;
use std::sync::{Arc, Mutex};

use windows::core::{implement, ComObject, Error, IUnknown, Interface, Result, BSTR};
use windows::Win32::Foundation::{E_FAIL, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayPutElement};
use windows::Win32::System::Variant::{VARIANT, VT_I4};
use windows::Win32::UI::Accessibility::{
    IInvokeProvider, IInvokeProvider_Impl, IRawElementProviderFragment,
    IRawElementProviderFragmentRoot, IRawElementProviderFragmentRoot_Impl,
    IRawElementProviderFragment_Impl, IRawElementProviderSimple, IRawElementProviderSimple_Impl,
    NavigateDirection, NavigateDirection_FirstChild, NavigateDirection_LastChild,
    NavigateDirection_NextSibling, NavigateDirection_Parent, NavigateDirection_PreviousSibling,
    ProviderOptions, ProviderOptions_ServerSideProvider, UIA_AutomationIdPropertyId,
    UIA_ButtonControlTypeId, UIA_ControlTypePropertyId, UIA_HasKeyboardFocusPropertyId,
    UIA_HyperlinkControlTypeId, UIA_InvokePatternId, UIA_IsContentElementPropertyId,
    UIA_IsControlElementPropertyId, UIA_IsEnabledPropertyId, UIA_IsKeyboardFocusablePropertyId,
    UIA_IsOffscreenPropertyId, UIA_NamePropertyId, UiaHostProviderFromHwnd, UiaRect,
    UIA_PATTERN_ID, UIA_PROPERTY_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindowVisible, PostMessageW};

// A link's Invoke or SetFocus posts `WM_CARD_UIA_LINK` (`wparam` the
// link's id, `lparam` one of these).
pub(crate) const LINK_INVOKE: isize = 0;
pub(crate) const LINK_FOCUS: isize = 1;

/// `UIA_E_ELEMENTNOTAVAILABLE`: the link was on the card a moment ago but is
/// not any more (the card moved on). The standard answer for a stale element.
const UIA_E_ELEMENTNOTAVAILABLE: windows::core::HRESULT =
    windows::core::HRESULT(0x8004_0201_u32 as i32);

/// What a link is to a screen reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkRole {
    /// Opens something outside the card ("Report a problem").
    Hyperlink,
    /// Does something in place ("Copy", "Read aloud", "Copy details").
    Button,
}

/// One footer link as the providers see it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LinkInfo {
    /// Stable per link kind, so a re-layout keeps the same runtime id.
    pub id: i32,
    /// The visible label, which is also the accessible name.
    pub name: String,
    pub role: LinkRole,
    /// Client coordinates, in physical pixels.
    pub rect: RECT,
}

/// The links on screen and which one has keyboard focus, in Tab order.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub links: Vec<LinkInfo>,
    pub focus: Option<i32>,
}

pub(crate) type Shared = Arc<Mutex<Snapshot>>;

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Snapshot> {
    // A poisoned snapshot is still a valid snapshot: it is only ever replaced
    // whole.
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// UIA control type for a role.
pub(crate) fn control_type(role: LinkRole) -> i32 {
    match role {
        LinkRole::Hyperlink => UIA_HyperlinkControlTypeId.0,
        LinkRole::Button => UIA_ButtonControlTypeId.0,
    }
}

/// The automation id for a link, stable across labels.
pub(crate) fn automation_id(id: i32) -> String {
    format!("WingmanCard.FooterLink.{id}")
}

/// The index of the neighbour of `index` in a list of `len`, or `None` at
/// either end (UIA siblings do not wrap).
pub(crate) fn sibling_index(index: usize, len: usize, next: bool) -> Option<usize> {
    if index >= len {
        return None;
    }
    if next {
        (index + 1 < len).then_some(index + 1)
    } else {
        index.checked_sub(1)
    }
}

/// The link whose rect contains the client point, if any.
pub(crate) fn link_at(links: &[LinkInfo], x: i32, y: i32) -> Option<&LinkInfo> {
    links.iter().find(|l| {
        (l.rect.left..l.rect.right).contains(&x) && (l.rect.top..l.rect.bottom).contains(&y)
    })
}

fn none<T>() -> Result<T> {
    // S_OK with a null out pointer: UIA's "there is nothing there".
    Err(Error::empty())
}

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// Creates the card's root provider, to hand to `UiaReturnRawElementProvider`.
pub(crate) fn new_root(hwnd: HWND, shared: &Shared) -> IRawElementProviderSimple {
    ComObject::new(CardRoot {
        hwnd: hwnd.0 as isize,
        shared: shared.clone(),
    })
    .to_interface()
}

/// The provider for one link, for raising a focus event.
pub(crate) fn link_provider(hwnd: HWND, shared: &Shared, id: i32) -> IRawElementProviderSimple {
    ComObject::new(LinkElement {
        hwnd: hwnd.0 as isize,
        shared: shared.clone(),
        id,
    })
    .to_interface()
}

fn screen_rect(hwnd: HWND, r: &RECT) -> UiaRect {
    let mut tl = POINT {
        x: r.left,
        y: r.top,
    };
    let mut br = POINT {
        x: r.right,
        y: r.bottom,
    };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut tl);
        let _ = ClientToScreen(hwnd, &mut br);
    }
    UiaRect {
        left: tl.x as f64,
        top: tl.y as f64,
        width: (br.x - tl.x) as f64,
        height: (br.y - tl.y) as f64,
    }
}

fn runtime_id(parts: &[i32]) -> Result<*mut SAFEARRAY> {
    unsafe {
        let sa = SafeArrayCreateVector(VT_I4, 0, parts.len() as u32);
        if sa.is_null() {
            return Err(Error::from(E_FAIL));
        }
        for (i, p) in parts.iter().enumerate() {
            let idx = i as i32;
            SafeArrayPutElement(sa, &idx, p as *const i32 as *const c_void)?;
        }
        Ok(sa)
    }
}

// ---------------------------------------------------------------------------
// The fragment root: the card window
// ---------------------------------------------------------------------------

#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    IRawElementProviderFragmentRoot
)]
struct CardRoot {
    hwnd: isize,
    shared: Shared,
}

impl IRawElementProviderSimple_Impl for CardRoot_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }
    fn GetPatternProvider(&self, _pattern: UIA_PATTERN_ID) -> Result<IUnknown> {
        none()
    }
    fn GetPropertyValue(&self, _property: UIA_PROPERTY_ID) -> Result<VARIANT> {
        // Name, role and the rest come from the window itself, through the
        // host provider below.
        Ok(VARIANT::default())
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        unsafe { UiaHostProviderFromHwnd(hwnd_of(self.hwnd)) }
    }
}

impl IRawElementProviderFragment_Impl for CardRoot_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        let snap = lock(&self.shared);
        let pick = if direction == NavigateDirection_FirstChild {
            snap.links.first()
        } else if direction == NavigateDirection_LastChild {
            snap.links.last()
        } else {
            None
        };
        match pick {
            Some(l) => Ok(ComObject::new(LinkElement {
                hwnd: self.hwnd,
                shared: self.shared.clone(),
                id: l.id,
            })
            .to_interface()),
            None => none(),
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        // The window's own provider supplies the root's id.
        Ok(std::ptr::null_mut())
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        let mut rc = RECT::default();
        unsafe { GetWindowRect(hwnd_of(self.hwnd), &mut rc)? };
        Ok(UiaRect {
            left: rc.left as f64,
            top: rc.top as f64,
            width: (rc.right - rc.left) as f64,
            height: (rc.bottom - rc.top) as f64,
        })
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        Ok(())
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(ComObject::new(CardRoot {
            hwnd: self.hwnd,
            shared: self.shared.clone(),
        })
        .to_interface())
    }
}

impl IRawElementProviderFragmentRoot_Impl for CardRoot_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        let mut pt = POINT {
            x: x as i32,
            y: y as i32,
        };
        unsafe {
            let _ = ScreenToClient(hwnd_of(self.hwnd), &mut pt);
        }
        let snap = lock(&self.shared);
        match link_at(&snap.links, pt.x, pt.y) {
            Some(l) => Ok(ComObject::new(LinkElement {
                hwnd: self.hwnd,
                shared: self.shared.clone(),
                id: l.id,
            })
            .to_interface()),
            None => none(),
        }
    }
    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        let snap = lock(&self.shared);
        match snap.focus {
            Some(id) => Ok(ComObject::new(LinkElement {
                hwnd: self.hwnd,
                shared: self.shared.clone(),
                id,
            })
            .to_interface()),
            None => none(),
        }
    }
}

// ---------------------------------------------------------------------------
// One footer link
// ---------------------------------------------------------------------------

#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    IInvokeProvider
)]
struct LinkElement {
    hwnd: isize,
    shared: Shared,
    id: i32,
}

impl LinkElement {
    /// This link as it is now, or the stale-element error.
    fn current(&self) -> Result<(LinkInfo, Option<i32>)> {
        let snap = lock(&self.shared);
        snap.links
            .iter()
            .find(|l| l.id == self.id)
            .map(|l| (l.clone(), snap.focus))
            .ok_or_else(|| Error::from(UIA_E_ELEMENTNOTAVAILABLE))
    }
    fn post(&self, how: isize) -> Result<()> {
        // Refuse a link that is no longer on the card.
        self.current()?;
        unsafe {
            PostMessageW(
                Some(hwnd_of(self.hwnd)),
                WM_CARD_UIA_LINK,
                WPARAM(self.id as usize),
                LPARAM(how),
            )
        }
    }
}

impl IRawElementProviderSimple_Impl for LinkElement_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }
    fn GetPatternProvider(&self, pattern: UIA_PATTERN_ID) -> Result<IUnknown> {
        if pattern == UIA_InvokePatternId {
            let invoke: IInvokeProvider = ComObject::new(LinkElement {
                hwnd: self.hwnd,
                shared: self.shared.clone(),
                id: self.id,
            })
            .to_interface();
            return invoke.cast();
        }
        none()
    }
    fn GetPropertyValue(&self, property: UIA_PROPERTY_ID) -> Result<VARIANT> {
        let Ok((link, focus)) = self.current() else {
            return Ok(VARIANT::default());
        };
        Ok(if property == UIA_NamePropertyId {
            VARIANT::from(BSTR::from(link.name.as_str()))
        } else if property == UIA_ControlTypePropertyId {
            VARIANT::from(control_type(link.role))
        } else if property == UIA_AutomationIdPropertyId {
            VARIANT::from(BSTR::from(automation_id(link.id).as_str()))
        } else if property == UIA_HasKeyboardFocusPropertyId {
            VARIANT::from(focus == Some(link.id))
        } else if property == UIA_IsKeyboardFocusablePropertyId
            || property == UIA_IsEnabledPropertyId
            || property == UIA_IsControlElementPropertyId
            || property == UIA_IsContentElementPropertyId
        {
            VARIANT::from(true)
        } else if property == UIA_IsOffscreenPropertyId {
            VARIANT::from(false)
        } else {
            VARIANT::default()
        })
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        none()
    }
}

impl IRawElementProviderFragment_Impl for LinkElement_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        if direction == NavigateDirection_Parent {
            return Ok(ComObject::new(CardRoot {
                hwnd: self.hwnd,
                shared: self.shared.clone(),
            })
            .to_interface());
        }
        let next = direction == NavigateDirection_NextSibling;
        if !next && direction != NavigateDirection_PreviousSibling {
            return none();
        }
        let snap = lock(&self.shared);
        let Some(at) = snap.links.iter().position(|l| l.id == self.id) else {
            return none();
        };
        match sibling_index(at, snap.links.len(), next) {
            Some(i) => Ok(ComObject::new(LinkElement {
                hwnd: self.hwnd,
                shared: self.shared.clone(),
                id: snap.links[i].id,
            })
            .to_interface()),
            None => none(),
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        // UiaAppendRuntimeId (3) tells UIA to prepend the window's own id.
        runtime_id(&[3, self.id])
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        let (link, _) = self.current()?;
        Ok(screen_rect(hwnd_of(self.hwnd), &link.rect))
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        self.post(LINK_FOCUS)
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(ComObject::new(CardRoot {
            hwnd: self.hwnd,
            shared: self.shared.clone(),
        })
        .to_interface())
    }
}

impl IInvokeProvider_Impl for LinkElement_Impl {
    fn Invoke(&self) -> Result<()> {
        self.post(LINK_INVOKE)
    }
}

/// `true` while the card window is shown; a hidden card exposes no links.
pub(crate) fn window_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(id: i32, left: i32, right: i32) -> LinkInfo {
        LinkInfo {
            id,
            name: format!("link {id}"),
            role: LinkRole::Button,
            rect: RECT {
                left,
                top: 100,
                right,
                bottom: 120,
            },
        }
    }

    #[test]
    fn siblings_do_not_wrap() {
        assert_eq!(sibling_index(0, 3, true), Some(1));
        assert_eq!(sibling_index(2, 3, true), None);
        assert_eq!(sibling_index(0, 3, false), None);
        assert_eq!(sibling_index(2, 3, false), Some(1));
        assert_eq!(sibling_index(5, 3, true), None);
        assert_eq!(sibling_index(0, 0, true), None);
    }

    #[test]
    fn a_point_finds_the_link_under_it() {
        let links = [link(1, 10, 50), link(2, 60, 100)];
        assert_eq!(link_at(&links, 20, 110).map(|l| l.id), Some(1));
        assert_eq!(link_at(&links, 60, 110).map(|l| l.id), Some(2));
        assert!(link_at(&links, 55, 110).is_none());
        assert!(link_at(&links, 20, 99).is_none());
        assert!(link_at(&links, 100, 110).is_none());
    }

    #[test]
    fn roles_map_to_the_two_control_types() {
        assert_eq!(control_type(LinkRole::Hyperlink), 50005);
        assert_eq!(control_type(LinkRole::Button), 50000);
    }

    #[test]
    fn automation_ids_are_distinct_per_link() {
        assert_ne!(automation_id(1), automation_id(2));
    }
}
