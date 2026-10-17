# Per-action hotkey chords (the rest of #42)

Status: **proposed, 2026-10-17**

Scope: `actions/mod.rs` (`ActionHotkey`, its parsing and checks), `hotkey.rs`
(the low-level hook's chord matching), `hotkey_conflicts.rs` (the known-binding
table and the `RegisterHotKey` probe), `app.rs` (the dispatch from a chord to a
run), and later the Settings Actions page. Part of #42; the double-press repeat
of the last action shipped already. The expansion plan's Phase 2 list names
"per-action hotkeys" and this is that item.

## Where it is today

- `Action::hotkey: Option<ActionHotkey>` exists. `ActionHotkey { vk, ctrl,
  shift, alt, win }` deserializes from an inline table in `actions.toml`
  (`hotkey = { vk = 0x33, ctrl = true, shift = true }`, `deny_unknown_fields`).
- **It is inert.** Nothing reads `Action::hotkey`; the doc comment on the type
  says so. Every built-in action sets `hotkey: None`.
- The hook (`hotkey::hook_proc`, a `WH_KEYBOARD_LL`, installed once for the
  Copilot key) matches four things: `hotkeys.primary`, `hotkeys.secondary`,
  `hotkeys.pause` (lock-free, `PAUSE_CHORD`, works while paused) and
  `hotkeys.palette` (behind `HookShared`'s mutex, silent while paused). It
  swallows what it matches, suppresses auto-repeat with `HELD_VK`, and only
  posts a message (`WM_APP_HOTKEY`, `WM_APP_PAUSE_TOGGLE`,
  `WM_APP_PALETTE_TOGGLE`). The main thread decides.
- `hotkey_conflicts` has `known_bindings()` and `check(chord)` (system and common
  app shortcuts) used by learn mode, and `win32::probe_available`, a
  `RegisterHotKey` probe that no code calls yet.

## Problem

A person who runs one action all day (Define a word, Explain this error) has to
open Quick Ask or change the primary action. The type that would hold a chord is
already in the file format, but a chord written there does nothing, and nothing
says so.

## Goals

- `hotkey = "Ctrl+Shift+3"` in an action runs that action, exactly as if it had
  been picked from Quick Ask: every gate (paused, busy, readiness, redaction)
  still applies.
- A chord that cannot work is reported once, in a card, and the action still
  runs from everywhere else.
- Nothing runs while idle (rule 5): no timer, no new hook.

## Non-goals

- A per-action chord for the double-press. One chord, one action.
- Chords that need two steps (a leader key).
- Editing chords in Settings in this slice (see "Later").

## Design

### The file format

Accept a string, keep the table. A string is what a person types and shares; the
table form stays valid so nothing written today breaks.

```toml
[[actions]]
id = "define-word"
hotkey = "Ctrl+Shift+3"
```

`ActionHotkey` gets a custom `Deserialize` accepting both shapes. The string is
`+`-separated, case-insensitive, whitespace around `+` allowed: modifiers `Ctrl`,
`Shift`, `Alt`, `Win`, then exactly one key, last. Keys: `A`-`Z`, `0`-`9`,
`F1`-`F24`, and the names `Space`, `Enter`, `Tab`, `Esc`, `Insert`, `Delete`,
`Home`, `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`, `Pause`. Anything else is a parse error
that names the text. The parse and the print (for a card or Settings) are one
pure pair in `actions/` that round-trips; `hotkey::Chord` is not reused, as the
type's own comment decides, and a `From` converts at the one binding site.

### Validation

At actions load, per action, pure and unit-tested, each failure is a message on
that action, never a load failure of the whole file:

| Rule | Result |
|---|---|
| No modifier, and key is not `F13`-`F24` | refused. A bare letter would eat typing everywhere. |
| Only modifiers, or two keys | parse error |
| Equals `hotkeys.primary`, `secondary`, `pause` or `palette` | refused, naming the slot |
| Equals another action's chord | the first in load order keeps it, the later one is refused, naming both |
| `hotkey_conflicts::check` says system-owned (Task Manager, lock, Snipping Tool) | refused |
| `check` says common app (a browser accelerator) | allowed, noted on the action |
| Win key with a letter or digit | allowed, but only the hook can see it, see below |

Refused chords are dropped from the binding table only. The action stays usable
from the palette and the tray, and one card at load lists every refusal
(rule 7), with the file name and the action id.

### Binding: the existing hook, not `RegisterHotKey`

`RegisterHotKey` reports a clash with another app's registration, which is its
one real advantage, and costs nothing idle. But it cannot see Win-key chords the
shell owns, it does not swallow the key the way the rest of Wingman's chords do,
it needs a second matching path, and it has no notion of paused or learn mode.
The hook already runs, already suppresses auto-repeat and already posts instead
of deciding. So per-action chords join it.

- A fixed-size table of packed chords (`pack_chord`, one `AtomicU64` each, a cap
  of 32) beside `PAUSE_CHORD`. Lock-free because the hook callback must stay
  cheap, and it is read only on a non-modifier keydown with a modifier held, the
  same early return the other chords use. A key with no match costs a short scan.
- `set_action_chords(&[(slot, Chord)])`, called from `apply_config`/actions
  reload the way `set_bindings` and `set_pause_chord` are, on the main thread.
- A match posts `WM_APP_ACTION_HOTKEY` with the slot index in `lparam`. The main
  thread maps slot to action id (the table it set) and runs the same path
  Quick Ask uses (the pending action id, then `start` with every gate). The hook
  never reads the action list.
- Silent while paused, as the palette is: Pause means Wingman does nothing from
  a key. (The pause chord alone works while paused.)
- Learn mode takes precedence over matching, as it does for the other slots.

### Reporting a clash with another app

The hook swallows a matching key before another app's `RegisterHotKey` handler
would see it only if Wingman's hook runs first; the order is not guaranteed.
So at actions load, on the main thread, call `win32::probe_available` once per
chord (register, unregister, immediately). A `false` is a note on the card,
"Ctrl+Shift+3 is already taken by another app, so it may not reach Wingman".
This is the probe's first caller. It is a one-shot at load and on reload, never
periodic. If the probe cannot run (no window handle yet), say nothing: the table
check above is the primary mechanism, as `hotkey_conflicts` already states.

### Later

The Settings Actions page shows each action's chord read-only, with the refusal
reason when there is one. Editing through learn mode needs one more learn slot
per action and writing back through `config_writer`'s actions equivalent; that is
a separate slice once the display shows what is bound.

## Tests

Pure: parse and print round-trip for every key class and for each modifier
combination; every error case above; the table form still loads; the validation
table row by row against fake primary, secondary, pause, palette and a second
action; duplicate order is load order. In `hotkey.rs`: a packed-table match,
a near miss (extra modifier) does not match, auto-repeat posts once, a match is
silent while paused, a bare-modifier keydown never scans. Scanning tests
(CRLF-normalised, non-empty candidate set): every `Action` constructor that
sets `hotkey` is covered by the validation; `WM_APP_ACTION_HOTKEY` has a
`wnd_proc` arm and is in the deferred-message lists beside `WM_APP_HOTKEY`.

Manual check (the named observable): bind `Ctrl+Shift+3` to Define a word,
select a word, press it. Expected: the action runs. With the table wired to
nothing the key would pass through to the focused app. Also bind a second action
to `Ctrl+Shift+Esc` and confirm the load card refuses it.

## Open questions

1. **String or table form?** Recommended: accept both, print the string.
2. **Refuse or allow the "common app" conflicts?** Recommended: allow with a
   note, refuse only the system-owned ones, since the person may not use that app.
3. **Should a per-action chord fire while paused?** Recommended: no, same as
   Quick Ask.
4. **Cap on bound chords?** Recommended: 32, refused beyond with a card line;
   far above any real use, small enough to keep the hook scan trivial.
5. **Allow bare `F13`-`F24`?** Recommended: yes, those keys exist for this and
   collide with nothing.
