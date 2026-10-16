# Settings window: dark mode and high contrast (#582, part of #427, #362)

Status: **proposed**

Scope: `src/ui/settings.rs` (the Win32 window and its controls), a small
paint helper next to it, `src/ui/theme.rs` (tokens for control states), and the
`--ui-gallery --theme` override extended to the Settings states. Out of scope:
the palette (its own slice of #362), moving theme detection out of `card.rs`
(#657), any change to what Settings edits or saves.

## Why a design doc (rule 12)

Settings is the one surface that is made of stock Win32 controls, so it is the
one surface that cannot simply paint from `theme.rs`. Following the theme means
replacing how most of its controls are drawn. That changes the module's
structure, so the approach is written down and agreed before code.

## Decision

Draw the controls ourselves, from the shared tokens. Do **not** use the
undocumented uxtheme ordinals (`SetPreferredAppMode`, `AllowDarkModeForWindow`,
`FlushMenuThemes`, ordinals 133 to 136): they have changed meaning between
Windows builds and a wrong guess fails silently, which is the failure this repo
guards against (rule 8). Everything below uses documented APIs, and high
contrast needs nothing else than `GetSysColor`.

Keeping the native control classes (EDIT, COMBOBOX, BUTTON, trackbar) and
changing only how they paint means keystrokes, tab order, IME and most of
accessibility keep working as they do today.

## The theme source

`Theme::resolve(apps_use_light_theme, high_contrast)` and `Tokens::for_theme`
already exist and already serve the card and the region overlay. Settings adds
a few tokens it needs and the card did not: `control_fill` (edit and combo
background), `control_border`, `control_border_hot`, `accent` (checked box,
trackbar fill, default button), `accent_text`, and `disabled_text`. In high
contrast they come from `SysColors` only: `COLOR_WINDOW`, `COLOR_WINDOWTEXT`,
`COLOR_GRAYTEXT`, `COLOR_HIGHLIGHT`, `COLOR_HIGHLIGHTTEXT`, `COLOR_HOTLIGHT`.
No literal colour appears in the paint code, and a source-scan test (see
Testing) keeps it that way, as it does for the overlay.

## Per control

### Title bar

`DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)` (documented, Windows 10
20H1 and later; the `Win32_Graphics_Dwm` feature is already enabled). Set to
true for dark, false for light, and left alone in high contrast, where the
system draws the caption. If the call fails the title bar stays light: the body
is still correct, so this fails safe.

### Static text, edit, list box

Handled in the parent's `WM_CTLCOLORSTATIC`, `WM_CTLCOLOREDIT` and
`WM_CTLCOLORLISTBOX`: set the text and background colours, return a cached
`HBRUSH` for the theme (recreated on a theme change, deleted on destroy).
Read-only and disabled edits use `disabled_text`. Edits keep the standard
sunken border; in dark it is drawn by painting a 1 px frame in the parent after
the control (via `WM_NCPAINT` on a subclass) only if the stock frame proves
unreadable in the screenshots. That is the open question below, not a decision.

### Checkboxes, radios, group boxes, push buttons

These do not honour `WM_CTLCOLOR*` for their glyph or text once visual styles
are on, so they are drawn by us. Two ways to do it:

1. `BS_OWNERDRAW` with `WM_DRAWITEM`.
2. Keep the native style (`BS_AUTOCHECKBOX`, `BS_GROUPBOX`, `BS_PUSHBUTTON`,
   `BS_DEFPUSHBUTTON`) and draw in `NM_CUSTOMDRAW`, which a common-controls v6
   button sends to its parent through `WM_NOTIFY`, returning `CDRF_SKIPDEFAULT`
   after painting.

Prefer 2. The class and style stay a real checkbox, so its UI Automation role,
toggle state and `BM_GETCHECK` keep working with no extra code (see
Accessibility). Option 1 changes the role. The paint code is the same either
way: a rounded box or radio dot, check glyph or dot in `accent_text`, label in
`text`, the group box frame in `control_border`, and the group caption drawn
over a gap cut in the frame. A shared helper draws each control from
`(state, tokens, dpi)` and is a pure layout function plus a thin GDI layer, so
the layout half is unit-tested.

States drawn: normal, hot (mouse over), pressed, checked, disabled, and focused.

### Combo boxes and their drop-downs

All combos are `CBS_DROPDOWNLIST`. Make them `CBS_OWNERDRAWFIXED` and draw both
the closed field and each list item in `WM_DRAWITEM` (the closed field is item
-1 with `ODS_COMBOBOXEDIT`). The role stays a combo box. The arrow button area
is painted by us as a chevron.

The open list is a separate window the control owns. It gets
`WM_CTLCOLORLISTBOX` (fill and text) in the parent. What stays stock is its
frame and, when it scrolls, its scrollbar. If the screenshots show a visible
light edge or scrollbar, the fallback is `SetWindowTheme(list, "DarkMode_Explorer",
None)` on that one window, found through `CB_GETCOMBOBOXINFO`. That is a
documented call with an undocumented theme name, uses no ordinals, and fails
back to the stock look; it is allowed only if measured to help.

### Trackbar and scrollbars

The trackbar (one, for the card seconds or the image edge) is drawn with
`NM_CUSTOMDRAW` stages `CDDS_ITEMPREPAINT` for channel, thumb and ticks,
using `control_fill`, `accent` and `control_border`. Keeping the native
trackbar keeps arrow-key and page behaviour.

The body scrolls through a `WS_VSCROLL` on the body window. That is a
non-client scrollbar, which cannot be recoloured by painting. Replace it with a
slim owned scrollbar painted by us in the body's right edge: the body already
computes scroll position, page and range, handles `WM_MOUSEWHEEL`, and has
the pure `WM_VSCROLL` position function, so the new bar reuses those and only
adds hit-testing (track, thumb, arrows) and paint. It also exposes scroll
patterns by keeping the `SetScrollInfo` state on a hidden native scrollbar
control, so assistive tech still sees a scrollable pane. This is the largest
single piece of work and gets its own slice.

### Focus rects and keyboard cues

Windows hides focus rects and accelerator underlines until the keyboard is
used (`WM_UPDATEUISTATE`, `UISF_HIDEFOCUS`, `UISF_HIDEACCEL`). Owner painting
must respect that, or keyboard users lose their place. Each draw helper reads
`CDIS_FOCUS` plus the control's UI state and, when visible, draws a 2 px focus
frame in `text` offset 2 px outside the control, and underlines the `&`
mnemonic. A control that has focus but a hidden cue draws no rect, exactly as
the stock one does. In high contrast the frame uses `COLOR_WINDOWTEXT` (and the
checked glyph `COLOR_HIGHLIGHT`), never a colour that could match the
background.

## High contrast

The only colours are `SysColors`. No tiers of grey and no accent beyond
`COLOR_HIGHLIGHT` and `COLOR_HOTLIGHT`. Borders are 2 px in `window_text` so
every control edge is visible. Disabled controls use `COLOR_GRAYTEXT`, never a
dimmed text colour. The title bar is left to the system. High contrast wins
over the light and dark app mode, as `Theme::resolve` already does.

## Live re-theme

The Settings window may be open when the user switches mode. It handles the
same three messages as the card and the overlay: `WM_SETTINGCHANGE` (when
`setting_change_affects_theme`), `WM_THEMECHANGED` and `WM_SYSCOLORCHANGE`. On
any of them: re-resolve the theme and `SysColors`, rebuild the cached brushes,
set the title-bar attribute, then `RedrawWindow(RDW_INVALIDATE | RDW_ALLCHILDREN |
RDW_UPDATENOW)` so every child repaints. A forced theme (gallery) ignores them.
The body window and the child controls get `WM_THEMECHANGED` forwarded
automatically by Windows, and must not re-resolve independently: one owner (the
top-level proc) resolves and the rest read it.

## DPI

Nothing changes in how layout scales (`WM_DPICHANGED` is already handled, #233).
Paint helpers take the window's current DPI and scale every pixel constant
(border, radius, glyph size, focus offset) through the existing dp-to-px
function. Cached brushes and pens do not depend on DPI. Fonts already follow
the DPI path; owner-drawn controls use the control's own `WM_GETFONT` font so
they follow it for free.

## Accessibility

Owner painting can break accessibility if it changes the control's identity.

- **`BS_OWNERDRAW` changes the role.** MEASURED nowhere yet. `THEORY
  (unverified)`: a `BS_OWNERDRAW` button is exposed to UI Automation and MSAA
  as a plain push button, losing the checkbox or radio role and the
  toggle state, because the system derives the role from the style bits.
  That is why option 2 (native style plus `NM_CUSTOMDRAW`) is preferred: the
  style bits are unchanged, so role, state and `BM_GETCHECK`/`BM_SETCHECK`
  stay. Slice 0 verifies this with Accessibility Insights or `inspect.exe`
  on a throwaway window, for both options and for `CBS_OWNERDRAWFIXED`, and
  records the result here, replacing the theory.
- If a control must be `BS_OWNERDRAW` after all, answer `WM_GETOBJECT` for
  `OBJID_CLIENT` with an `IAccessible` wrapper that reports the right role and
  state. This is a fallback, since it is real code to get right.
- Combos keep the combo role under `CBS_OWNERDRAWFIXED`; the item text must
  still be reachable, so `CBS_HASSTRINGS` stays set.
- Colour is never the only cue: checked state is a glyph, focus is a frame,
  disabled is greyed text plus no hot state.
- Text contrast: every text pair in every theme meets 4.5:1, enforced by
  tests over the tokens (as `theme.rs` does today). The new tokens are added to
  that test.
- Narrator reads the screen through UIA, so a pass with Narrator over the
  Settings window is a named manual check for the last slice.

## `--ui-gallery --theme` for Settings

`Card` has `force_theme_for_gallery`. Settings gets the same on `GalleryWindow`:
it sets a forced theme and the Windows high-contrast black `SysColors`
(`HC_BLACK`), then repaints. The driver passes the `--theme` value to the
Settings states as it already does for the card and the region overlay. The four
existing Settings states then give 12 screenshots (light, dark, high contrast).
`HC_WHITE` is covered by tests on the tokens only, since the gallery takes one
name `high-contrast`. No registry value is ever written.

## Testing

- Pure and unit-tested: the token contrast (extended), the checkbox, radio,
  group box, combo and trackbar layout geometry as a function of size and DPI,
  the scrollbar thumb geometry and hit-testing, and the "which state is this"
  decision from `CDIS_*`/`ODS_*` flags.
- Source-scan guard, CRLF-normalised with a non-empty candidate check: the
  paint helpers contain no `rgb(`, no `COLORREF(` followed by a digit and no
  stock brush or pen other than `NULL_BRUSH`.
- Dispatch guards: the top-level proc has arms for the three theme messages,
  `WM_CTLCOLOR*`, and `WM_NOTIFY` (custom draw); the combo has `WM_DRAWITEM`.
- Real Win32 (test class names, rule 9): create the window, force each theme,
  and read back pixels from a `PrintWindow` capture for one control of each
  kind (a checkbox glyph pixel, a combo field pixel). This is the "wired to
  nothing" check for owner painting: with no handler wired, the pixel would be
  the stock light colour.
- Manual, named in each PR: screenshots in all three themes (looked at by
  eye), a real switch of Windows colour mode with Settings open, real high
  contrast (Aquatic and Desert) at 100% and 200% scale, keyboard-only
  navigation, Narrator.

## Slices (one PR each)

0. **Accessibility measurement** (no product change): a throwaway window with
   the three control approaches, inspected with `inspect.exe`; record the
   result in this spec. Decides option 1 or 2 above.
1. **Plumbing**: theme state on the Settings window, new tokens, DWM title bar,
   `WM_CTLCOLOR*` for static, edit and list box, live re-theme, gallery
   `--theme`. Checkboxes, combos and scrollbars still stock, so dark is
   knowingly incomplete at this point and is not released alone.
2. **Buttons**: checkboxes, group boxes, push buttons, focus and keyboard cues.
3. **Combo boxes** and their drop-downs.
4. **Trackbar.**
5. **Scrollbar** (replace the non-client one).
6. **High contrast pass and Narrator pass**, and the module doc's "Theme" note
   in `settings.rs` is rewritten to say what is true.

Slices 1 to 5 can merge in order behind nothing: until slice 5 lands, dark mode
is gated by a single constant so users never see a half-dark window. That
constant is deleted in slice 6.

## Open questions

- Whether the stock sunken edit border is acceptable in dark or needs the
  subclass described above. Decided by looking at slice 1's screenshots.
- Whether the combo drop-down list needs `SetWindowTheme(..., "DarkMode_Explorer")`
  for its frame and scrollbar. Decided by slice 3's screenshots. Allowed only
  if it measurably helps and the call is allowed to fail.
- Whether to ship dark as the first user-visible change at slice 5 or hold
  it until slice 6.
