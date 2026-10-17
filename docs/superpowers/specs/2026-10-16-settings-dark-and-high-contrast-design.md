# Settings window: dark mode and high contrast (#582, part of #427, #362)

Status: **accepted, 2026-10-16**

Scope: `src/ui/settings.rs` (the Win32 window and its controls), a small
paint helper next to it, `src/ui/theme.rs` (tokens for control states), a
Windows application manifest if Slice 0 shows one is needed, and the
`--ui-gallery --theme` override extended to the Settings states. Out of scope:
the palette (its own slice of #362), moving theme detection out of `card.rs`
(#657), any change to what Settings edits or saves.

## Why a design doc (rule 12)

Settings is made of stock Win32 controls, so unlike the card it cannot simply
paint from `theme.rs`. Following the theme means replacing how most of its
controls are drawn. That changes the module's structure, so the approach is
written down and agreed before code.

## Where it is today (checked against master)

- Settings is always drawn light. The module doc in `settings.rs` says so on
  purpose: a correct light window beats a half-dark one. It handles none of
  `WM_CTLCOLOR*`, `WM_THEMECHANGED`, `WM_SETTINGCHANGE` or `WM_SYSCOLORCHANGE`
  today, so re-theming is new work here, not a copy of the card's.
- **The exe carries a common-controls v6 manifest** (`assets/app.manifest`,
  embedded by `assets/app.rc`, #807), so Settings runs v6 controls. Slice 0
  measured v5 only because its probe had no manifest; that is not how the exe
  runs. MEASURED 2026-10-17: a Settings button answers `BCM_GETIDEALSIZE`
  (a v6-only message) in the test process, which embeds the same resource, and
  `tests/manifest_embed.rs` checks the built exe. This matters:
  `NM_CUSTOMDRAW` for buttons is a v6 feature.
- Control inventory: `EDIT` (all single-line edits are created with
  `WS_EX_BORDER`, a thin frame-colour border, not a sunken one), `COMBOBOX`
  (all `CBS_DROPDOWNLIST | CBS_HASSTRINGS`), `BS_AUTOCHECKBOX`, `BS_GROUPBOX`,
  `BS_PUSHBUTTON` and `BS_DEFPUSHBUTTON`, one trackbar, and a body window with
  `WS_VSCROLL`. **There is no list box and no radio button.** The only
  `WM_CTLCOLORLISTBOX` traffic is from a combo's drop-down list.
- Other surfaces already solved parts of this. `card.rs` answers
  `WM_CTLCOLOREDIT` for its edit fields (#350) and `card_buttons.rs`
  owner-draws push buttons with hover and pressed states (#351). Reuse both
  instead of writing a second version.
- High contrast: stock controls already follow the system colours, so Settings
  is probably correct in high contrast today for free (still unmeasured:
  Slice 0 could not switch themes, so it is a manual check in the last slice). A plan that replaces stock controls in every theme would regress
  that.

## Decision

Draw the controls ourselves, from the shared tokens, **in dark mode only**.
Light mode and high contrast keep today's stock controls, untouched. That makes
every slice safe to ship alone: the only users who can see custom paint are
the ones who get dark mode, and the high-contrast path is never a regression
surface.

Do **not** use the undocumented uxtheme ordinals (`SetPreferredAppMode`,
`AllowDarkModeForWindow`, `FlushMenuThemes`, ordinals 133 to 136): they have
changed meaning between Windows builds and a wrong guess fails silently, which
is the failure this repo guards against (rule 8). Everything below uses
documented APIs. The one undocumented string, the `DarkMode_Explorer` theme
name, is allowed only for the body scrollbar, as a measured, fail-back route (see
below). It is widely reported to need those ordinals, so Slice 0 measured it
on this machine, and it works there for the scrollbar only.

Keeping the native control classes (EDIT, COMBOBOX, BUTTON, trackbar) and
changing only how they paint means keystrokes, tab order, IME and most of
accessibility keep working as they do today.

## Slice 0: measurements (MEASURED 2026-10-16)

Slice 0 changed no product behaviour. It was a throwaway test window (test-only
class name, no registry or theme setting touched) run on Windows 11 build
26300 with the apps colour mode set to dark, once with no manifest (the way
Settings runs today) and once with an activation context built from a temp
common-controls 6.0.0.0 manifest. Results replace the theories above. Items 3
and 6 were not measured, see below. The throwaway code is not kept.

1. **Common-controls version. MEASURED 2026-10-16: v5 without a manifest.**
   With `InitCommonControlsEx(ICC_BAR_CLASSES)` and no manifest, the loaded
   `comctl32.dll` is the WinSxS 5.82 one and `DllGetVersion` reports 5.82. With
   the v6 activation context it is WinSxS 6.0 and reports 6.16. The probe had
   no manifest, which is not how the exe runs: the exe has shipped the v6
   manifest since #807, so Settings runs v6 (MEASURED 2026-10-17, see
   "Where it is today").
2. **`NM_CUSTOMDRAW` and `BS_*` buttons. MEASURED 2026-10-16.** Under v5:
   none arrives for a checkbox, radio, group box or push button (a trackbar
   does send it, on both versions). Under v6: it arrives (`CDDS_PREPAINT`) for
   `BS_AUTOCHECKBOX`, `BS_AUTORADIOBUTTON` and `BS_PUSHBUTTON`, and answering
   `CDRF_SKIPDEFAULT` after filling the rect replaces the control's paint.
   **It does not arrive for `BS_GROUPBOX`, even under v6**, and obviously not
   for `BS_OWNERDRAW`. This contradicts the group box line below: the group
   box cannot ride the manifest route. See "Checkboxes and group boxes".
3. **UI Automation role. MEASURED 2026-10-16: the theory is right.**
   `BS_OWNERDRAW` is exposed as ControlType Button (50000) with no Toggle
   pattern, so a checkbox made owner-drawn loses its role and checked state. A
   `BS_AUTOCHECKBOX` that is custom-drawn (v6 `NM_CUSTOMDRAW`, fully
   repainted) keeps ControlType CheckBox (50002) and the Toggle pattern, with
   the state following `BM_SETCHECK`. A `CBS_OWNERDRAWFIXED` combo keeps
   ControlType ComboBox (50003), same as a stock one. Push buttons and radios
   are Button (50000) and RadioButton (50013) as stock.
4. **Combo arrow.** Not measured. The subclass alternative (paint the whole
   closed face in `WM_PAINT` and `WM_PRINTCLIENT`) is real work and belongs to
   the combo slice's own gallery check, so it stays a design decision, not a
   measurement.
5. **`DarkMode_Explorer` without ordinals. MEASURED 2026-10-16**, with the
   apps colour mode set to dark (read only; the light-mode result is not
   measured, since the setting must not be changed). `SetWindowTheme(hwnd,
   "DarkMode_Explorer", None)`, no ordinal call:
   - **Non-client scrollbar (`WS_VSCROLL`): yes**, on both v5 and v6. The bar
     strip went from near white (luminance 240) to near black (29). This is
     the body's scrollbar case. It was measured on a multiline edit rather
     than on the Settings body window, which is the same kind of scrollbar.
   - **Combo closed face: no change** (0 pixels), v5 and v6.
   - **Combo drop-down list: effectively no.** Themed on the list window, 212
     pixels changed of a 200x50 list, so a frame at most. The fill and text
     still come from `WM_CTLCOLORLISTBOX`.
   - **Trackbar: no change**, v5 and v6.
   - **Edit: v6 only**, and it goes fully dark (luminance 236 to 41); on v5 it
     changes nothing. That would fight the `WM_CTLCOLOREDIT` plan, so do not
     apply the theme to edits.
6. **`PrintWindow` on children. MEASURED 2026-10-16: works.** With
   `PW_RENDERFULLCONTENT`, a capture of a child and of the top-level window
   both contain the pixels of an owner-drawn button and of a v6 custom-drawn
   checkbox (thousands of the marker colour in each), and a stock checkbox
   captures fully. The pixel tests and the gallery can rely on it. Under v5 a
   custom-drawn checkbox is of course never repainted (see 2).
7. **High contrast today.** Not measured: it needs Aquatic and Desert switched
   on, which changes the system theme settings. It stays a named manual check
   in the last slice, so "stock controls are the baseline" is still unverified.
8. **Size.** Not built in release (the machine is shared). A minimal manifest
   is well under 1 KB of text; a resource entry adds a few dozen bytes of
   directory, and the resource section rounds up to the file alignment. So
   `THEORY (unverified)`: the manifest costs about 1 KB at most. The release
   build in the manifest slice records the real number.

**What changes in the design.** (a) Checkboxes: the manifest route is
confirmed as the way, and nothing but v6 can custom-draw a real checkbox;
`BS_OWNERDRAW` is the rejected fallback with the measured role loss. (b)
Group boxes cannot use `NM_CUSTOMDRAW`; see below. (c) The scrollbar's
documented route works, so it is used for the body scrollbar (questions 3 and 5). (d) Do not
theme edits or the combo list with `DarkMode_Explorer`. (e) The light-mode
scrollbar result is unmeasured and gets a note in slice 5's check.
## The theme source

`Theme::resolve(apps_use_light_theme, high_contrast)` and `Tokens::for_theme`
already exist and already serve the card and the region overlay. Settings adds
the tokens it needs and the card did not: `control_fill` (edit and combo
background), `control_border`, `control_border_hot`, `accent` (checked box,
trackbar fill, default button), `accent_text`, and `disabled_text`. In dark
they are literal tokens. High contrast does not use them: those controls stay
stock and draw from the system. No literal colour appears in the paint code,
and a source-scan test (see Testing) keeps it that way, as it does for the
overlay.

`control_border` is its own token, not `Tokens.border`: the existing light
`border` (214 on 250) is about 1.4:1 and is decorative, while a control edge
has to reach 3:1 against its surroundings (see Accessibility).

## Per control

### Title bar

`DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)` (documented, Windows 10
20H1 and later; the `Win32_Graphics_Dwm` feature is already enabled). Set to
true for dark, false for light, and left alone in high contrast, where the
system draws the caption. If the call fails the title bar stays light: the body
is still correct, so this fails safe.

### Static text, edit

Handled in the parent's `WM_CTLCOLORSTATIC` and `WM_CTLCOLOREDIT` (and the
combo list's `WM_CTLCOLORLISTBOX`, below): set the text and background
colours, return a cached `HBRUSH` for the theme (recreated on a theme change,
deleted on destroy). Reuse the card's #350 pattern. Read-only and disabled
edits use `disabled_text`.

Edits are created with `WS_EX_BORDER`, a 1 px frame in the system frame colour
(`COLOR_WINDOWFRAME`, near black), which would be near-black on near-black in
dark. **Decision: a custom frame is part of this design, not a maybe.** Keep
`WS_EX_BORDER` (it reserves the non-client area), and paint the frame in
`control_border` (hot: `control_border_hot`; focused: `accent`) in a
`WM_NCPAINT` handler on a subclass. The colours are settled from the slice 1 screenshots (question 2).

### Push buttons

**Decision: `BS_OWNERDRAW` for push buttons, reusing `card_buttons.rs`.** A
push button is already a push button to assistive tech, so nothing is lost,
and the paint helper, hover and pressed states already exist from #351. The
Settings default button (`BS_DEFPUSHBUTTON`) keeps its Enter behaviour; the
owner-draw style is added to it.

### Checkboxes and group boxes

The role problem applies to checkboxes only (there are no radios), so the
decision is per control and depends on Slice 0:

- **Slice 2 uses it for push buttons too (Decision).** Push buttons were
  specified as `BS_OWNERDRAW`. Custom draw reaches them the same way (MEASURED
  2026-10-16, item 2) and keeps the button's own class, style and UI Automation
  role, so Settings has one mechanism for buttons and checkboxes
  (`settings_buttons.rs`) and still reuses the card's button look and paint.
- **Manifest route (Decision; shipped in #807, and Slice 0 confirmed that v6
  sends the notification):** add the
  common-controls 6.0.0.0 manifest, keep `BS_AUTOCHECKBOX`, and draw in
  `NM_CUSTOMDRAW` (a v6 button sends it to its parent through `WM_NOTIFY`;
  return `CDRF_SKIPDEFAULT` after painting). The class and style stay a real
  checkbox, so role, toggle state and `BM_GETCHECK` keep working with no
  extra code.
- **If the manifest is rejected:** `BS_OWNERDRAW` checkboxes (measured: the role
  drops to a plain button with no toggle state), with
  `WM_GETOBJECT` for `OBJID_CLIENT` answered by an `IAccessible` wrapper that
  reports the checkbox role and checked state. Real code to get right, with
  its own tests, and the reason the manifest route is preferred.
- Group boxes: **`NM_CUSTOMDRAW` does not arrive for `BS_GROUPBOX` even under
  v6 (MEASURED 2026-10-16)**, so they cannot use the checkbox route. Paint the
  frame in a subclass (`WM_PAINT`, frame in `control_border`, caption drawn
  over a gap cut in the frame), keeping the `BS_GROUPBOX` class. THEORY
  (unverified): a group box reads as a group or static text to assistive tech,
  so its role is not a regression risk; verify it in the Narrator pass.

A shared helper draws each control from `(state, tokens, dpi)` and is a pure
layout function plus a thin GDI layer, so the layout half is unit-tested.
States drawn: normal, hot (mouse over), pressed, checked, disabled, focused.

### Combo boxes and their drop-downs

All combos are `CBS_DROPDOWNLIST`. `CBS_OWNERDRAWFIXED` with `WM_DRAWITEM`
paints the closed field (item -1 with `ODS_COMBOBOXEDIT`) and each list item
but **not the arrow button**, which stays system-drawn and light. So the combo
needs a paint subclass that draws the whole closed face, field and chevron,
in `WM_PAINT` and `WM_PRINTCLIENT`, with `WM_DRAWITEM` for the list items.
The role stays a combo box; `CBS_HASSTRINGS` stays set so item text is
reachable. This is more than "owner-draw the combo", and the slice is sized
for it.

The open list is a separate window the control owns. It gets
`WM_CTLCOLORLISTBOX` (fill and text) in the parent. What stays stock is its
frame and, when it scrolls, its scrollbar. Slice 0 measured
`SetWindowTheme(list, "DarkMode_Explorer", None)` as changing almost nothing
(MEASURED 2026-10-16), so it is not used: the list keeps the stock frame.

### Trackbar

The trackbar (one, for the card seconds or the image edge) is a v5 or v6
common control. Draw it in `NM_CUSTOMDRAW` stages `CDDS_ITEMPREPAINT` for
channel, thumb and ticks, using `control_fill`, `accent` and `control_border`.
Trackbar custom draw works on both comctl32 versions, so this slice does not
depend on the manifest. Keeping the native trackbar keeps arrow-key and page
behaviour.

### Scrollbar

The body scrolls through a `WS_VSCROLL` on the body window. That is a
non-client scrollbar, which cannot be recoloured by painting, and it is also
where the window's UI Automation scroll pattern comes from. A hidden native
scrollbar child does not give assistive tech a scrollable pane. So this is the
weakest piece of the plan and is ordered by risk:

1. **Use the documented route.** `SetWindowTheme(body, "DarkMode_Explorer",
   None)`. MEASURED 2026-10-16: it darkens a `WS_VSCROLL` bar with no ordinal
   call (dark colour mode; the light-mode case is unmeasured, so slice 5
   re-checks it), and the scroll pattern is untouched.
2. **If it does not, keep the stock (light) scrollbar** and accept a light strip
   in a dark window. This is the accessible choice and the default fallback.
3. **A custom owned scrollbar is allowed only with a UI Automation provider**
   (an `IRawElementProviderSimple` with `IScrollProvider`) that keeps the
   scroll pattern. That is a large piece of work, so it is not built.
   Decision: see question 5.

### Focus rects and keyboard cues

Windows hides focus rects and accelerator underlines until the keyboard is
used (`WM_UPDATEUISTATE`, `UISF_HIDEFOCUS`, `UISF_HIDEACCEL`). Owner painting
must respect that, or keyboard users lose their place. Each draw helper reads
`CDIS_FOCUS` plus the control's UI state and, when visible, draws a 2 px focus
frame in `text` offset 2 px outside the control, and underlines the `&`
mnemonic. A control that has focus but a hidden cue draws no rect, exactly as
the stock one does. The focus frame reaches 3:1 against the window background.

## High contrast

**Decision: in high contrast, and in light mode, Settings keeps stock controls
and none of the paint code runs.** The system draws every control from
`SysColors`, as it does today, and the title bar is left to the system. High
contrast wins over the light and dark app mode, as `Theme::resolve` already
does: with it on, no `WM_CTLCOLOR*` handler overrides the colours and no
owner-draw or custom-draw path is taken. This removes the largest regression
surface: nothing a high-contrast user sees changes. If Slice 0's manifest route
is taken, light and high contrast do change to v6 visuals, which is why that
slice is checked in Aquatic and Desert before it lands.

Decision: light mode does not use the custom paint (question 4).

## Live re-theme

The Settings window may be open when the user switches mode. This is new work
in `settings.rs`, which handles none of these messages today. It handles
`WM_SETTINGCHANGE` (when `setting_change_affects_theme`; a top-level window
receives it even inside the modal loop, confirmed by the manual "switch colour
mode while open" check), `WM_THEMECHANGED` and `WM_SYSCOLORCHANGE`. On any of
them: re-resolve the theme and `SysColors`, rebuild the cached brushes, set
the title-bar attribute, switch between the stock and custom paths, then
`RedrawWindow(RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW)` so every child
repaints. A forced theme (gallery) ignores them. The body window and the child
controls get `WM_THEMECHANGED` forwarded automatically by Windows, and must not
re-resolve independently: one owner (the top-level proc) resolves and the rest
read it.

## DPI

Nothing changes in how layout scales (`WM_DPICHANGED` is already handled, #233).
Paint helpers take the window's current DPI and scale every pixel constant
(border, radius, glyph size, focus offset) through the existing dp-to-px
function. Cached brushes and pens do not depend on DPI. Fonts already follow
the DPI path; owner-drawn controls use the control's own `WM_GETFONT` font so
they follow it for free.

## Accessibility

Owner painting can break accessibility if it changes the control's identity.

- The role decision per control is in "Per control" and is gated on Slice 0's
  measurement, which replaces the theory above with a recorded result.
- Combos keep the combo role under owner draw; the item text must still be
  reachable, so `CBS_HASSTRINGS` stays set.
- Colour is never the only cue: checked state is a glyph, focus is a frame,
  disabled is greyed text plus no hot state.
- **Text contrast:** every text pair in dark meets 4.5:1, enforced by tests over
  the tokens (as `theme.rs` does today). The new tokens are added to that test.
- **Non-text contrast (WCAG 1.4.11):** `control_border` against the adjacent
  background, the checked glyph (`accent` against `control_fill`), and the focus
  frame each meet 3:1, enforced by the same kind of token test. The existing
  `border` token does not, which is why `control_border` exists.
- The scrollbar keeps (or, if replaced, re-provides) the UI Automation scroll
  pattern.
- Narrator reads the screen through UIA, so a pass with Narrator over the
  Settings window, in dark and in high contrast, is a named manual check for the
  last slice.

## `--ui-gallery --theme` for Settings

`Card` has `force_theme_for_gallery`. Settings gets the same on `GalleryWindow`:
it sets a forced theme and the Windows high-contrast black `SysColors`
(`HC_BLACK`), then repaints. The driver passes the `--theme` value to the
Settings states as it already does for the card and the region overlay. The four
existing Settings states then give 12 screenshots (light, dark, high contrast),
where light and high contrast show the stock look. `HC_WHITE` is covered by
tests on the tokens only, since the gallery takes one name `high-contrast`. No
registry value is ever written.

## Testing

- Pure and unit-tested: the token contrast (text 4.5:1 and the non-text 3:1
  cases), the checkbox, group box, combo and trackbar layout geometry as a
  function of size and DPI, and the "which state is this" decision from
  `CDIS_*`/`ODS_*` flags.
- Source-scan guard, CRLF-normalised with a non-empty candidate check: the
  paint helpers contain no `rgb(`, no `COLORREF(` followed by a digit and no
  stock brush or pen other than `NULL_BRUSH`. The same guard asserts that no
  paint path is reachable when high contrast is on.
- Dispatch guards: the top-level proc has arms for the three theme messages,
  `WM_CTLCOLORSTATIC`, `WM_CTLCOLOREDIT`, `WM_CTLCOLORLISTBOX` and `WM_NOTIFY`
  (custom draw); push buttons and combo list items have `WM_DRAWITEM`.
- Real Win32 (test class names, rule 9): create the window, force each theme,
  and read back pixels from a `PrintWindow` capture for one control of each
  kind (a checkbox glyph pixel, a combo arrow pixel, an edit frame pixel). This
  is the "wired to nothing" check for owner painting: with no handler wired,
  the pixel would be the stock light colour. Slice 0 item 6 measured that it holds.
- Manual, named in each PR: screenshots in all three themes (looked at by
  eye), a real switch of Windows colour mode with Settings open, real high
  contrast (Aquatic and Desert) at 100% and 200% scale, keyboard-only
  navigation, Narrator.

## Slices (one PR each)

Every slice leaves light mode and high contrast exactly as they are (stock),
and dark mode gated, so each is safe to ship alone. The one exception is the
manifest slice, which is a deliberate light-mode change.

0. **Measurements** (no product change): the seven items above, recorded in
   this spec. Decides the manifest, the checkbox route, the combo approach and
   the scrollbar approach.
0b. **Manifest** (decided, question 1): `assets/app.manifest`
    requesting common-controls 6.0.0.0, wired through `app.rc`. Checked in
    light and high contrast at 100%, 150% and 200%, then shipped alone with a
    release note.
1. **Plumbing**: theme state on the Settings window, new tokens, DWM title bar,
   `WM_CTLCOLOR*` for static and edit, the custom edit frame, live re-theme,
   gallery `--theme`. The dark path is gated by one constant,
   `SETTINGS_DARK_ENABLED = false`, so no user sees a half-dark window.
   Done when the gallery shows the dark and high-contrast states and the
   stock states are byte-identical to before.
2. **Buttons**: push buttons (reusing `card_buttons.rs`), checkboxes, group
   boxes, focus and keyboard cues.
3. **Combo boxes**: the paint subclass for the closed face, list items, and
   the drop-down list.
4. **Trackbar.**
5. **Scrollbar**: the documented route, or the stock fallback (no custom
   bar, question 5).
6. **Verification pass and release**: the Narrator pass, real high contrast,
   Windows colour-mode switch while open; the `SETTINGS_DARK_ENABLED` constant
   is deleted in this slice and a source-scan test asserts the identifier no
   longer exists; and the "Theme" note in the module doc of `settings.rs` is
   rewritten to say what is true.

### Size budget

The gate is `perf-budgets.json` `exe_size_bytes.max` 4,500,000 against a last
measurement of 3,982,336, about 518 KB of headroom shared with the options
card, overlay and knowledge specs. No new crate and no new `windows` feature is
planned (`Win32_Graphics_Dwm`, `Win32_UI_Controls` and GDI are already on;
`SetWindowTheme` lives in `Win32_UI_Controls`). Estimates, THEORY
(unverified) until the slices measure them (Slice 0 did not build in release): tokens and the shared paint
helper about 8 KB, buttons and checkboxes about 6 KB, the combo subclass about
8 KB, trackbar about 2 KB, an `IAccessible` wrapper if needed about 6 KB, a
custom scrollbar with a UIA provider about 20 KB. About 25 to 30 KB without
the last two, and a manifest is about 1 KB. Each slice's PR records the
measured size from CI and raises the gate in the same PR if it must.

## Resolved questions

Each was an open question with a recommended answer; the recommended answer
is the decision.

1. **Decision: add the common-controls v6 manifest** (Slice 0 measured v5
   with a probe that had no manifest; the exe's own manifest landed in #807). It keeps the checkbox a real checkbox with no
   accessibility wrapper. It is a visible change for light-mode and
   high-contrast users, so it ships alone in slice 0b, checked at 100%, 150%
   and 200% and in Aquatic and Desert. The Memory window in the knowledge spec
   benefits from the same manifest.
2. **Decision: the custom edit frame is built; the exact colours and the hot
   and focus states are set from slice 1's screenshots**, within the token
   contrast tests (3:1 non-text).
3. **Decision: use `DarkMode_Explorer` for the body scrollbar only.** The
   stock look is the fallback if it ever fails. Never for edits, the combo
   face, the combo list or the trackbar (measured not to help).
4. **Decision: no custom paint in light mode.** Stock in light, so light and
   high contrast cannot regress and the custom code runs only for dark users.
5. **Decision: no custom scrollbar.** Use the documented theme route, else the
   stock scrollbar. A custom bar would need a UI Automation provider, which is
   a project in itself.
6. **Decision: dark ships at slice 6, after the Narrator pass** and the
   colour-mode-switch check. `SETTINGS_DARK_ENABLED` stays false until then.

## Related decisions

- **Decision: the Quick Ask palette sends no model request when it opens.**
  This spec does not touch the palette, so nothing here changes.
