---
name: native-ui
description: Use before changing, auditing or reviewing anything a user sees in Wingman (card, palette, preview, confirm, region overlay, Settings, tray menu, pending status). Covers the Windows 11 look Wingman aims for, the shared tokens in src/ui/theme.rs, how to render every surface at every theme and scale with the UI gallery and actually look at the pictures, and the GDI/DPI/high-contrast traps that have shipped before. Also use when asked to make the app look modern or native.
---

# Native UI for Wingman

Wingman is a Win32/GDI app with no web view. "Modern" here means **looks
like it shipped with Windows 11**, not like a web page. The user should not
be able to tell the card from a system flyout at a glance. Distinctiveness
comes from the content (one verdict, one action, plain words), never from
decoration.

## The target: Windows 11 fidelity

| Aspect | Target | Where it lives |
|---|---|---|
| Type | Segoe UI Variable (fallback Segoe UI). Body 14 px, caption 12 px, subtitle 20 px semibold, all at 96 DPI and scaled. Sentence case, no ALL CAPS labels | `theme.rs` type tokens |
| Corners | 8 px top-level windows via `DWMWA_WINDOW_CORNER_PREFERENCE` (`DWMWCP_ROUND`), 4 px controls | `card.rs` shows the call |
| Spacing | 4 px grid: 8/12/16/24. Content padding 16 px on cards and flyouts | `theme.rs` spacing |
| Colour | Neutral surfaces, the system accent only for the primary action and focus. Light, dark and high contrast all from `theme.rs`; high contrast uses `GetSysColor`, never a hard-coded colour | `theme.rs` |
| Backdrop | Mica or Acrylic via `DWMWA_SYSTEMBACKDROP_TYPE` where the surface is a real top-level window and the OS supports it; solid token colour otherwise. Never fake it with a bitmap | per surface |
| Title bar | `DWMWA_USE_IMMERSIVE_DARK_MODE` in dark | Settings, preview |
| Focus | Visible 2 px focus ring on every keyboard-reachable thing; Tab order follows reading order; Esc dismisses | every surface |
| Motion | None by default. Only to answer an action, and off when `SPI_GETCLIENTAREAANIMATION` is false | |
| Icons | Segoe Fluent Icons glyphs (fallback Segoe MDL2 Assets), never emoji | |
| Words | Plain verbs, the button says what happens ("Copy table", not "OK"). Errors say what went wrong and what to do. No em dashes in user-facing text (AGENTS.md rule 11) | |

Check a choice against the real thing: open a Windows 11 flyout (Quick
Settings, the volume flyout, Settings app) side by side with the gallery
picture. If Wingman's surface looks heavier, busier or more colourful, it
is wrong.

## Rules of the code

1. **Every colour, font size, radius and gap comes from `src/ui/theme.rs`.**
   A literal `rgb(...)` or pixel constant in a surface file is a finding.
   Add a token if one is missing; do not add a local constant.
2. **Every size is scaled by the window's DPI** (`GetDpiForWindow`,
   `MulDiv(px, dpi, 96)`); re-layout on `WM_DPICHANGED` using the suggested
   rect. A layout that is right at 100 % and clipped at 150 % is the most
   common UI bug this repo has shipped (#742, #743, #744).
3. **Measure text, never guess its width** (`GetTextExtentPoint32W` /
   `DrawTextW` with `DT_CALCRECT`) in the font actually selected into the DC.
4. **High contrast is its own theme**, not dark mode: system colours only,
   no custom backgrounds, borders visible.
5. **Never block the UI thread.** It hosts the low-level keyboard hook; a
   slow paint or a file/network call there freezes the Copilot key. No
   nested modal loops while `&mut App` is borrowed.
6. **Double-buffer paints** (`BeginBufferedPaint` or a memory DC) so resizing
   and theme switches do not flicker. Free every GDI object you create
   (`DeleteObject`), restore what you selected.
7. **Accessibility is part of done**: controls keep their UIA roles (a
   `BS_OWNERDRAW` button loses its role; custom-draw keeps it), names are
   unique and meaningful, Narrator reads the card's verdict.

## Look at it: the visual check (mandatory)

A UI change is not done until you have looked at the pictures. Tests prove
the layout maths; only the picture shows that it looks right.

```powershell
# in your worktree, with the cargo env from BRIEF.md set
cargo build
.\<target-dir>\debug\wingman.exe --ui-gallery --matrix --screenshot <scratch>\after
```

Then **Read the PNGs** (the Read tool shows images). For each state you
touched, check at 100 % and 150 % in light, dark and high contrast:
clipping, overlap, text touching an edge, misaligned baselines, uneven
gaps, wrong colour in one theme, a focus ring missing, ellipsis where the
text would fit. Render `before` from `origin/master` the same way when the
change is visual and compare file by file. Name the states you looked at
in the PR ("Manual check:" line) and attach before/after for the main one.

The tray menu is in the gallery as `tray-menu` and `tray-menu-submenu`: the
real menu, built on a tray with no shell icon and captured from the screen,
so another window in front of it spoils the picture (look at it). Use
`--only tray-menu` to render one surface. The palette is drawn at every theme
and scale twice: `palette-*--<theme>--<percent>.png` is the GDI paint (`WM_PRINT`)
and `...--d2d.png` is the real Direct2D code painted into a bitmap. A screen grab
and `PrintWindow` cannot read the palette's own Direct2D surface (MEASURED
2026-10-17: a monitor `BitBlt` shows the windows underneath, `PrintWindow` with
`PW_RENDERFULLCONTENT` is black apart from the query box), so never judge the
palette from either. Surfaces the gallery cannot show yet (the real backdrop)
need a screenshot of the running app: say so in the PR rather than claiming
them checked.

## Auditing a surface

Read-only. Render the matrix, read every picture of the surface, then read
its code. Report findings as issues (see `filing-findings`), one per
distinct problem, each with: the state and scale/theme where it shows, what
is wrong, what Windows 11 does instead, and the file. Rank by what a user
notices first: clipped or unreadable text > wrong theme colour > broken
keyboard path > spacing and alignment > polish. Put a gallery PNG name in
every finding so it can be checked again after the fix.

## Traps that already cost a session

- The palette's Direct2D surface is invisible to `PrintWindow` and to a screen
  capture (#626, #987). The gallery paints it through a DC render target
  instead. A black or see-through palette picture is a capture problem until
  the `--d2d` picture says otherwise.
- Settings runs comctl32 v5 today; custom-drawn checkboxes need the v6
  manifest (#662 spec). Group boxes need a `WM_PAINT` subclass for dark.
- `uxtheme` ordinal APIs (undocumented dark mode) are ruled out; use
  owner/custom draw and DWM attributes.
- A theme change arrives as `WM_SETTINGCHANGE` with `ImmersiveColorSet`; a
  surface that only reads the theme at creation looks wrong until restart.
- The Settings gallery is real at 100% only (#1016, #1052): that scale is a DPI-unaware window, so its checkbox boxes and scrollbar are the real 96 DPI ones. At 150%, 200% and 250% the layout follows `--dpi` but native checkbox glyphs and the scrollbar keep the screen's own DPI, so a checkbox box looks too big there. Judge checkbox and scrollbar detail at 100% or on a screen at that scale.
