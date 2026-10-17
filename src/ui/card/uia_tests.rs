//! #654: the card's UI Automation tree. The first group checks what the card
//! offers to UIA; the last group is a live check, with a real `IUIAutomation`
//! client finding the links by name on a test card window and invoking one.

use super::*;
use crate::ui::card_uia::{LinkRole, Snapshot};
use windows::core::{Interface, BSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationInvokePattern, TreeScope_Descendants,
    UIA_ButtonControlTypeId, UIA_HyperlinkControlTypeId, UIA_InvokePatternId, UIA_NamePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};

fn instance() -> HINSTANCE {
    let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
    HINSTANCE(h.0)
}

fn error_card() -> Card {
    let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
    card.set_owner(card.hwnd());
    card.show_error_with_details("Couldn't add the event", "detail text");
    card.inner.try_expand();
    card
}

fn answer_card() -> Card {
    let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
    card.set_owner(card.hwnd());
    card.show_model_answer("2 + 2 = 4", "You carried correctly.", 0, None);
    card.inner.try_expand();
    card
}

fn names(snap: &Snapshot) -> Vec<&str> {
    snap.links.iter().map(|l| l.name.as_str()).collect()
}

#[test]
fn an_expanded_error_card_offers_two_links_in_tab_order() {
    let card = error_card();
    let snap = card.inner.uia_snapshot();
    assert_eq!(names(&snap), ["Copy details", "Report a problem"]);
    assert_eq!(snap.links[0].role, LinkRole::Button);
    assert_eq!(snap.links[1].role, LinkRole::Hyperlink);
    assert_eq!(snap.focus, None);
    assert!(snap
        .links
        .iter()
        .all(|l| l.rect.right > l.rect.left && l.rect.bottom > l.rect.top));
}

#[test]
fn an_expanded_answer_card_offers_read_aloud_and_copy() {
    let card = answer_card();
    let snap = card.inner.uia_snapshot();
    assert_eq!(names(&snap), ["Read aloud", "Copy"]);
    assert!(snap.links.iter().all(|l| l.role == LinkRole::Button));
}

#[test]
fn the_snapshot_follows_tab_focus() {
    let mut card = error_card();
    card.inner.move_footer_focus(false);
    assert_eq!(
        card.inner.uia_snapshot().focus,
        Some(FooterLink::CopyDetails.uia_id())
    );
    card.inner.move_footer_focus(false);
    assert_eq!(
        card.inner.uia_snapshot().focus,
        Some(FooterLink::Report.uia_id())
    );
}

#[test]
fn a_collapsed_card_offers_no_links() {
    let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
    card.show_model_answer("2 + 2 = 4", "You carried correctly.", 0, None);
    assert_eq!(card.inner.uia_snapshot(), Snapshot::default());
}

#[test]
fn link_ids_are_distinct_and_round_trip() {
    let all = [
        FooterLink::CopyDetails,
        FooterLink::Report,
        FooterLink::ReadAloud,
        FooterLink::Copy,
    ];
    for l in all {
        assert_eq!(FooterLink::from_uia_id(l.uia_id()), Some(l));
    }
    let mut ids: Vec<i32> = all.iter().map(|l| l.uia_id()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), all.len());
    assert_eq!(FooterLink::from_uia_id(0), None);
}

#[test]
fn a_link_request_for_a_link_that_is_not_shown_does_nothing() {
    let mut card = answer_card();
    // "Copy details" is an error card's link.
    card.inner
        .uia_link_requested(FooterLink::CopyDetails.uia_id(), card_uia::LINK_INVOKE);
    assert_eq!(card.inner.focus_link, None);
    card.inner.uia_link_requested(99, card_uia::LINK_INVOKE);
    assert_eq!(card.inner.focus_link, None);
}

#[test]
fn a_focus_request_moves_the_focus_without_running_the_link() {
    let mut card = error_card();
    card.inner
        .uia_link_requested(FooterLink::Report.uia_id(), card_uia::LINK_FOCUS);
    assert_eq!(card.inner.focus_link, Some(FooterLink::Report));
}

// -- live: a real UIA client --------------------------------------------

/// Runs `client` on its own MTA thread (UIA must not be called from the
/// thread that owns the window: the provider call needs this thread to answer
/// `WM_GETOBJECT`) while this thread pumps messages. Returns the client's
/// result and whether "Report a problem" was posted meanwhile.
fn with_live_client<R: Send + 'static>(
    card: &Card,
    client: impl FnOnce(IUIAutomation, isize) -> R + Send + 'static,
) -> (R, bool) {
    let hwnd = card.hwnd().0 as isize;
    let worker = std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let automation: IUIAutomation = unsafe {
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).expect("CUIAutomation")
        };
        let out = client(automation, hwnd);
        unsafe { CoUninitialize() };
        out
    });
    let mut reported = false;
    let started = std::time::Instant::now();
    while !worker.is_finished() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the UIA client did not finish"
        );
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_APP_CARD_REPORT_PROBLEM {
                reported = true;
                continue;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    (worker.join().expect("UIA client thread"), reported)
}

fn find_by_name(
    automation: &IUIAutomation,
    hwnd: isize,
    name: &str,
) -> Option<windows::Win32::UI::Accessibility::IUIAutomationElement> {
    unsafe {
        let root = automation
            .ElementFromHandle(HWND(hwnd as *mut std::ffi::c_void))
            .ok()?;
        let cond = automation
            .CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(BSTR::from(name)))
            .ok()?;
        root.FindFirst(TreeScope_Descendants, &cond).ok()
    }
}

/// Other card tests run on other threads of this process, and a card that
/// takes the foreground closes this one (its `WM_KILLFOCUS`). A live check
/// whose card was closed under it proves nothing, so it runs again on a fresh
/// card; `attempt` returns `None` for such a run.
fn retry<T>(mut attempt: impl FnMut() -> Option<T>) -> T {
    for _ in 0..10 {
        if let Some(done) = attempt() {
            return done;
        }
    }
    panic!("the card was closed by another test on every attempt");
}

#[test]
fn live_uia_finds_each_error_card_link_by_name_with_its_role() {
    let (details, report, read_aloud) = retry(|| {
        let card = error_card();
        let (found, _) = with_live_client(&card, |auto, hwnd| {
            let d = find_by_name(&auto, hwnd, "Copy details").map(|e| unsafe {
                (
                    e.CurrentControlType().unwrap().0,
                    e.CurrentBoundingRectangle().unwrap(),
                    e.CurrentIsKeyboardFocusable().unwrap().as_bool(),
                )
            });
            let r = find_by_name(&auto, hwnd, "Report a problem")
                .map(|e| unsafe { e.CurrentControlType().unwrap().0 });
            let m = find_by_name(&auto, hwnd, "Read aloud").is_some();
            (d, r, m)
        });
        (card.state() == CardState::Expanded).then_some(found)
    });
    let (control, rect, focusable) = details.expect("Copy details is exposed");
    assert_eq!(control, UIA_ButtonControlTypeId.0);
    assert!(focusable);
    assert!(rect.right > rect.left && rect.bottom > rect.top);
    assert_eq!(report, Some(UIA_HyperlinkControlTypeId.0));
    assert!(!read_aloud, "an error card has no Read aloud link");
}

#[test]
fn live_uia_reports_the_tab_focused_link_as_focused() {
    let (focused, other) = retry(|| {
        let mut card = error_card();
        card.inner.move_footer_focus(false);
        let (found, _) = with_live_client(&card, |auto, hwnd| {
            let f = find_by_name(&auto, hwnd, "Copy details")
                .map(|e| unsafe { e.CurrentHasKeyboardFocus().unwrap().as_bool() });
            let o = find_by_name(&auto, hwnd, "Report a problem")
                .map(|e| unsafe { e.CurrentHasKeyboardFocus().unwrap().as_bool() });
            (f, o)
        });
        (card.state() == CardState::Expanded).then_some(found)
    });
    assert_eq!(focused, Some(true));
    assert_eq!(other, Some(false));
}

#[test]
fn live_uia_invoke_runs_the_link() {
    // The observable: Invoke on "Report a problem" posts the same message a
    // click on it does. Wired to nothing, no message arrives.
    let (invoked, reported) = retry(|| {
        let card = error_card();
        let (invoked, mut reported) = with_live_client(&card, |auto, hwnd| {
            let Some(el) = find_by_name(&auto, hwnd, "Report a problem") else {
                return false;
            };
            let pattern: IUIAutomationInvokePattern =
                unsafe { el.GetCurrentPattern(UIA_InvokePatternId) }
                    .expect("Invoke pattern")
                    .cast()
                    .expect("IUIAutomationInvokePattern");
            unsafe { pattern.Invoke() }.is_ok()
        });
        // Invoke only posts to the card window; pump until that has run.
        let started = std::time::Instant::now();
        while invoked && !reported && started.elapsed() < std::time::Duration::from_secs(2) {
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                if msg.message == WM_APP_CARD_REPORT_PROBLEM {
                    reported = true;
                    continue;
                }
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (card.state() == CardState::Expanded).then_some((invoked, reported))
    });
    assert!(invoked, "the link was not found or Invoke failed");
    assert!(reported, "Invoke did not post WM_APP_CARD_REPORT_PROBLEM");
}
