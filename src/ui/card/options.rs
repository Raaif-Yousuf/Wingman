//! The options card (#307): `CardState::Options`, a short list of one-click
//! choices for what is on screen, drawn by the same `Card` window as every
//! other card so it shares placement, theme, DPI and click-away handling.
//!
//! Kept in its own file so the card's other states stay untouched. It is a
//! child module of `card`, which is what lets it reach the card's private
//! fields.
//!
//! What it shows: a summary line, then 2 to 4 rows (a keycap with the digit and
//! the label), then a hint line. No Cancel and no close button: Esc, a click
//! elsewhere, the auto-dismiss, or picking a row closes it, and nothing has
//! run until a row is picked.
//!
//! Rows are real child buttons (owner-drawn, subclassed), not one painted
//! surface, so each has a window name for assistive tech. The card never takes
//! focus (typing keeps going to the app underneath), so the subclass handles
//! the click itself and never lets the button call `SetFocus`. Keys 1 to 4 and
//! Esc reach the card through the keyboard hook's bounded claim
//! (`crate::options_keys`), armed here while the card shows and cleared on
//! every path out of the state.

use super::*;
use crate::options_keys::{self, Choice, Claim, WM_APP_OPTIONS_CHOICE};

/// One button: the action id it runs and the label shown on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRow {
    pub id: String,
    pub label: String,
}

/// How the card behaves, from `[options]` in the config and from whether the
/// bare key will run option 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionsBehavior {
    /// Seconds before the card closes itself (already clamped by the caller).
    pub dismiss_secs: u32,
    /// Whether the keyboard hook claims keys 1 to 4 and Esc while it shows.
    pub claim_keys: bool,
    /// Whether one press of the Wingman key runs option 1. Only then is row 1
    /// drawn as the default (accent outline): the outline promises the key.
    pub key_runs_first: bool,
}

/// Live state for `CardState::Options`. `Some` for exactly as long as the
/// row buttons exist (see `leave_options_if_active`).
pub(super) struct OptionsUi {
    summary: String,
    rows: Vec<OptionRow>,
    buttons: Vec<HWND>,
    generation: u32,
    behavior: OptionsBehavior,
}

/// First child-control id of the row buttons: row `n` (from 0) is
/// `ID_OPTION_BASE + n`. Clear of the preview's 3900..=3902.
const ID_OPTION_BASE: i32 = 3910;
const OPTION_SUBCLASS_ID: usize = 2;

/// The options card is a little wider than an answer card: a label of up to
/// six words plus the keycap needs the room.
const OPTIONS_WIDTH_DP: i32 = 320;
/// The least a row is tall: Windows 11's list items are 32 dp.
const OPTION_ROW_MIN_H_DP: i32 = 32;
/// Summary lines kept before the ellipsis.
const OPTIONS_SUMMARY_MAX_LINES: i32 = 2;

/// The reply that keeps a click from activating the card (`MA_NOACTIVATE`).
const MA_NOACTIVATE: isize = 3;

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The hint line under the rows. It names exactly what the hook claims, so a
/// digit that is not claimed never looks like it should have worked.
pub(super) fn hint_text(count: usize, claim_keys: bool) -> String {
    let count = count.min(options_keys::MAX_OPTIONS);
    match (claim_keys, count) {
        (true, 0) => "Esc to close".to_string(),
        (true, 1) => "Press 1, or Esc to close".to_string(),
        (true, n) => format!("Press 1 to {n}, or Esc to close"),
        (false, 0) => "Click elsewhere to close".to_string(),
        (false, _) => "Click an option, or click elsewhere to close".to_string(),
    }
}

/// The name a row button carries for assistive tech: the whole label, with
/// where it sits in the list ("Review this email, 1 of 3").
pub(super) fn accessible_name(label: &str, position: usize, count: usize) -> String {
    format!("{label}, {position} of {count}")
}

/// Which row is drawn as the default. `None` when the bare key will not run
/// option 1 (it is only promised once the key picks), or there are no rows.
pub(super) fn default_row(count: usize, key_runs_first: bool) -> Option<usize> {
    (key_runs_first && count > 0).then_some(0)
}

/// The card's measurements, in pixels.
#[derive(Debug, Clone, Copy)]
pub(super) struct LayoutInputs {
    pub width: i32,
    pub padding: i32,
    pub gap: i32,
    pub summary_h: i32,
    pub row_h: i32,
    pub hint_h: i32,
    pub rows: usize,
}

/// Where everything goes, in the card's client coordinates.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct OptionsMetrics {
    pub width: i32,
    pub height: i32,
    pub summary: RECT,
    pub rows: Vec<RECT>,
    pub hint: RECT,
}

/// Lays out the summary, the rows and the hint. The height is the sum of what
/// is there, so it grows with the row count and the text scale and is computed
/// once per change, never per row.
pub(super) fn options_layout(i: &LayoutInputs) -> OptionsMetrics {
    let left = i.padding;
    let right = (i.width - i.padding).max(left);
    let mut y = i.padding;
    let summary = RECT {
        left,
        top: y,
        right,
        bottom: y + i.summary_h,
    };
    y += i.summary_h;
    let mut rows = Vec::with_capacity(i.rows);
    if i.rows > 0 {
        y += i.gap * 2;
        for n in 0..i.rows {
            if n > 0 {
                y += i.gap;
            }
            rows.push(RECT {
                left,
                top: y,
                right,
                bottom: y + i.row_h,
            });
            y += i.row_h;
        }
        y += i.gap * 2;
    } else {
        y += i.gap;
    }
    let hint = RECT {
        left,
        top: y,
        right,
        bottom: y + i.hint_h,
    };
    y += i.hint_h;
    OptionsMetrics {
        width: i.width,
        height: y + i.padding,
        summary,
        rows,
        hint,
    }
}

/// The keycap's side: the row minus a gap above and below.
pub(super) fn keycap_size(row_h: i32, gap: i32) -> i32 {
    (row_h - gap * 2).max(1)
}

// ---------------------------------------------------------------------------
// Card API
// ---------------------------------------------------------------------------

impl Card {
    /// Shows the options card for the card showing `generation`. With no
    /// `rows` it is the shell: the summary placeholder and no buttons, which
    /// [`Card::set_options`] later fills. Anything showing is replaced. The
    /// card never takes focus.
    pub fn show_options(
        &mut self,
        generation: u32,
        summary: &str,
        rows: &[OptionRow],
        behavior: OptionsBehavior,
    ) {
        self.inner
            .show_options_state(generation, summary, rows, behavior);
    }

    /// The shell becomes ready: swaps in the summary and the rows, once.
    /// `false` (and nothing changes) when the card is no longer showing
    /// `generation`: it was hidden or replaced while the router worked.
    pub fn set_options(&mut self, generation: u32, summary: &str, rows: &[OptionRow]) -> bool {
        self.inner.set_options_state(generation, summary, rows)
    }

    /// The generation of the options card on screen, `None` in every other
    /// state.
    pub fn options_generation(&self) -> Option<u32> {
        self.inner.options.as_ref().map(|o| o.generation)
    }
}

impl CardInner {
    fn show_options_state(
        &mut self,
        generation: u32,
        summary: &str,
        rows: &[OptionRow],
        behavior: OptionsBehavior,
    ) {
        self.leave_preview_if_active();
        self.leave_options_if_active();
        self.reset_activation_and_timers();
        self.headline.clear();
        self.detail.clear();
        self.difficulty = None;
        self.scroll_offset = 0;
        self.scroll_max = 0;
        self.open_settings_on_click = false;
        self.options = Some(OptionsUi {
            summary: summary.to_string(),
            rows: rows
                .iter()
                .take(options_keys::MAX_OPTIONS)
                .cloned()
                .collect(),
            buttons: Vec::new(),
            generation,
            behavior,
        });
        self.state = CardState::Options;
        self.layout_options(true);
        self.create_option_buttons();
        self.arm_options_timer_and_claim();
        self.sync_options_window_text();
        self.reveal();
        self.announce_options();
    }

    fn set_options_state(&mut self, generation: u32, summary: &str, rows: &[OptionRow]) -> bool {
        let current = match (&self.state, &self.options) {
            (CardState::Options, Some(o)) => o.generation == generation,
            _ => false,
        };
        if !current {
            return false;
        }
        self.destroy_option_buttons();
        if let Some(ui) = self.options.as_mut() {
            ui.summary = summary.to_string();
            ui.rows = rows
                .iter()
                .take(options_keys::MAX_OPTIONS)
                .cloned()
                .collect();
        }
        // Once: the card re-measures for the final row count, not per row.
        self.layout_options(false);
        self.create_option_buttons();
        // The user only sees the buttons now, so the time to read them starts
        // now, and the claim names the buttons that exist.
        self.arm_options_timer_and_claim();
        self.sync_options_window_text();
        self.invalidate();
        self.announce_options();
        true
    }

    /// The one place every path out of `CardState::Options` goes through (the
    /// `leave_preview_if_active` twin): destroys the row buttons and stops the
    /// keyboard claim. Called at the top of every state-entering method and of
    /// `hide`.
    pub(super) fn leave_options_if_active(&mut self) {
        self.destroy_option_buttons();
        if let Some(ui) = self.options.take() {
            if ui.behavior.claim_keys {
                options_keys::clear();
            }
        }
    }

    fn destroy_option_buttons(&mut self) {
        if let Some(ui) = self.options.as_mut() {
            for hwnd in ui.buttons.drain(..) {
                if !hwnd.0.is_null() {
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                }
            }
        }
    }

    /// Arms the one-shot auto-dismiss and, when the setting allows, the key
    /// claim for the buttons that exist now. Nothing runs between presses
    /// (rule 5): the timer is killed on every hide.
    fn arm_options_timer_and_claim(&mut self) {
        let Some(ui) = self.options.as_ref() else {
            return;
        };
        let secs = ui.behavior.dismiss_secs;
        unsafe {
            let _ = SetTimer(
                Some(self.hwnd),
                TIMER_DISMISS,
                secs.saturating_mul(1000),
                None,
            );
        }
        if ui.behavior.claim_keys {
            let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
            options_keys::arm(Claim {
                generation: ui.generation,
                count: ui.buttons.len(),
                expiry_tick: options_keys::expiry_after(now, secs),
            });
        }
    }

    /// The window's own name follows what the card shows (#356): the summary
    /// and the hint, so a screen reader that reads the window gets both.
    fn sync_options_window_text(&self) {
        let Some(ui) = self.options.as_ref() else {
            return;
        };
        let hint = hint_text(ui.rows.len(), ui.behavior.claim_keys);
        self.set_window_text(&format!("{}. {}", ui.summary, hint));
    }

    /// Raises a system alert event so a screen reader announces the card when
    /// it appears. The card never takes focus, so nothing else would.
    fn announce_options(&self) {
        use windows::Win32::UI::Accessibility::NotifyWinEvent;
        use windows::Win32::UI::WindowsAndMessaging::{EVENT_SYSTEM_ALERT, OBJID_WINDOW};
        unsafe {
            NotifyWinEvent(EVENT_SYSTEM_ALERT, self.hwnd, OBJID_WINDOW.0, 0);
        }
    }

    fn options_layout_inputs(&self, rows: usize) -> LayoutInputs {
        let width = self.scale(OPTIONS_WIDTH_DP);
        let padding = self.scale(PADDING_DP);
        let gap = self.scale(GAP_DP);
        let content_w = (width - padding * 2).max(1);
        let summary_text = self.options.as_ref().map_or("", |o| o.summary.as_str());
        let summary_line = self.line_height(self.fonts.headline).max(1);
        let summary_h = self
            .measure_wrapped(self.fonts.headline, summary_text, content_w)
            .clamp(summary_line, summary_line * OPTIONS_SUMMARY_MAX_LINES);
        let body_line = self.line_height(self.fonts.body).max(1);
        LayoutInputs {
            width,
            padding,
            gap,
            summary_h,
            row_h: (body_line + gap * 2).max(self.scale(OPTION_ROW_MIN_H_DP)),
            hint_h: body_line,
            rows,
        }
    }

    fn options_metrics(&self) -> OptionsMetrics {
        let rows = self.options.as_ref().map_or(0, |o| o.rows.len());
        options_layout(&self.options_layout_inputs(rows))
    }

    /// Sizes and places the card. `from_cursor` picks the monitor under the
    /// pointer (a fresh card); the ready transition stays where the shell was.
    fn layout_options(&mut self, from_cursor: bool) {
        let work = self.target_work_area(from_cursor);
        let metrics = self.options_metrics();
        self.place_bottom_right(work, metrics.width, metrics.height);
    }

    fn create_option_buttons(&mut self) {
        let metrics = self.options_metrics();
        let (instance, parent, font) = (self.instance, self.hwnd, self.fonts.body);
        let Some(ui) = self.options.as_mut() else {
            return;
        };
        let count = ui.rows.len();
        for (n, (row, rect)) in ui.rows.iter().zip(metrics.rows.iter()).enumerate() {
            let name = accessible_name(&row.label, n + 1, count);
            let hwnd = create_preview_button(
                parent,
                instance,
                font,
                &name,
                rect,
                ID_OPTION_BASE + n as i32,
                true,
            );
            if hwnd.0.is_null() {
                continue;
            }
            unsafe {
                let _ = SetWindowSubclass(
                    hwnd,
                    Some(options_button_subclass),
                    OPTION_SUBCLASS_ID,
                    parent.0 as usize,
                );
            }
            ui.buttons.push(hwnd);
        }
    }

    /// The DPI-change counterpart of [`Self::create_option_buttons`]: re-lays
    /// out and moves the existing buttons, re-applying the rebuilt font.
    pub(super) fn relayout_options(&mut self) {
        self.layout_options(false);
        let metrics = self.options_metrics();
        let font = self.fonts.body;
        let Some(ui) = self.options.as_ref() else {
            return;
        };
        for (hwnd, rect) in ui.buttons.iter().zip(metrics.rows.iter()) {
            unsafe {
                let _ = SetWindowPos(
                    *hwnd,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER,
                );
                SendMessageW(
                    *hwnd,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
    }

    /// A row button was clicked (the subclass posts `WM_COMMAND`).
    pub(super) fn options_command(&mut self, id: i32) {
        let Some(ui) = self.options.as_ref() else {
            return;
        };
        let Some(n) = id
            .checked_sub(ID_OPTION_BASE)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n < ui.rows.len())
        else {
            return;
        };
        self.post_options_choice(Choice::Pick(n + 1));
    }

    /// Tells the owner window what was chosen, with the generation it was
    /// chosen on. The claim is spent first, so a digit typed before the owner
    /// has handled this click reaches the app instead of running a second row.
    fn post_options_choice(&self, choice: Choice) {
        let Some(ui) = self.options.as_ref() else {
            return;
        };
        if ui.behavior.claim_keys {
            options_keys::clear();
        }
        if let Some(owner) = self.owner {
            unsafe {
                let _ = PostMessageW(
                    Some(owner),
                    WM_APP_OPTIONS_CHOICE,
                    WPARAM(choice.to_wparam()),
                    LPARAM(ui.generation as isize),
                );
            }
        }
    }

    // -- painting -----------------------------------------------------------

    /// The summary and the hint. The rows are the child buttons' own paint.
    pub(super) unsafe fn paint_options(&self, hdc: HDC, palette: &Palette) {
        let Some(ui) = self.options.as_ref() else {
            return;
        };
        let metrics = self.options_metrics();
        let looking = ui.rows.is_empty();

        SelectObject(hdc, HGDIOBJ(self.fonts.headline.0));
        SetTextColor(
            hdc,
            windows::Win32::Foundation::COLORREF(if looking {
                palette.detail
            } else {
                palette.headline
            }),
        );
        draw_text_line(
            hdc,
            &ui.summary,
            metrics.summary,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.hint));
        draw_text_line(
            hdc,
            &hint_text(ui.rows.len(), ui.behavior.claim_keys),
            metrics.hint,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
    }

    /// Paints one row button for `WM_DRAWITEM`: the shared outline button,
    /// then the keycap, the label, and (when the key will run it) the accent
    /// outline of the default row.
    pub(super) fn draw_option_button(&self, lparam: LPARAM) {
        use crate::ui::card_buttons as btn;
        let dis = lparam.0 as *const windows::Win32::UI::Controls::DRAWITEMSTRUCT;
        if dis.is_null() {
            return;
        }
        let dis = unsafe { &*dis };
        let Some(ui) = self.options.as_ref() else {
            return;
        };
        let Some(index) = usize::try_from(dis.CtlID as i32 - ID_OPTION_BASE).ok() else {
            return;
        };
        let Some(row) = ui.rows.get(index) else {
            return;
        };
        let palette = self.palette();
        let high_contrast = self.button_high_contrast();
        let surface = btn::Surface {
            bg: palette.bg,
            border: palette.border,
            text: palette.headline,
            hint: palette.hint,
        };
        let accent = btn::system_accent(self.theme, palette.bg);
        let hovered = button_is_hovered(dis.hwndItem);
        let look = btn::look(
            btn::Role::Secondary,
            btn::state_of(dis, hovered),
            &surface,
            accent,
            high_contrast.as_ref(),
        );
        let radius = self.scale(CONTROL_RADIUS_DP);
        let ring = self.scale(1);
        unsafe {
            btn::draw(dis, "", self.fonts.body, &look, palette.bg, radius, ring);

            let hdc = dis.hDC;
            let rc = dis.rcItem;
            let gap = self.scale(GAP_DP);
            let row_h = rc.bottom - rc.top;
            let cap = keycap_size(row_h, gap);
            let cap_rect = RECT {
                left: rc.left + gap,
                top: rc.top + (row_h - cap) / 2,
                right: rc.left + gap + cap,
                bottom: rc.top + (row_h - cap) / 2 + cap,
            };

            // The keycap: a small outlined square with the digit, in the
            // quieter text colour so it never outshouts the label.
            let cap_ink = if high_contrast.is_some() {
                look.text
            } else {
                palette.detail
            };
            let fill = CreateSolidBrush(windows::Win32::Foundation::COLORREF(look.fill));
            let pen = CreatePen(
                PS_SOLID,
                ring.max(1),
                windows::Win32::Foundation::COLORREF(cap_ink),
            );
            let old_brush = SelectObject(hdc, HGDIOBJ(fill.0));
            let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
            let corner = (radius * 2 / 3).max(2) * 2;
            let _ = RoundRect(
                hdc,
                cap_rect.left,
                cap_rect.top,
                cap_rect.right,
                cap_rect.bottom,
                corner,
                corner,
            );
            SelectObject(hdc, old_pen);
            SelectObject(hdc, old_brush);
            let _ = DeleteObject(HGDIOBJ(pen.0));
            let _ = DeleteObject(HGDIOBJ(fill.0));

            let old_font = SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(cap_ink));
            draw_text_line(
                hdc,
                &(index + 1).to_string(),
                cap_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );

            // The label, one line, ending in an ellipsis. The button's window
            // name carries the whole label for assistive tech.
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(look.text));
            let label_rect = RECT {
                left: cap_rect.right + gap * 2,
                top: rc.top,
                right: (rc.right - gap * 2).max(cap_rect.right + gap * 2),
                bottom: rc.bottom,
            };
            draw_text_line(
                hdc,
                &row.label,
                label_rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            SelectObject(hdc, old_font);

            // The default row: an accent outline, because one press of the
            // key runs it. In high contrast the system highlight colour.
            if default_row(ui.rows.len(), ui.behavior.key_runs_first) == Some(index) {
                let color = match &high_contrast {
                    Some(hc) => hc.highlight,
                    None => accent,
                };
                let width = self.scale(2).max(2);
                let pen = CreatePen(PS_SOLID, width, windows::Win32::Foundation::COLORREF(color));
                let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
                let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
                let inset = width / 2;
                let _ = RoundRect(
                    hdc,
                    rc.left + inset,
                    rc.top + inset,
                    rc.right - inset,
                    rc.bottom - inset,
                    radius * 2,
                    radius * 2,
                );
                SelectObject(hdc, old_brush);
                SelectObject(hdc, old_pen);
                let _ = DeleteObject(HGDIOBJ(pen.0));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The row buttons' subclass
// ---------------------------------------------------------------------------

/// Handles a row button's mouse itself. A stock button calls `SetFocus` on a
/// click, which would activate the card and pull typing away from the app the
/// user is in; handling the press and release here, with no default
/// processing, keeps the focus where it was. A click is a press and a release
/// both inside the button, as for any push button, and it arrives at the card
/// as the usual `WM_COMMAND`.
unsafe extern "system" fn options_button_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    parent: usize,
) -> LRESULT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture};
    use windows::Win32::UI::WindowsAndMessaging::{
        BM_SETSTATE, WM_CAPTURECHANGED, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    };
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE),
        WM_LBUTTONDOWN => {
            let _ = SetCapture(hwnd);
            SendMessageW(hwnd, BM_SETSTATE, Some(WPARAM(1)), Some(LPARAM(0)));
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            if GetCapture() == hwnd {
                let _ = ReleaseCapture();
                SendMessageW(hwnd, BM_SETSTATE, Some(WPARAM(0)), Some(LPARAM(0)));
                let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let mut rc = RECT::default();
                let inside = GetClientRect(hwnd, &mut rc).is_ok()
                    && x >= rc.left
                    && x < rc.right
                    && y >= rc.top
                    && y < rc.bottom;
                if inside {
                    let id = windows::Win32::UI::WindowsAndMessaging::GetDlgCtrlID(hwnd);
                    let _ = PostMessageW(
                        Some(HWND(parent as *mut c_void)),
                        WM_COMMAND,
                        WPARAM((id as usize) & 0xFFFF),
                        LPARAM(hwnd.0 as isize),
                    );
                }
            }
            return LRESULT(0);
        }
        // Losing the capture (another window took it) ends a press that never
        // got its release.
        WM_CAPTURECHANGED => {
            SendMessageW(hwnd, BM_SETSTATE, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        WM_MOUSEMOVE => set_button_hover(hwnd, true),
        WM_MOUSELEAVE => set_button_hover(hwnd, false),
        WM_NCDESTROY => {
            set_button_hover(hwnd, false);
            let _ = RemoveWindowSubclass(hwnd, Some(options_button_subclass), OPTION_SUBCLASS_ID);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{contrast_ratio, relative_luminance};

    fn inputs(rows: usize) -> LayoutInputs {
        LayoutInputs {
            width: 320,
            padding: 14,
            gap: 6,
            summary_h: 20,
            row_h: 32,
            hint_h: 16,
            rows,
        }
    }

    // -- the hint line ------------------------------------------------------

    #[test]
    fn the_hint_names_exactly_what_the_hook_claims() {
        assert_eq!(hint_text(0, true), "Esc to close");
        assert_eq!(hint_text(1, true), "Press 1, or Esc to close");
        assert_eq!(hint_text(2, true), "Press 1 to 2, or Esc to close");
        assert_eq!(hint_text(3, true), "Press 1 to 3, or Esc to close");
        assert_eq!(hint_text(4, true), "Press 1 to 4, or Esc to close");
        // More rows than the cap never promise a fifth key.
        assert_eq!(hint_text(9, true), "Press 1 to 4, or Esc to close");
    }

    #[test]
    fn with_the_claim_off_the_hint_never_promises_a_key() {
        for count in 0..=4 {
            let hint = hint_text(count, false);
            assert!(!hint.contains("Esc") && !hint.contains("Press"), "{hint}");
        }
    }

    #[test]
    fn hints_have_no_em_dash() {
        for claim in [true, false] {
            for count in 0..=5 {
                assert!(!hint_text(count, claim).contains('\u{2014}'));
            }
        }
    }

    #[test]
    fn a_row_is_named_by_its_whole_label_and_position() {
        assert_eq!(
            accessible_name("Review this email to Dana", 1, 3),
            "Review this email to Dana, 1 of 3"
        );
    }

    #[test]
    fn only_a_key_that_runs_option_one_marks_it_as_the_default() {
        assert_eq!(default_row(3, true), Some(0));
        assert_eq!(default_row(3, false), None);
        assert_eq!(default_row(0, true), None);
    }

    // -- layout -------------------------------------------------------------

    #[test]
    fn the_card_grows_with_each_row_by_one_row_and_one_gap() {
        let one = options_layout(&inputs(1)).height;
        let two = options_layout(&inputs(2)).height;
        let four = options_layout(&inputs(4)).height;
        assert_eq!(two - one, 32 + 6);
        assert_eq!(four - one, 3 * (32 + 6));
    }

    #[test]
    fn the_shell_with_no_rows_is_shorter_than_a_card_with_rows() {
        let shell = options_layout(&inputs(0));
        assert!(shell.rows.is_empty());
        assert!(shell.height < options_layout(&inputs(2)).height);
        // Summary, a gap, the hint, and the padding around them.
        assert_eq!(shell.height, 14 + 20 + 6 + 16 + 14);
    }

    #[test]
    fn rows_sit_inside_the_padding_in_order_and_never_overlap() {
        let m = options_layout(&inputs(4));
        assert_eq!(m.rows.len(), 4);
        let mut previous_bottom = m.summary.bottom;
        for r in &m.rows {
            assert_eq!((r.left, r.right), (14, 320 - 14));
            assert!(r.top > previous_bottom, "rows must not touch the summary");
            assert_eq!(r.bottom - r.top, 32);
            previous_bottom = r.bottom;
        }
        assert!(m.hint.top > previous_bottom);
        assert_eq!(m.hint.bottom + 14, m.height);
    }

    #[test]
    fn the_card_height_scales_with_the_display_scale() {
        // The same card at 150%: every measure scaled, so the height is too.
        let at = |s: f32| {
            let px = |v: i32| (v as f32 * s).round() as i32;
            options_layout(&LayoutInputs {
                width: px(320),
                padding: px(14),
                gap: px(6),
                summary_h: px(20),
                row_h: px(32),
                hint_h: px(16),
                rows: 3,
            })
            .height
        };
        let (h100, h150) = (at(1.0), at(1.5));
        assert!(
            (h150 as f32 - h100 as f32 * 1.5).abs() <= 2.0,
            "{h100} {h150}"
        );
    }

    #[test]
    fn a_tall_summary_pushes_everything_down_and_nothing_is_clipped() {
        let mut tall = inputs(2);
        tall.summary_h = 40;
        let (a, b) = (options_layout(&inputs(2)), options_layout(&tall));
        assert_eq!(b.height - a.height, 20);
        assert_eq!(b.rows[0].top - a.rows[0].top, 20);
    }

    #[test]
    fn a_degenerate_width_never_panics_or_inverts_a_rect() {
        let m = options_layout(&LayoutInputs {
            width: 10,
            padding: 14,
            ..inputs(2)
        });
        for r in m.rows.iter().chain([&m.summary, &m.hint]) {
            assert!(r.right >= r.left && r.bottom >= r.top);
        }
    }

    #[test]
    fn the_keycap_fits_inside_its_row() {
        for (row_h, gap) in [(32, 6), (48, 9), (80, 15), (4, 6)] {
            let cap = keycap_size(row_h, gap);
            assert!(cap >= 1);
            if row_h > gap * 2 {
                assert!(cap <= row_h - gap * 2);
            }
        }
    }

    // -- contrast (spec: 4.5:1 for text, 3:1 for the outline and keycap) ----

    fn ratio(a: u32, b: u32) -> f64 {
        contrast_ratio(relative_luminance(a), relative_luminance(b))
    }

    #[test]
    fn the_keycap_and_label_meet_the_contrast_bar_in_every_theme() {
        use crate::ui::theme::{Theme, Tokens, HC_BLACK, HC_WHITE};
        let themes = [
            (Theme::Light, HC_BLACK),
            (Theme::Dark, HC_BLACK),
            (Theme::HighContrast, HC_BLACK),
            (Theme::HighContrast, HC_WHITE),
        ];
        for (theme, sys) in themes {
            let t = Tokens::for_theme(theme, &sys);
            // The label and keycap digit sit on the card, hover included
            // (the hover fill moves 8 percent towards the text colour).
            let hover = crate::ui::card_buttons::blend(t.surface, t.text, 8);
            for bg in [t.surface, hover] {
                assert!(ratio(t.text, bg) >= 4.5, "{theme:?} label on {bg:#08x}");
                assert!(ratio(t.text_dim, bg) >= 4.5, "{theme:?} digit on {bg:#08x}");
                // The keycap border is drawn in the digit's colour.
                assert!(ratio(t.text_dim, bg) >= 3.0, "{theme:?} keycap border");
            }
            assert!(ratio(t.text_faint, t.surface) >= 4.5, "{theme:?} hint");
        }
    }

    #[test]
    fn the_default_row_outline_is_visible_on_the_card_in_light_and_dark() {
        use crate::ui::theme::{Theme, DARK, LIGHT};
        for (theme, t) in [(Theme::Light, LIGHT), (Theme::Dark, DARK)] {
            let accent = crate::ui::card_buttons::system_accent(theme, t.surface);
            assert!(ratio(accent, t.surface) >= 3.0, "{theme:?}: accent outline");
        }
    }

    #[test]
    fn the_high_contrast_outline_uses_system_colours_that_differ_from_the_card() {
        use crate::ui::theme::{HC_BLACK, HC_WHITE};
        for sys in [HC_BLACK, HC_WHITE] {
            assert!(ratio(sys.highlight, sys.window) >= 3.0);
        }
    }

    // -- the card, on a real window -----------------------------------------

    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE, WM_LBUTTONUP};

    fn instance() -> HINSTANCE {
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        HINSTANCE(h.0)
    }

    fn rows(n: usize) -> Vec<OptionRow> {
        [
            ("review-this-email", "Review this email to Dana"),
            ("explain-screen", "Explain what is on screen"),
            ("translate", "Translate to Spanish"),
            ("extract-text", "Copy text from screen"),
        ]
        .iter()
        .take(n)
        .map(|(id, label)| OptionRow {
            id: id.to_string(),
            label: label.to_string(),
        })
        .collect()
    }

    const BEHAVIOR: OptionsBehavior = OptionsBehavior {
        dismiss_secs: 20,
        claim_keys: true,
        key_runs_first: false,
    };

    fn new_card() -> Card {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card
    }

    /// Every message of `msg` queued for the card's window, drained. The
    /// filter names the one message: an unfiltered peek would hand back a
    /// pending `WM_PAINT` forever (a removed `WM_PAINT` leaves its region
    /// invalid), and the loop would never end.
    fn drain(card: &Card, msg: u32) -> Vec<(usize, isize)> {
        let mut out = Vec::new();
        let mut m = MSG::default();
        while unsafe { PeekMessageW(&mut m, Some(card.hwnd()), msg, msg, PM_REMOVE).as_bool() } {
            out.push((m.wParam.0, m.lParam.0));
        }
        out
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        options_keys::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn child_count(card: &Card) -> usize {
        card.inner.options.as_ref().map_or(0, |o| o.buttons.len())
    }

    #[test]
    fn the_shell_has_no_buttons_and_claims_only_esc() {
        let _g = lock();
        let mut card = new_card();
        card.show_options(5, "Looking at your screen", &[], BEHAVIOR);
        assert_eq!(card.state(), CardState::Options);
        assert_eq!(card.options_generation(), Some(5));
        assert_eq!(child_count(&card), 0);
        let claim = options_keys::current().expect("the shell claims Esc");
        assert_eq!((claim.generation, claim.count), (5, 0));
        card.hide();
        assert_eq!(options_keys::current(), None);
    }

    #[test]
    fn ready_rows_become_real_buttons_named_by_label_and_position() {
        let _g = lock();
        let mut card = new_card();
        card.show_options(1, "Compose window to Dana", &rows(3), BEHAVIOR);
        assert_eq!(child_count(&card), 3);
        let names: Vec<String> = card
            .inner
            .options
            .as_ref()
            .unwrap()
            .buttons
            .iter()
            .map(|h| window_text(*h))
            .collect();
        assert_eq!(names[0], "Review this email to Dana, 1 of 3");
        assert_eq!(names[2], "Translate to Spanish, 3 of 3");
        let claim = options_keys::current().unwrap();
        assert_eq!(claim.count, 3, "digits 1 to 3 are claimed");
        card.hide();
    }

    #[test]
    fn more_than_four_rows_show_four() {
        let _g = lock();
        let mut card = new_card();
        let mut many = rows(4);
        many.push(OptionRow {
            id: "x".into(),
            label: "Fifth".into(),
        });
        card.show_options(1, "Summary", &many, BEHAVIOR);
        assert_eq!(child_count(&card), 4);
        card.hide();
    }

    #[test]
    fn the_shell_becomes_ready_once_and_re_arms_the_claim_for_the_rows() {
        let _g = lock();
        let mut card = new_card();
        card.show_options(2, "Looking at your screen", &[], BEHAVIOR);
        let shell_h = unsafe {
            let mut rc = RECT::default();
            let _ = GetClientRect(card.hwnd(), &mut rc);
            rc.bottom
        };
        assert!(card.set_options(2, "Compose window", &rows(3)));
        assert_eq!(child_count(&card), 3);
        assert_eq!(options_keys::current().unwrap().count, 3);
        let ready_h = unsafe {
            let mut rc = RECT::default();
            let _ = GetClientRect(card.hwnd(), &mut rc);
            rc.bottom
        };
        assert!(ready_h > shell_h, "the card grows for the rows");
        card.hide();
    }

    #[test]
    fn a_late_reply_for_another_generation_changes_nothing() {
        let _g = lock();
        let mut card = new_card();
        card.show_options(3, "Looking at your screen", &[], BEHAVIOR);
        assert!(!card.set_options(2, "Stale", &rows(2)));
        assert_eq!(child_count(&card), 0);
        card.hide();
        // After a hide the same generation is gone too.
        assert!(!card.set_options(3, "Late", &rows(2)));
        assert_eq!(card.state(), CardState::Hidden);
    }

    #[test]
    fn hiding_or_replacing_the_card_destroys_the_buttons_and_stops_the_claim() {
        let _g = lock();
        let mut card = new_card();
        card.show_options(1, "S", &rows(2), BEHAVIOR);
        let first = card.inner.options.as_ref().unwrap().buttons[0];
        card.show_answer("Done", "", 0, None);
        assert_eq!(card.state(), CardState::Collapsed);
        assert!(card.inner.options.is_none());
        assert_eq!(
            options_keys::current(),
            None,
            "an answer card claims nothing"
        );
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(first)) }.as_bool(),
            "the row button must be destroyed"
        );
        // Every other way out of the state clears the claim as well.
        for leave in [
            (|c: &mut Card| c.show_pending(PendingStage::Working)) as fn(&mut Card),
            |c: &mut Card| c.show_error("e", "d"),
            |c: &mut Card| c.hide(),
        ] {
            card.show_options(1, "S", &rows(2), BEHAVIOR);
            assert!(options_keys::current().is_some());
            leave(&mut card);
            assert_eq!(options_keys::current(), None);
            assert_ne!(card.state(), CardState::Options);
        }
    }

    #[test]
    fn a_card_that_is_dropped_while_showing_stops_the_claim() {
        let _g = lock();
        {
            let mut card = new_card();
            card.show_options(1, "S", &rows(2), BEHAVIOR);
            assert!(options_keys::current().is_some());
        }
        assert_eq!(options_keys::current(), None);
    }

    #[test]
    fn with_the_claim_switched_off_the_card_claims_nothing() {
        let _g = lock();
        options_keys::clear();
        let mut card = new_card();
        let off = OptionsBehavior {
            claim_keys: false,
            ..BEHAVIOR
        };
        card.show_options(1, "S", &rows(3), off);
        assert_eq!(options_keys::current(), None);
        assert!(card.set_options(1, "S", &rows(2)));
        assert_eq!(options_keys::current(), None);
        card.hide();
    }

    #[test]
    fn the_auto_dismiss_timer_closes_the_card_and_stops_the_claim() {
        let _g = lock();
        let mut card = new_card();
        card.show_options(1, "S", &rows(2), BEHAVIOR);
        card.inner
            .handle_message(WM_TIMER, WPARAM(TIMER_DISMISS), LPARAM(0));
        assert_eq!(card.state(), CardState::Hidden);
        assert_eq!(options_keys::current(), None);
    }

    #[test]
    fn clicking_a_row_posts_its_number_and_the_generation_to_the_owner() {
        let _g = lock();
        let mut card = new_card();
        drain(&card, WM_APP_OPTIONS_CHOICE);
        card.show_options(41, "S", &rows(3), BEHAVIOR);
        let second = card.inner.options.as_ref().unwrap().buttons[1];
        // A real click: press and release inside the button, through the
        // button's own window procedure (so the subclass, not a shortcut,
        // does the work).
        unsafe {
            SendMessageW(
                second,
                WM_LBUTTONDOWN,
                Some(WPARAM(1)),
                Some(LPARAM(0x0005_0005)),
            );
            SendMessageW(
                second,
                WM_LBUTTONUP,
                Some(WPARAM(0)),
                Some(LPARAM(0x0005_0005)),
            );
        }
        // The subclass posts WM_COMMAND to the card; the card's own handler
        // turns it into the owner message.
        let commands = drain(&card, WM_COMMAND);
        assert_eq!(commands.len(), 1, "one click, one command");
        card.inner
            .handle_message(WM_COMMAND, WPARAM(commands[0].0), LPARAM(commands[0].1));
        assert_eq!(
            drain(&card, WM_APP_OPTIONS_CHOICE),
            vec![(2, 41)],
            "row 2 on generation 41"
        );
        assert_eq!(options_keys::current(), None, "a pick spends the claim");
        card.hide();
    }

    #[test]
    fn a_release_outside_the_button_is_not_a_click() {
        let _g = lock();
        let mut card = new_card();
        card.show_options(1, "S", &rows(2), BEHAVIOR);
        let first = card.inner.options.as_ref().unwrap().buttons[0];
        drain(&card, WM_COMMAND);
        unsafe {
            SendMessageW(
                first,
                WM_LBUTTONDOWN,
                Some(WPARAM(1)),
                Some(LPARAM(0x0005_0005)),
            );
            // x = -50: well outside the button.
            SendMessageW(
                first,
                WM_LBUTTONUP,
                Some(WPARAM(0)),
                Some(LPARAM(0x0005_FFCE)),
            );
        }
        assert!(drain(&card, WM_COMMAND).is_empty());
        card.hide();
    }

    #[test]
    fn a_command_for_a_button_that_does_not_exist_posts_nothing() {
        let _g = lock();
        let mut card = new_card();
        card.show_options(1, "S", &rows(2), BEHAVIOR);
        drain(&card, WM_APP_OPTIONS_CHOICE);
        for id in [
            ID_OPTION_BASE - 1,
            ID_OPTION_BASE + 2,
            ID_OPTION_BASE + 99,
            0,
            -5,
        ] {
            card.inner
                .handle_message(WM_COMMAND, WPARAM(id as usize), LPARAM(0));
        }
        assert!(drain(&card, WM_APP_OPTIONS_CHOICE).is_empty());
        card.hide();
    }

    #[test]
    fn the_card_never_takes_focus_while_options_show() {
        let _g = lock();
        let mut card = new_card();
        card.show_options(1, "S", &rows(3), BEHAVIOR);
        let ex = unsafe { GetWindowLongPtrW(card.hwnd(), GWL_EXSTYLE) } as u32;
        assert_ne!(ex & WS_EX_NOACTIVATE.0, 0, "the card must stay no-activate");
        let first = card.inner.options.as_ref().unwrap().buttons[0];
        let before = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() };
        unsafe {
            SendMessageW(
                first,
                WM_LBUTTONDOWN,
                Some(WPARAM(1)),
                Some(LPARAM(0x0005_0005)),
            );
            SendMessageW(
                first,
                WM_LBUTTONUP,
                Some(WPARAM(0)),
                Some(LPARAM(0x0005_0005)),
            );
        }
        let after = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() };
        assert_eq!(before, after, "clicking a row must not move the focus");
        card.hide();
    }

    #[test]
    fn painting_the_card_in_every_theme_and_scale_never_panics_and_draws_something() {
        let _g = lock();
        for theme in [Theme::Light, Theme::Dark, Theme::HighContrast] {
            for dpi in [96u32, 144, 192, 240] {
                for key_runs_first in [false, true] {
                    let mut card = new_card();
                    card.force_theme_for_gallery(theme);
                    card.force_dpi_for_gallery(dpi);
                    let behavior = OptionsBehavior {
                        key_runs_first,
                        ..BEHAVIOR
                    };
                    card.show_options(1, "Looking at your screen", &[], behavior);
                    let (shell, w, h) = card.inner.render_rgba().expect("the shell paints");
                    assert!(w > 0 && h > 0 && shell.len() == (w * h * 4) as usize);
                    assert!(card.set_options(1, "Compose window to Dana", &rows(4)));
                    let (ready, w2, h2) = card.inner.render_rgba().expect("the rows paint");
                    assert!(h2 > h && w2 == w, "{theme:?} {dpi}");
                    assert_ne!(shell, ready[..shell.len().min(ready.len())].to_vec());
                    card.hide();
                }
            }
        }
    }

    #[test]
    fn a_row_button_paints_its_keycap_label_and_default_outline() {
        let _g = lock();
        // Draws one row through the real WM_DRAWITEM path into a memory DC and
        // checks the default row differs from a plain one (the outline), so
        // the draw code is wired to the row it should be.
        let paint_row = |key_runs_first: bool| -> Vec<u8> {
            let mut card = new_card();
            card.force_theme_for_gallery(Theme::Light);
            let behavior = OptionsBehavior {
                key_runs_first,
                ..BEHAVIOR
            };
            card.show_options(1, "S", &rows(3), behavior);
            let button = card.inner.options.as_ref().unwrap().buttons[0];
            let bytes = render_button(&card, button);
            card.hide();
            bytes
        };
        let plain = paint_row(false);
        let default = paint_row(true);
        assert!(!plain.is_empty());
        assert_ne!(plain, default, "the default row must be drawn differently");
    }

    /// Draws `button` through `draw_option_button` into a memory bitmap and
    /// returns its pixels.
    fn render_button(card: &Card, button: HWND) -> Vec<u8> {
        use windows::Win32::Graphics::Gdi::{
            CreateDIBSection, GdiFlush, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };
        use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
        let mut rc = RECT::default();
        unsafe { GetClientRect(button, &mut rc).expect("button rect") };
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut out = Vec::new();
        unsafe {
            let dc = CreateCompatibleDC(None);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels: *mut c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut pixels, None, 0)
                .expect("DIB");
            let old = SelectObject(dc, HGDIOBJ(bmp.0));
            let dis = DRAWITEMSTRUCT {
                CtlID: ID_OPTION_BASE as u32,
                hwndItem: button,
                hDC: dc,
                rcItem: rc,
                ..Default::default()
            };
            card.inner
                .draw_option_button(LPARAM(&dis as *const DRAWITEMSTRUCT as isize));
            let _ = GdiFlush();
            out.extend_from_slice(std::slice::from_raw_parts(
                pixels as *const u8,
                (w * h * 4) as usize,
            ));
            SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(dc);
        }
        out
    }

    #[test]
    fn the_options_state_leaves_the_other_states_alone() {
        // The options code is reached only in its own state: a preview or an
        // answer never grows row buttons, and `WM_COMMAND` outside Options
        // does nothing with an option id.
        let _g = lock();
        let mut card = new_card();
        card.show_answer("Right.", "Details.", 0, None);
        drain(&card, WM_APP_OPTIONS_CHOICE);
        card.inner
            .handle_message(WM_COMMAND, WPARAM(ID_OPTION_BASE as usize), LPARAM(0));
        assert!(drain(&card, WM_APP_OPTIONS_CHOICE).is_empty());
        assert!(card.inner.options.is_none());
    }
}
