# Quick Ask palette (#25)

Status: **approved** (owner asleep; written against issue #25, #199's
grouping clause, and #132's no-model-tier clause, all already scoped by the
owner in the issue bodies and the 2026-09-16 expansion plan §6/§7. Treated as
pre-approved per the overnight-agent instructions; the owner reviews on
waking.)

Scope: a new `ui/palette.rs` (Win32 window) and `ui/palette_model.rs` (pure
model), a `hotkeys.palette` config field, and additive wiring in
`app.rs`/`hotkey.rs`/`ui/tray.rs`. Out of scope: DirectWrite rendering (filed
as a follow-up, see below), context chips (#63), the intent router, the
settings Actions page.

## Why a design doc (rule 12)

This adds a new UI surface with its own window class, message flow and key
handling, which is architectural per CLAUDE.md rule 12. Short because the
shape is already pinned by the expansion plan §7 and issue #25's Done-when;
this fills in the concrete split between pure and Win32 code, the message
names, and the dispatch mechanism.

## Split (rule 8: pure vs. Win32)

- `ui/palette_model.rs`: fuzzy scoring, grouping/ranking (#199, #132),
  the key-handling state machine, and the action-id dispatch table. No
  `windows` crate dependency at all -- every function here is unit-tested
  without a live window.
- `ui/palette.rs`: the real `HWND`, a child `EDIT` control for the query
  text, GDI painting of the row list, and the glue that turns Win32 messages
  into calls into `palette_model`.

## Window lifecycle

Pre-created hidden at startup (`App::run`, mirroring `Card::new` and
`card.set_owner`), `WS_POPUP | WS_EX_TOOLWINDOW | WS_EX_TOPMOST`, never
destroyed until process exit. `Palette::show` does no gathering work itself:
the caller (`App::toggle_palette`) builds the catalogue (cheap: iterating
`actions::load_actions()` plus two fixed utility entries, no I/O beyond the
`actions.toml` read `load_actions` already does today for `ask()`) and hands
it to `Palette::show`, which centers the window on the active monitor
(`capture::active_monitor_rect`, made `pub(crate)`), resets the query, and
calls `ShowWindow(SW_SHOW)` plus focuses the query `EDIT` control. Hiding
(`Esc`, losing focus, or a dispatched action) is `ShowWindow(SW_HIDE)`, never
`DestroyWindow` -- rule 5: zero work while hidden, and showing again is just
a repaint, not a window creation.

## Text input and key handling

A child `WC_EDIT` control (same pattern `ui/card.rs`'s preview state already
uses for its editable fields) holds the query text. `EN_CHANGE` (delivered as
`WM_COMMAND` to the palette's own `HWND`, since the edit control is its
child) re-runs `palette_model::build_rows` and invalidates the list area.

Up/Down/Enter/Esc must work while the edit control has focus, which plain
`WM_KEYDOWN` does not reach the parent for (Win32 delivers it to whichever
window has focus). This crate already solved exactly this problem for the
confirm card's preview state (`ui/card.rs`'s `preview_control_subclass` via
`SetWindowSubclass`/`DefSubclassProc`): the palette's query edit control is
subclassed the same way, mapping a matched key to a small set of `WM_COMMAND`
ids that the palette's own wndproc treats identically to a real command.

## Dispatch: the same entry point the tray item uses

`palette_model::dispatch_target_for(action_id) -> Option<DispatchTarget>` is
a pure lookup (`DispatchTarget` is a closed enum: `CheckMyWork`,
`ExtractText`, `AddToCalendar`, `CalculateSelection`, `CopyRegion`), unit
tested to cover every built-in action id plus the two utility ids
(`"calculate-selection"`, `"copy-region"`, new consts in `palette_model.rs`
since these two are tray-only utilities, not part of the `Action` model). On
Enter, the palette posts `WM_APP_PALETTE_RUN` (`WM_APP + 11`) to the owner
window, carrying the selected action id (`Box<String>`, same boxed-payload
idiom `WM_APP_LEARNED` uses), and hides itself. `App`'s `wnd_proc` maps the
id through `dispatch_target_for` and calls the exact same method its tray
item already calls (`App::ask`, `App::extract_text`,
`App::add_event_from_screen`, `App::calculate_selection`,
`App::copy_region`) -- never a second, palette-only code path. A `None` from
`dispatch_target_for` (should never happen for a row the palette itself
built) is a no-op, not a panic (rule 7).

## Grouping and ranking (#199, #132)

Empty query: grouped view. If no provider is configured
(`Chain::ready_provider_names().is_empty()`), the model-free actions
(`extract-text-to-clipboard`, `calculate-selection`, `copy-region`) are
listed first, flat, followed by a one-line hint row ("Add a model in
Settings to unlock more actions."), then the remaining actions grouped by
`Action::group` (headers, in first-seen order). If a provider is configured,
there is no hint row and no free/gated split: every action is grouped by
`group` in first-seen order, ungrouped actions listed first with no header.

Non-empty query: flat, ranked list, no headers, no hint row. Fuzzy match is
a subsequence match (case-insensitive) with a prefix bonus (whole match
starts at position 0) and a per-character word-start bonus (matched
character is the first of a word), ties broken alphabetically.

## Rendering: GDI, then DirectWrite/Direct2D (#216)

Issue #25's body mentions DirectWrite. The first implementation used GDI
(`TextOutW`/`DrawTextW`, the same primitives `ui/card.rs` and `ui/region.rs`
already use), consistent with rule 12's "match the shape already pinned" and
with keeping a pre-created window's first paint on the sub-100ms path with no
new dependency. A DirectWrite rendering pass was filed as a follow-up issue
rather than built there (see the closing comment on #25).

**Superseded 2026-09-18 (issue #216):** the row list, router summary line and
footer now render through `ID2D1HwndRenderTarget` + a cached
`IDWriteTextFormat` (`ui/palette.rs`'s `PaletteRenderer`), created once at
window-creation time and never per paint -- see that module's doc comment
for the full design, the MEASURED show-latency numbers, and why
`ID2D1RenderTarget::DrawText` is used instead of building a per-row
`IDWriteTextLayout` (the `DrawTextLayout` render call needs
`windows_numerics::Vector2`, a type the `windows` crate does not re-export,
which would need a new direct Cargo dependency outside this task's
`Cargo.toml` scope). The original GDI path is kept, byte-for-byte, as a
fallback for a window whose Direct2D factory creation failed, or that hits
`D2DERR_RECREATE_TARGET` (device loss) -- rule 7: every failure ends in a
card, never a blank palette. Per-monitor-v2 DPI now updates the render
target's DPI on `WM_DPICHANGED` rather than reading it once (a gap the GDI
path also had, closed as part of this work since the render target's own
DPI tracking made it unavoidable to look at).

## Footer

`mode: <Mode::label()> - <first configured provider>:<its model>` (or just
the mode label when nothing is configured). Built in `app.rs` from
`Config`/`Chain` (not pure -- it reads live config), formatted by a pure
`palette_model::footer_line` helper.

## What stays out of scope

- Context chips (awareness, Phase 4): #63 already tracks this.
- DirectWrite: follow-up issue.
- Per-action hotkeys (`Action::hotkey`): still inert, unrelated to this
  issue.
- The Actions settings page (#48): `disabled_groups`/`enabled` are still
  hand-edited `actions.toml`; the palette only reads what `actions::load_actions`
  already resolves.
