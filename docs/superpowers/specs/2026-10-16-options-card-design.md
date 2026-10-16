# The options card and the Wingman key

Status: **accepted, 2026-10-16** (AGENTS.md rule 12). Covers #305 (epic),
#306, #307, #308, #324, #393, and, as later slices, #42 and #107. Supplements
the [expansion plan](2026-09-16-expansion-plan-design.md) §6 "The intent
router" and the [palette spec](2026-09-17-palette-design.md). Where they
disagree on what a bare key press does, this spec wins.

## The problem

A bare Wingman (Copilot) key press runs one action, Check my work, a physics
checker. On most screens that does nothing useful. The pieces for something
better exist but sit behind the Quick Ask palette: the intent router (#24)
guesses one action, and the palette is a full list with a search box.

The goal is the loop from the product vision, with the key as the whole
interface: **press, a small card offers 2 to 4 options for what is on screen,
one press or click picks one**. No chat, no typing, no follow-up. Anything
that changes something still goes through the preview card and waits for
Enter. Wingman never presses Send, Submit, Buy or Pay, whatever the key does.

## Where it is today

- `hotkey.rs`: a low-level keyboard hook. A matching keydown posts
  `WM_APP_HOTKEY` to the main window, swallows the key and, for a chord
  containing Win, injects a Ctrl tap (`send_ctrl_tap`) so the Start menu does
  not open. The hook ignores key-up events entirely, and posts once per
  keydown event, auto-repeat included (THEORY, unverified: holding the key
  repeats the message; no code dedupes it today, the busy flag hides it).
- `app.rs`: `WM_APP_HOTKEY` runs `cancel_pending_send_preview()`, else
  `App::ask(Trigger::Hotkey)` (Check my work). `WM_APP_ACTIVATE` (a duplicate
  launch of the exe, which is also what the Copilot-key picker and the Start
  menu do) is rate limited and then also calls `ask`. A first launch with no
  flag does nothing (`single_instance::first_launch_action`).
- `router.rs`: pure. One call on a 512 px screenshot (`ROUTER_IMAGE_LONG_EDGE`),
  `max_tokens` 200, schema `{summary, intent, confidence}`, run on the
  cheapest ready provider (`cheapest_router_target`). Consumed only by the
  palette, which pre-selects one row (`should_apply`).
- `app.rs` `dispatch_palette_action` / `dispatch_palette_target`: the one
  dispatch from an action id to a flow. The options card reuses it.
- `ui/card.rs`: states `Hidden, Pending, Collapsed, Expanded, Preview`.
  Pending and Collapsed are `WS_EX_NOACTIVATE`. Answer cards dismiss on a
  click elsewhere through the `dismiss.rs` `WH_MOUSE_LL` watcher (armed only
  while an answer is showing). A preview is cancelled when focus leaves it
  (`preview_focus_left_the_card`). No Cancel on a card where nothing was done
  (#392, shipped).
- `config.rs`: `Hotkeys { primary, secondary, pause, palette }`,
  `Palette { router_threshold }` (0.7).

- `router_worker` builds its chain with `without_prompt()`: the router never
  shows the #105 send preview. With "confirm before sending" on, a cloud
  router call is refused by the send guard and the press gets no suggestion.
  `maybe_start_router` also returns silently when paused or when a password
  manager is in front.
- Every action captures again on the main thread after hiding the card (hide,
  settle, capture), so no capture is shared between the router and an action.
- `password_manager_hint` is fed from `palette_password_manager`, which is
  written only when the palette opens. The options flow has to set its own.
- Answer cards already auto-dismiss (`show_answer(.., auto_dismiss_secs, ..)`).
  The options card does not exist yet.
- Exe size: `perf-budgets.json` records 3,982,336 bytes measured against a gate
  of 4,500,000 after the later raises, about 518 KB of headroom (AGENTS.md's
  "about 3.5 MB" is stale).
  The slices below, and the Settings dark mode, overlay and knowledge specs,
  all draw on that one headroom. See "Size budget".

## Decisions

These are the product rules this design is built on.

- **One press, one action, one card.** No chat. The options card is a menu,
  not a conversation.
- **Look, Propose, Confirm, Do is unchanged.** The options card is a new
  "Look" front end: it chooses which action runs. It never runs a write
  itself. Every option that writes goes through the existing preview card.
- **No Cancel button on any card where nothing has been done.** Esc, the
  key, or a click elsewhere dismisses. This covers the options card.
- **Dynamic is the default key mode** (#393), with Always pick and Always
  close as Settings choices.
- **A bare launch never makes a billed call.** See "Bare launch".

## Shared rules for every surface (this spec and the overlay spec)

The options card and the overlay spec both put things on screen that the key,
Esc and a click can act on. One rule set, defined here. The overlay spec points
at this section instead of restating it.

- **Surfaces, highest first:** the Preview card (a write waiting for Enter),
  the Options card, then Answer, error and info cards. The overlay (marks,
  labels, ghost text) is not a surface. It belongs to exactly one card (its
  summary card, or the preview it annotates) and lives as long as that card.
- **A key press, Esc or a click-away acts on the highest surface only, and
  whatever it dismisses takes its overlay with it.** With an overlay and a
  preview card up together (#329), the key dismisses the preview, nothing
  runs, and the ghost text goes with it.
- **`Context` carries `overlay: bool`, not a new `CardKind`.** An overlay
  changes what a dismissal removes, never what the key means.
- **Click-away: the overlay always hides, the card follows the setting.** A click
  that is not on a label changes what is on screen, so a stale overlay would
  lie and it always hides. The card follows `options.click_away_dismiss`. If the
  card stays (setting off), its line is rewritten to say the marks are hidden,
  so it never claims marks that are gone. A click that lands on a clickable
  overlay label (#326) is a pick, not a dismissal: the handler hit-tests the
  label rectangles before it applies the click-away rule.
- **Esc claim for an overlay.** The summary card never takes focus, so while an
  overlay shows the hook claims Esc only (no digits), under the same rules as
  the options card's claim: no modifiers, an expiry tick (the overlay's age cap
  plus 1 s) that lapses by itself, cleared on every hide path.
- **Alt+Tab or any foreground change:** a preview card survives with its edits.
  The overlay hides (the marks no longer line up with the screen) and is not
  restored when the user comes back. The preview card stays the source of
  truth.
- **Screen readers:** a card or summary card appearing raises a system alert
  event named by its summary. Full UIA support is #119.

## User-visible behaviour

### States

| State | What the user sees |
|---|---|
| Idle | Nothing. |
| Looking, router not started | The options shell at once: a summary placeholder ("Looking at your screen") and no buttons. The screen was captured at the press. The double-press window is open (when `double_press_ms` is above 0). |
| Looking, router started | The same shell. The router request is in flight and can be cancelled. |
| Options ready | Summary line, then 2 to 4 numbered buttons. |
| Running, cancellable | The existing Pending card for an action's Look or Propose model call. Nothing has been written. |
| Running, committed | The Pending card while an executor writes after Enter (fill form, calendar add, replace text), and the short clipboard write of a model-free action. It cannot be cancelled. |
| Result | The existing answer, error, info or preview card. |

### What a press does

This is the #393 table, made exact. "Option" means a button on the options
card. "Double" means two presses within the double-press window (below).

| State when the key is pressed | Single press | Double press |
|---|---|---|
| No card, nothing to repeat | Look: capture, show the shell, the router starts when the window closes | Same as single. The second tap is part of the same gesture and cancels nothing. |
| No card, a last action exists | Look, as above | Repeat the last action (the shell hides, the action captures again as every action does) |
| Looking, router not started (window open) | The second press of a double. With a last action: repeat it, the router never starts, the early capture is dropped. Without one: absorbed, the Look carries on. | n/a (a third press is ignored) |
| Looking, router started | Cancel, no card | Cancel, no card |
| Options card, option 1 is key-pickable | Run option 1 | Run option 2 if it exists and is key-pickable, otherwise nothing |
| Options card, option 1 is not key-pickable | Dismiss | Run option 2 if key-pickable, otherwise Dismiss |
| Running, cancellable | Cancel, no card (the generation pattern `preview.rs` uses drops the late result) | Same |
| Running, committed | Ignored (swallowed, nothing shown). The result card always appears. | Ignored |
| Answer, error or info card | Dismiss, and its overlay | Dismiss |
| Preview card (Do it, Edit; Book anyway; a send preview) | Dismiss, nothing runs (a send preview is cancelled, as today) | Dismiss, nothing runs |

A cancel is not an error: no card, no message. The committed state exists
because the generation pattern can drop a result card but cannot stop a write
that already started. Cancelling there would leave the user with a write and no
card telling them it happened (rule 7).

**Decision: a preview card does not count as "has options" for the key.**
Its option 1 is Do it, which writes. Pressing the key to get rid of a card
out of habit must never act. Enter (or the button) confirms, as today.
"Book anyway" (#390) is never reachable from the key. This holds in every key
mode, including Always pick.

**Decision: an option is key-pickable only if running it cannot change
anything without a further Enter and cannot overwrite the clipboard.**
Read-only actions (explain, review) and actions that end in a preview qualify.
Model-free actions that write the clipboard (Copy text from screen, Copy
region, Read QR) do not: they are picked by click or by digit, or by the key
when their confidence is at or above `palette.router_threshold`. The pure
function `key_pickable(effect, confidence)` decides, and the catalogue table of
built-ins gains an `effect` column (`none`, `clipboard`, `preview`; a
user-authored action with a write executor is `preview`, otherwise `none`).
When option 1 fails the test the key dismisses the card, the safe outcome.

### Key modes (Settings)

One Settings choice, "When a card is showing, the Wingman key...":

1. **Picks an option if there is one, otherwise closes the card** (Dynamic,
   default): the table above.
2. **Always picks an option**: on an options card as Dynamic. On any other
   card it does nothing beyond what Esc does not already do, so it is a no-op
   (Esc still closes). It never confirms a preview.
3. **Always closes the card**: any press on any card dismisses it, nothing
   runs. Options are picked with the mouse or keys 1 to 4. This is #324's
   toggle.

The mode changes only the "card showing" rows. "No card" and "in flight" are
the same in all three.

### Clicking elsewhere

#324 wanted a click elsewhere not to dismiss; the no-Cancel rule (#392) wants
it to. Resolution:

- **Decision: a click elsewhere dismisses the options card, answer, error and
  info cards (and their overlay), and passes through to the app underneath.**
  The click is never swallowed, so the user still lands in the app they
  clicked. The `dismiss.rs` watcher stays, and is armed while the options card
  shows.
- **Decision: a Do it / Edit preview survives focus loss and click-away.** It
  can hold edits, and an Alt+Tab to check a date must not discard them (#324's
  real complaint). It closes on Esc or the key. The silent cancel in
  `preview_focus_left_the_card` is removed for this kind of preview, in the
  slice that changes the key table.
- **Decision: the send preview (#105) keeps its focus-loss cancel.** Its worker
  is blocked and `busy` is set, and there is nothing in it to lose, so a
  forgotten one would hold the app busy and the tray looking busy. The cancel
  stays quiet (no card, #392). Resolved question 9.
- A setting `options.click_away_dismiss` (default `true`) lets a user who
  wants #324's persistent card turn the dismissal off for answer and options
  cards. Esc, the key and the options card's auto-dismiss still close.
- The card never takes focus when shown (`WS_EX_NOACTIVATE`), so typing keeps
  going to the app underneath. Keys 1 to 4 and Esc therefore need a way to
  reach the card; see "Keys 1 to 4 and Esc".

## The router: a ranked shortlist (#306)

### Shape

`router.rs` stays pure. `RouterResult` becomes:

```rust
pub struct RouterOption { pub intent: String, pub label: String, pub confidence: f64 }
pub struct RouterResult {
    pub summary: String,
    pub options: Vec<RouterOption>, // ranked, len 0..=4, all ids real
}
```

The old single `intent` and `confidence` are gone. The palette's consumer
(`palette_model`, `should_apply`) reads `options.first()`. Separately, see
"Related decisions": the palette no longer sends a router request when it
opens.

### JSON schema

Property order is load-bearing (AGENTS.md rule 3): the model must describe
what it sees before it commits to anything, and must name the action before
it writes a label for it.

```json
{
  "type": "object",
  "properties": {
    "summary": { "type": "string" },
    "options": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "intent":     { "type": "string", "enum": ["<candidate ids>"] },
          "label":      { "type": "string" },
          "confidence": { "type": "number" }
        },
        "required": ["intent", "label", "confidence"],
        "additionalProperties": false
      }
    }
  },
  "required": ["summary", "options"],
  "additionalProperties": false
}
```

- `summary` before `options`; `intent` before `label` before `confidence`.
  `serde_json` keeps `preserve_order`; a unit test asserts the key order of
  the built schema so a refactor cannot reorder it silently.
- The `enum` is built from the candidate ids (no `"none"` entry: an empty
  `options` array is how the model says nothing fits).
- The schema deliberately has no `minItems` or `maxItems`: providers differ
  on which array keywords structured output accepts, and a prompt-only limit
  is not trusted (see #301). The cap is enforced in code.
- `max_tokens` rises from 200 to 400 (four options at about 35 tokens each,
  plus the summary).
- The prompt asks for up to four options, best first, labels that name the
  thing on screen ("Review this email to Dana"), plain text.

### Validation (pure, `parse_router_result`)

Applied in this order, each step a named unit test:

1. Drop any option whose `intent` is not in the candidate list. Never trust
   the model to invent an action.
2. Drop duplicates by `intent`, keeping the higher confidence.
3. Clamp each confidence to `0.0..=1.0`.
4. Drop options below `options.min_confidence`. The palette keeps
   `palette.router_threshold` (0.7) for its own pre-select; the card needs a
   lower bar because it exists to show alternatives (#306's own example lists
   0.41 and 0.12). Default 0.2.
5. Sort by confidence, descending. Ties keep catalogue order (stable sort).
6. Keep at most `options.max_options` (default 4, clamped 2 to 4).
7. Sanitise every label: strip markdown characters, replace an em dash with
   a colon and a space (rule 11), collapse whitespace, then clamp to 40
   characters **on a char boundary** (never a byte slice, since the text
   came from a model). An empty label falls back to the action's catalogue
   name.

Lenient input: a reply in the old `{summary, intent, confidence}` shape
(small local models drift, see #223) is read as a one-option list whose
label is the action name. Unparseable JSON is an error to the caller, as
today.

### The floor: always a useful card

After validation, a pure `with_fallbacks(options, catalogue, model_ready)`
guarantees the card never ends up empty or a single dead end:

- If fewer than 2 options survive, append, in order and skipping any
  already present: **Explain what's on screen** (#131, needs a model) when a
  model is ready, then **Copy text from screen** (#41, model-free).
- With no model ready (no key, or Offline with no local model), the router
  does not run at all. The card is the static list: the catalogue's
  model-free actions in a fixed order (#132), capped at 4. Zero network.

The fallback labels are the catalogue names, not model text.

### Failure

The router is advisory, but its failure is the user's whole card. Rule 7
applies: a router error, timeout, refusal or unparseable reply does not
show an error card. It shows the options card with the floor above and a
one-line summary, "Could not read the screen, here are a few options". The
failure reason goes to the diagnostics log, not the card. The router call
has its own timeout (default 10 s; THEORY, unverified, needs a latency
measurement on the cheapest target before it is fixed).

The router call is an egress path, so its gates are stated, in the order they
are checked at the press, before the shell shows:

1. Paused: the key passes through (unchanged).
2. A password manager, secure desktop or payment page in front: the existing
   capture-gate card is shown instead of the options card, and no router call
   is made. The gate runs at the press (a foreground check, no capture) and
   the result is kept for the whole Look, so the action chosen later honours it
   as `palette_password_manager` does for the palette.
3. Offline: the router runs on the local model or not at all.
4. Cloud with "confirm before sending" on: the router never shows the send
   preview (it runs by itself, with no preview), so the send guard
   refuses it. The card shows the floor and no request leaves the machine. A
   user who wants cloud routing with that toggle on gets the floor; the action
   they then pick shows its own send preview as always. This keeps one press
   from opening two previews.
5. Otherwise the router runs on the cheapest ready provider through the
   existing redaction and payment-denylist gates.

The floor's "Copy text from screen" refuses itself over a password manager, so
it is never a way around gate 2.

## The options card (#307)

### Layout

A small card in the configured corner (`CardCorner`, same placement code as
the other cards), same theme and per-monitor DPI handling as `Card`.

```
+------------------------------------------+
| Compose window to Dana, "Q3 numbers"     |
|                                          |
|  1  Review this email to Dana            |
|  2  Make it sound friendlier             |
|  3  Translate to Spanish                 |
+------------------------------------------+
```

- A summary line (clamped like labels), then one row per option. The number
  is shown as a keycap on the left. Row 1 is drawn as the default (accent
  outline), because a single key press runs it.
- Rows are owner-drawn buttons through `card_buttons.rs`, the way Do it and
  Edit already are. Hover and pressed states follow the existing buttons.
- No Cancel button, no close button. A footer hint line in the muted style,
  "Esc to close". With the key as a pick, the hint also names it:
  "Wingman key: 1, twice: 2" (shown only in Dynamic and Always pick).
- Height grows with the option count and the text scale setting
  (`set_text_scale`). Width is fixed like the other cards.
- It appears at once with a placeholder (no blank wait), then the buttons
  drop in when the router replies. The card re-measures once, not per row.

### Implementation shape

A new state `CardState::Options` on the existing `Card`, not a second window
class: it needs the same corner placement, theme, DPI and dismiss plumbing.
`show_options(summary, &[OptionRow])` and `set_options(summary, &[OptionRow])`
(the pending-to-ready transition), where `OptionRow { id: String, label:
String }`. Clicking row `n` posts a message carrying the action id (not the
index), so a stale click after the card changed cannot run the wrong thing:
the handler checks the options generation first.

Choosing an option **hides the options card before the action's capture**,
exactly as the palette hides itself first, so the card is never in the
screenshot or the OCR input. It then calls `dispatch_palette_action` (the
one dispatch; no second one).

### Keys 1 to 4 and Esc

The card does not take focus, so a window message to it would never arrive.
Two choices were weighed:

- **Decision: handle 1 to 4 and Esc in the existing low-level hook**, but
  only while the options card is showing. A matching key is swallowed and
  posted as a message. Any other key passes through untouched. This keeps
  focus in the user's app and works without a click.
- The cost is that digits cannot be typed into the app underneath while the
  options card is open. The claim is therefore narrow and bounded:
  1. **No modifiers.** A digit or Esc is claimed only with Shift, Ctrl, Alt
     and both Win keys up, so Ctrl+1 (browser tab), Win+1 (taskbar) and
     Shift+1 pass through. The hook already tracks modifier state to match
     chords; the claim reuses it.
  2. **Only digits that name an option.** With three options, 1 to 3 are
     claimed and 4 passes through. Main-row and numpad digits both count.
  3. **Bounded in time.** The options card auto-dismisses after
     `options.auto_dismiss_secs` (default 20; one `SetTimer` armed when the
     card shows and killed when it hides, the same one-shot pattern the answer
     cards use, so nothing runs while idle). The claim is a lock-free atomic
     holding an expiry tick (auto-dismiss plus 1 s). The hook compares the
     key event's own timestamp with it, so a flag left set by a bug lapses by
     itself, with no timer.
  4. **Cleared on every path.** Hide, pick, cancel, dismiss, card window
     destroy and `App` drop all clear it. A unit test over the pure "which
     keys are claimed in which state" function proves nothing is claimed in
     any other state, with a modifier down, or past the expiry.
  5. **Switchable.** `options.claim_keys = false` turns the claim off for a
     user who types digits while the card is up or who runs an IME whose
     candidate selection uses digits. The options are then picked by click or
     by the Wingman key.
  6. **Visible.** The hint line says "1 to 3 or Esc". A digit that is not
     claimed reaches the app underneath, so a dropped digit cannot happen
     silently; the one that is claimed visibly runs an option.
- IME and password fields: the hook cannot see whether the app underneath is
  mid-composition or has a password box focused. THEORY (unverified): a claim
  of at most 20 s that needs a deliberate card on screen is acceptable. Manual
  check 8 covers an IME and a numpad.

Esc closes the card with nothing run.

### Accessibility

The card and its buttons expose names from the start: each row is a button
whose accessible name is the full label, with the position ("1 of 3"), and
the card is a window named by its summary. Full UIA support and keyboard
navigation inside the card is #119; this slice must not make it harder (real
child buttons, not a single painted surface). High contrast and the text
scale setting are honoured as on the existing cards. Contrast of the keycap
and the default outline is checked in light, dark and high-contrast themes,
at 4.5:1 for text and 3:1 for the outline and keycap border (WCAG 1.4.11).

The card never takes focus, so Tab and Narrator cannot reach the buttons
until #119. Until then the keyboard path is the digit and Esc claim and the
Wingman key itself. To make the card discoverable, showing it raises a system
alert event named by the summary (see "Shared rules"), and the hint line is
part of the card's accessible description. A user who cannot rely on either
can set `options.claim_keys = false` and use the key, or pin an action with
`primary_action`.

## The key: single versus double press

### Where the timing lives

**Decision: the hook stays dumb; the main thread owns timing.** The hook runs
on every keydown system wide and Windows unhooks a hook that takes too long.
It keeps doing what it does: match, post, swallow, Ctrl tap. It posts
`WM_APP_HOTKEY` for every distinct press, and the decision logic runs in the
message handler on the main thread.

Two things the hook must add, both small and lock-free:

1. **Ignore auto-repeat.** A held key must be one press, not a stream.
   `KBDLLHOOKSTRUCT` has no repeat bit, so the hook records that the primary's
   vk is down (an atomic) on the first matching keydown and clears it on the
   matching key-up (the hook currently returns early on every non-keydown
   message; it needs to look at key-up for that one vk). A repeated keydown
   while the flag is set is swallowed but not posted. If the key-up is missed
   (a focus change on a secure desktop), the flag would stick, so it also
   clears when no matching keydown has been seen for 2 s. The 2 s is measured
   from the **last** keydown, not the first: a repeating key refreshes it, so a
   4 s hold is still one press.
2. **Keep the Ctrl tap on every distinct swallowed press, including the second
   of a double, and not on swallowed repeats.** The Win-key cancel trick
   exists because a swallowed Win chord leaves Win logically down and the
   eventual key-up would open Start. A double press is two swallowed chords in
   a row; each distinct press needs its own tap, and the own-synthetic-input
   marker (`INJECTED_MARKER`) already stops the hook treating its own taps as
   presses. A swallowed repeat belongs to the press that started the hold and
   sends no further tap. MEASURED on the dev machine before this ships
   (Manual check 2): that neither the second press of a double nor a long hold
   makes Start flicker.

### The state machine

A pure type in a new module, `keypress.rs`, with no `windows` dependency:

```rust
pub enum Press { Single, Double }
pub enum Phase {
    Idle,
    Looking { router_started: bool },
    Options { count: usize, key_pickable: [bool; 4] },
    Running { committed: bool },   // committed = the Do phase, cannot be cancelled
    Answer,                        // answer, error or info
    Preview,                       // Do it / Edit / Book anyway / send preview
}
pub struct Context { phase: Phase, overlay: bool, mode: KeyMode, has_last_action: bool }
pub enum KeyAction {
    Dismiss, Cancel, Look, RunOption(usize), RepeatLast, Absorb, Ignore,
}
pub fn on_key(ctx: &Context, press: Press) -> KeyAction;      // the table
pub fn needs_window(ctx: &Context) -> bool;                    // wait to tell single from double?
```

`needs_window` is `true` only where single and double would do different
things: no card with a last action to repeat, and an options card with at
least 2 options (Dynamic or Always pick). While `Looking { router_started:
false }` the window is the thing being waited out. Everywhere else the first
press acts at once, so the window costs nothing there. `Absorb` is the second
tap of a double when there is nothing to repeat.

When `needs_window` is true, the first press arms a **one-shot** window timer
(`SetTimer` on the main window, killed when it fires or a second press
arrives). It is armed only after a press, never while idle, so rule 5 holds
(no polling, nothing running between presses). Expiry means Single, a second
press inside the window means Double, a third press inside the same window is
ignored (no triple). Every hide path kills the timer through one function,
which a unit test covers.

After the key dismisses or cancels something, presses are ignored for one
window length. Otherwise the second tap of a double on an answer card would
dismiss it and then immediately start a new Look.

### No latency on the common press

For "no card", waiting out the window before capturing would add its length
to every Look. Instead:

- The first press **captures at once** (the router-size capture, 512 px long
  edge) and **shows the options shell immediately**. The screenshot is taken
  before the shell exists, so it is the screen the user meant and never
  contains the card. The capture is held in memory only, never written, and
  dropped on cancel, on a double press, and when the router has used it.
- The router request (the billed part) starts when the window expires with no
  second press. A second press inside the window instead runs the last action
  by the normal path: the shell hides, the card settles and the action takes
  its own full capture, as every action does today. A double press therefore
  never costs a router call, and the early router-size capture is discarded.
- Delay added to a normal Look: the window length before the router starts.
  Delay added to picking an option by key: the window length (option 1 must
  wait to see whether option 2 was meant). Clicking an option and keys 1 to 4
  never wait.

### The window length

`hotkeys.double_press_ms`, default **300**, clamped 150 to 600. `0` turns
double-press off: the first press always acts at once, so option 1 is instant
and "repeat last" and "run option 2 by key" are unavailable (the number keys
still pick 2).

THEORY (unverified): 250 to 300 ms is enough for a deliberate double tap and
short enough not to feel laggy. #42 suggested 400 ms. The default is not
final until measured on the real key (Manual check 1): tap it ten times
quickly and ten times deliberately double, record the inter-press gaps, and
set the default at the 95th percentile of deliberate doubles. Laptop firmware
that emits the Copilot key as press and release in one burst (learn mode
exists because some does) would defeat double-press detection entirely; the
check covers that too.

## What a bare press runs instead of Check my work (#308)

**Decision: the default is the options card** (`hotkeys.primary_action =
"options"`). `Trigger::Hotkey` stops calling `ask`. `WM_APP_HOTKEY` and
`WM_APP_ACTIVATE` both go through `on_key` and the dispatch.

Check my work stays a first-class option: it is in the catalogue, so the
router offers it on a physics screen and it keeps its place in the palette.
Anyone who wants today's one-press flow pins it (below).

### Cost per press

The headline: **a press now costs one router call, and an option costs one
more call, so a press that used to cost one Check my work call costs about
two calls.** Not a silent change, so it is stated here and goes in the
release notes.

| | Before (Check my work) | After (options card, option picked) |
|---|---|---|
| Calls | 1 | 1 router + 1 action |
| Image sent | up to 1568 px long edge, about 1.5k input tokens | router: 512 px, about 350 tokens; action: as before |
| Router prompt | none | system prompt plus the candidate list, about 250 tokens for the built-ins |
| Output | the answer | router: at most 400, typically 100 to 150; action: as before |
| Latency to a result | one call | router (about 1 s) plus the action |

The image and prompt token figures are THEORY (unverified): estimated from
the capture size and the usual width times height over 750 rule, not measured.
The latency of about 1 s is the figure the expansion plan quotes for
`claude-haiku-4-5` / `qwen3.5:4b`. Both are measured before slice 3 ships
(Manual check 4: read the real token counts from the usage log and the latency
from the egress log, on the cloud and local paths).

The router uses the cheapest ready provider (`cheapest_router_target`), so a
user whose main action runs on an expensive model pays the cheap model for the
router half.

Ways a press avoids the extra call, in order of use:

- **Double press repeats the last action** (slice 5): no router call.
- **`primary_action` pinned to an action** (slice 5): no router call, today's
  one-press flow.
- **Per-app rules** (slice 6): a rule for the foreground app can skip the
  router for that app.
- **No model, or Offline with no local model**: the router never runs. The
  static model-free card is shown. Zero network, zero cost.

Local versus cloud:

- Cloud: one small vision call per press, the 512 px image leaves the
  machine (through the existing redaction and payment-denylist gates, which
  run before any capture is sent).
- Local (Ollama, `127.0.0.1:11434`, `think` explicit, `keep_alive` top
  level): no data leaves, no cost, but the latency of a 4B vision model on the
  dev machine's iGPU is unmeasured (Manual check 4). If it is several seconds,
  the placeholder card carries the wait, and `primary_action` is the escape.
- Offline mode: the router runs on the local model or not at all, never the
  cloud. The card still offers the local-only options.

### Bare launch

A bare first launch does nothing (unchanged). A **duplicate** launch (Start
menu, the Copilot-key picker, `poke_existing`) arrives as `WM_APP_ACTIVATE`
and cannot be told apart by argv, so it needs one rule for both:

**Decision: activation opens the options card in its static form: the
model-free options only, with no router call and so no network.** (A "last
used action" row joins it in slice 5, when that memory exists.) The picker
route still gives the user a card to act from, and the Start menu cannot cost
money. The hotkey (the hook) is the only route that starts the router. The
existing activation rate limit stays. Egress log check (Manual check 3):
launch the exe from the Start menu with Wingman running and confirm no new
egress line.

This is a deliberate narrowing of #308's "the picker's activation opens the
card": it does open a card, just without the router.

### Pause and Offline

While paused, the key passes through as it does today (the hook returns
before matching). Offline is covered above and in "Failure" (the router gates).
`model_action_gate` still decides busy and paused first for anything that
would run a model. Pause and Offline never leave a claim set: pausing hides
the options card and clears the digit claim.

## Config

All fields are optional with defaults, `#[serde(default)]`, so an existing
`config.toml` loads unchanged and no migration step is needed.

```toml
[hotkeys]
key_mode = "dynamic"        # "dynamic" | "pick" | "close"        (#393)
double_press_ms = 300       # 0 turns double-press off; clamped 150..600
primary_action = "options"  # "options" | "<action id>"           (slice 3, minimal form)

[options]
max_options = 4             # clamped 2..4
min_confidence = 0.2        # card inclusion bar; palette.router_threshold unchanged
click_away_dismiss = true
auto_dismiss_secs = 20      # options card only; clamped 5..120
claim_keys = true           # claim 1..N and Esc while the card shows
```

- Unknown `key_mode` or `primary_action` values fall back to the default and
  name the bad value in a card (rule 7), they do not stop startup. An id that
  is not in the catalogue is the same failure, shown the first time the key
  is pressed.
- `primary_action` ships in slice 3 in its minimal form (`"options"` or an
  action id) so the escape hatch lands with the change it exists for. Per-action
  chords and the last-action memory are slice 5.
- `key_mode` and `double_press_ms` appear in Settings next to the key
  settings (#345). `primary_action` is a dropdown of the catalogue plus
  "Show options".
- **Behaviour change for existing users:** after upgrade, the key no longer
  runs Check my work. The release note says so in one line and names the
  setting. There is no one-time prompt, since a prompt on first run is itself
  an unasked card.
- Settings strings follow rule 11: no em dashes.

## Per-action hotkeys and per-app rules (later slices)

### `primary_action` and per-action chords (#42)

- `primary_action = "<action id>"` makes a single press run that action,
  skipping the router. With it pinned, the options card is reachable from the
  tray item "What can I do here?" (slice 2) and from a duplicate launch (the
  static card). The palette chord opens the palette, not the options card.
  A pinned action still allows a double press to repeat the last action.
- An action may declare a chord (`Action.hotkey`, a field that exists and is
  inert today, #535). The hook matches it like the palette chord (a locked
  snapshot, not the pause chord's atomic) and posts a new message carrying the
  action id. A per-action chord always runs that action directly: no router, no
  options card. It does not fire while paused. Conflicts with the primary,
  secondary, pause, palette and each other are checked at load by
  `hotkey_conflicts.rs`, and a conflict is a named card, not a silent loss.
- "Last action" is remembered in memory only (the id of the last action that
  completed or was started by the user, not a router guess). It is not
  written to disk, so a restart has none. A double press with none is
  absorbed (see the table), it does not become a second Look.

### Per-app rules (#107)

Rules keyed by process name or window-title pattern give a default action, a
preferred model, and an awareness exclusion. For the options card, a rule's
default action is placed as option 1 **before the router answers**, so the
card is useful at once and the key picks it. A rule can also set "skip the
router" for that app. Rules are read at press time from the foreground
window, with no watcher and no polling (rule 5). The config shape and the
precedence against `primary_action` and the router are settled in their own
short spec once slices 1 to 4 have shipped; nothing in the earlier slices
blocks it.

## Failure cards

Every failure ends in a card (rule 7). New ones, plus what is not a failure:

| Case | Result |
|---|---|
| Router error, timeout, refusal, unparseable | Options card with the floor and a plain summary. Reason in the diagnostics log. |
| Router returns options but all invalid or filtered | Same: the floor. |
| Cloud router with "confirm before sending" on | The floor, no network. The chosen action shows its own send preview. |
| No model ready | Static model-free card, no network. |
| A chosen option fails | That action's own error card. The options card is already gone. |
| `primary_action` names a missing action | A card naming the setting and the id, once. |
| Capture fails (secure desktop, password manager, payment page) | The existing capture-gate card, shown instead of the options card, before any router call. |
| Press while paused | Passes through, nothing shown. |
| Key cancels an in-flight Look or Propose request | No card. Not an error. |
| Key during the committed Do phase | Ignored. The result card appears as usual. |
| Options card left up | Auto-dismisses after `options.auto_dismiss_secs`, the digit claim lapses with it. |
| Activation refused by the rate limit | The existing "Too many requests in a row" card. |

## Testing

Pure logic is unit-tested (rule 8); each Win32 piece has a named manual check.

### Unit tests (tests first)

- `router.rs`: the validation pipeline. The #306 fixture: a reply with five
  options, one unknown id, one duplicate, one below the bar, becomes exactly
  the expected ranked list of at most 4. Plus: ties keep catalogue order;
  confidence clamping; label sanitising (markdown, em dash, empty, a
  multi-byte label clamped on a char boundary, never panicking); the old
  single-intent shape reads as one option; the built schema's property order
  (`summary` then `options`; `intent`, `label`, `confidence`) and its
  `enum` equal the candidate ids.
- `with_fallbacks`: fewer than 2 options adds Explain then Copy text; no
  model gives the static model-free list; dedupe against existing options.
- `keypress.rs`: `on_key` as the full table, for all three key modes, for
  every card kind, in flight or not, with and without a last action, for
  Single and Double (a property-style loop over all combinations asserting a
  preview card never yields `RunOption`, in any mode). `needs_window` over
  the same grid. The settle rule after a dismiss or cancel.
- The key-claim function for the hook (which of 1 to 4 and Esc are claimed in
  which card state): claims nothing unless the options card is showing.
- Auto-repeat filter: a pure `should_post(held, elapsed)`.
- `config.rs`: defaults, round trip, clamping, unknown values fall back.
  A source-scanning guard (CRLF-normalised, candidate set asserted non-empty)
  that the options dispatch calls `dispatch_palette_action` and no second
  match on action ids exists.
- A test that no `Trigger::Hotkey` or `Trigger::Activation` path reaches a
  provider call without going through the readiness gate.

- `keypress.rs` additions: every `Phase` (including `Looking { router_started }`
  and `Running { committed }`) against Single and Double, with and without a
  last action. A committed run never yields `Cancel` or `Dismiss`. A double
  with nothing to repeat yields `Absorb`, never `Cancel`. `key_pickable`: a
  clipboard-writing option below the threshold is never key-picked, and a
  card whose option 1 fails it dismisses.
- The key-claim function: no modifier, only digits up to the option count,
  nothing past the expiry tick, nothing in any state but Options.
- Auto-repeat filter: the 2 s lapse is measured from the last keydown, so a
  4 s hold stays one press. The Ctrl tap is sent once per distinct press.
- The options generation: a stale click or digit after the card changed runs
  nothing.

### Manual checks (each names the observable that differs if it is wired to nothing)

1. **Double-press timing.** Observable: the measured gaps in a scratch log;
   the default is set from them. Also confirms the Copilot key delivers two
   distinct presses.
2. **Start menu on the second tap.** Observable: press the key twice quickly
   over a window; the Start menu does not open on either tap. Wired to
   nothing would show Start flickering open.
3. **Bare launch is free.** Observable: launch the exe from the Start menu
   with Wingman running; the egress log gains no line.
4. **Router cost and latency.** Observable: the usage log token counts and
   the egress log timing for one press on cloud and on local.
5. **Options card.** At 100% and 150% scaling, light, dark and high contrast:
   three stub options show, clicking the second or pressing 2 runs that
   action's normal flow, Esc closes with nothing run, a click elsewhere
   closes and still reaches the app underneath, and the card never steals
   typing focus.
6. **Key modes.** With three options showing: one press runs option 1, a quick
   double runs option 2; with no card, a double repeats the last action;
   switching to "Always closes the card" makes a press close it with nothing
   run; a preview card is dismissed, never confirmed, in every mode.
7. **Held key.** Holding the key down runs one Look, not several.

8. **Digit claim.** With an IME active and with the numpad: while an options
   card shows, digits that name an option run it, Ctrl+1 and Win+1 still reach
   the browser and taskbar, 4 passes through on a 3-option card, and once the
   card auto-dismisses digits type normally again. Observable: a digit typed
   into Notepad after the card closes appears.
9. **Password manager and send guard.** With a password manager in front the
   key shows the capture-gate card and the egress log gains no line. With
   "confirm before sending" on and a cloud provider, the key shows the floor
   and the egress log gains no line.
10. **Narrator.** The card appearing is announced, and the buttons have their
    names ("1 of 3").

## Slices

Each ships alone and leaves the app working, with the key behaving sensibly
at every step.

1. **Router shortlist (#306).** `RouterResult.options`, schema, validation,
   labels, fallbacks, and the `effect` column on the built-in catalogue. The
   any remaining palette consumer reads `options[0]`. The palette also stops
   sending a router request when it opens (see "Related decisions"). Done when
   the fixture test passes and opening the palette makes no request in the
   egress log (checked by hand).
2. **Options card (#307).** `CardState::Options`, keys 1 to 4 and Esc (the
   bounded claim, with auto-dismiss), click to pick, hide-before-capture.
   Reachable from a new tray item "What can I do here?", behind no setting,
   since the bare key is unchanged (the key keeps running Check my work, even
   with the card up). Done when Manual checks 5 and 8 pass.
3. **Bare key opens the options card (#308).** The default flips. This slice
   also carries what the flip needs to be safe: the auto-repeat filter, the
   "Options card" and "Looking" rows of the key table with single-press
   behaviour only (`keypress.rs` with `double_press_ms = 0` behaviour, so no
   timer yet), the committed-state rule, the static activation card,
   `primary_action` in its minimal form, the router gates, a one-line pointer
   in the expansion plan §6 to this spec, and the release note. Done when the
   Done-when of #308 holds and Manual checks 3, 4, 7 and 9 are recorded.
4. **The key picks and click-away (#393, #324).** `key_mode`, the Settings
   choice, click-away rules, the preview surviving focus loss (send preview
   excepted), `options.click_away_dismiss`. Done when Manual checks 6 and 10
   pass.
5. **Double press and per-action chords (#42).** The window timer, repeat last
   action, `double_press_ms`, the last-action row on the static card,
   per-action chords and their conflict checks. Gated on Manual checks 1 and
   2.
6. **Per-app rules (#107).** Its own short spec first.

Slices 1 and 2 are pure additions. Slice 3 is the one user-visible behaviour
change and should be the last to merge before a release.

### Size budget

No new crate and no new `windows` feature is needed (`SetTimer`, the keyboard
hook, `NotifyWinEvent` and the owner-drawn buttons are all in features already
enabled). Estimates, each THEORY (unverified) until measured: slice 1 about
4 KB, slice 2 about 15 KB, slice 3 about 6 KB, slice 4 about 6 KB, slice 5
about 10 KB, about 40 KB in all. The gate is `perf-budgets.json`
(`exe_size_bytes.max` 4,500,000, last measured 3,982,336), enforced by
`scripts/perf-budget.ps1`, leaving about 518 KB for this spec and the Settings
dark mode, overlay and knowledge specs together. Each slice's PR records the
measured size from CI. A slice that would break the gate raises it in the same
PR with the measured number and reason, as earlier PRs did, and the
bookkeeping across specs is the order they merge in.

## Resolved questions

Each was an open question with a recommended answer; the recommended answer
is the decision.

1. **Decision: the options card uses its own `options.min_confidence` (0.2),
   not `palette.router_threshold` (0.7).** At 0.7 the card would often hold
   one option. Small-model confidences are poorly calibrated, so 0.2 is a
   tunable, not a fact.
2. **Decision: the key runs option 1 even at low confidence, but only when
   option 1 is key-pickable.** A clipboard-writing option (Copy text from
   screen, Copy region) is key-picked only at or above the router threshold,
   otherwise the key dismisses, so a habit press never overwrites the
   clipboard silently. The hint line shows "Esc to close", and option 1's
   keycap is drawn without the accent outline when the key will not run it.
3. **Decision: the hook claims digits and Esc while the options card is
   open**, under the conditions above (no modifiers, only digits that name an
   option, an auto-dismiss, an expiry that lapses by itself,
   `options.claim_keys`). Letting the card take focus would steal typing.
   Revisit if the digit capture annoys anyone in testing.
4. **Decision: activation shows the static card.** The picker route does not
   run the router; the hotkey route does.
5. **Decision: ship `double_press_ms` at 300 as a placeholder** and set it
   from Manual check 1 before slice 5 merges.
6. **Decision: Always pick does nothing on an answer card.** Esc closes. A
   key that closes in a mode named "pick" would surprise.
7. **Decision: the options card auto-dismisses after 20 s** (this bounds the
   digit claim). Answer cards keep their current timers and
   `PENDING_SAFETY_TIMEOUT_SECS`.
8. **Decision: the router timeout is 10 s, and the card shows the
   placeholder while it waits.** If Manual check 4 shows a median router
   latency above 2 s, the model-free rows show at once and router rows
   replace them only when no pointer is over the card and no key was
   pressed.
9. **Decision: the send preview keeps its focus-loss cancel.** If this proves
   annoying, give it a long one-shot expiry instead of removing the cancel.

## Related decisions

- **Decision: the Quick Ask palette sends no model request when it opens.**
  It sends one only after the user picks an action or types something. The
  router call belongs to the key press and the options card, not to opening
  the palette.
- **Decision: a remote Ollama `base_url` in Local mode is allowed, with a
  one-time warning that the screenshot leaves this PC.** The router follows
  the same rule.
