# The options card and the Wingman key

Status: **proposed, 2026-10-16** (AGENTS.md rule 12). Covers #305 (epic),
#306, #307, #308, #324, #393, and, as later slices, #42 and #107. Supplements
the [expansion plan](2026-09-16-expansion-plan-design.md) §6 "The intent
router" and the [palette spec](2026-09-17-palette-design.md). Where they
disagree on what a bare key press does, this spec wins once approved.

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

## User-visible behaviour

### States

| State | What the user sees |
|---|---|
| Idle | Nothing. |
| Options pending | The options card appears at once, in the usual corner, with a summary placeholder ("Looking at your screen") and no buttons yet. |
| Options ready | Summary line, then 2 to 4 numbered buttons. |
| Action running | The existing Pending card for that action. |
| Result | The existing answer, error, info or preview card. |

### What a press does

This is the #393 table, made exact. "Option" means a button on the options
card. "Double" means two presses within the double-press window (below).

| State when the key is pressed | Single press | Double press |
|---|---|---|
| No card | Look: capture, show the options card, route | Repeat the last action on the current screen (#42 slice) |
| Options card with options | Run option 1 | Run option 2 (nothing if there is no option 2) |
| Options pending (router in flight) | Cancel, no card | Cancel, no card |
| Action running (request in flight) | Cancel, no card (the generation pattern `preview.rs` uses drops the late result) | Same |
| Answer, error or info card (no options) | Dismiss | Dismiss |
| Preview card (Do it, Edit; Book anyway) | Dismiss, nothing runs | Dismiss, nothing runs |

A cancel is not an error: no card, no message.

**Decision: a preview card does not count as "has options" for the key.**
Its option 1 is Do it, which writes. Pressing the key to get rid of a card
out of habit must never act. Enter (or the button) confirms, as today.
"Book anyway" (#390) is never reachable from the key. Only the options card
is key-pickable. This holds in every key mode, including Always pick.

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
  info cards, and passes through to the app underneath.** The click is never
  swallowed, so the user still lands in the app they clicked. The `dismiss.rs`
  watcher stays, and is armed while the options card shows.
- **Decision: a preview card survives focus loss and click-away.** It can hold
  edits, and an Alt+Tab to check a date must not discard them (#324's real
  complaint). It closes on Esc or the key. `preview_focus_left_the_card`'s
  silent cancel is removed in the same slice that changes the key table (it
  is the #324 half that still stands).
- A setting `options.click_away_dismiss` (default `true`) lets a user who
  wants #324's persistent card turn the dismissal off for answer and options
  cards. Esc and the key still close.
- The card never takes focus when shown (`WS_EX_NOACTIVATE`), so typing keeps
  going to the app underneath. Keys 1 to 4 and Esc therefore need a way to
  reach the card; see "Keys 1 to 4".

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
(`palette_model`, `should_apply`) reads `options.first()`, so its behaviour
is unchanged.

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
Two choices, with a recommendation:

- **Decision: handle 1 to 4 and Esc in the existing low-level hook**, but
  only while the options card is showing (a lock-free atomic flag, set and
  cleared by the card, like the paused flag). A matching key is swallowed and
  posted as a message. Any other key passes through untouched. This keeps
  focus in the user's app and works without a click.
- The cost is that digits cannot be typed into the app underneath while the
  options card is open. The card is short-lived and `click_away_dismiss` plus
  Esc get the user out; the hint line says so. See the open question.

Esc closes the card with nothing run. The flag is cleared on every path that
hides the card (hide, pick, cancel, dismiss), and a unit test over the pure
"which keys are claimed in which card state" function proves nothing is
claimed in any other state.

### Accessibility

The card and its buttons expose names from the start: each row is a button
whose accessible name is the full label, with the position ("1 of 3"), and
the card is a window named by its summary. Full UIA support and keyboard
navigation inside the card is #119; this slice must not make it harder (real
child buttons, not a single painted surface). High contrast and the text
scale setting are honoured as on the existing cards. Contrast of the keycap
and the default outline is checked in light, dark and high-contrast themes.

## The key: single versus double press

### Where the timing lives

**Decision: the hook stays dumb; the main thread owns timing.** The hook runs
on every keydown system wide and Windows unhooks a hook that takes too long.
It keeps doing what it does: match, post, swallow, Ctrl tap. It posts
`WM_APP_HOTKEY` for every distinct press, and the decision logic runs in the
message handler on the main thread.

Two things the hook must add, both small and lock-free:

1. **Ignore auto-repeat.** A held key must be one press, not a stream. The
   hook records that the primary's vk is down (an atomic) on the first
   matching keydown and clears it on the matching key-up (the hook currently
   returns early on every non-keydown message; it needs to look at key-up for
   that one vk). A repeated keydown while the flag is set is swallowed but
   not posted. If the key-up is missed (a focus change in a secure desktop),
   the flag would stick, so it also clears on a short deadline (2 s,
   compared against the keydown's timestamp, no timer).
2. **Keep the Ctrl tap on every swallowed press, including the second of a
   double.** The Win-key cancel trick exists because a swallowed Win chord
   leaves Win logically down and the eventual key-up would open Start. A
   double press is two swallowed chords in a row; each needs its own tap, and
   the own-synthetic-input marker (`INJECTED_MARKER`) already stops the hook
   treating its own taps as presses. MEASURED on the dev machine before this
   slice ships (Manual check 2): that the second press of a double still does
   not flicker Start.

### The state machine

A pure type in a new module, `keypress.rs`, with no `windows` dependency:

```rust
pub enum Press { Single, Double }
pub struct Context { card: CardKind, in_flight: bool, mode: KeyMode, has_last_action: bool }
pub enum KeyAction {
    Dismiss, Cancel, Look, RunOption(usize), RepeatLast, Ignore,
}
pub fn on_key(ctx: &Context, press: Press) -> KeyAction;      // the table
pub fn needs_window(ctx: &Context) -> bool;                    // wait to tell single from double?
```

`needs_window` is `true` only where single and double would do different
things: an options card with at least 2 options (Dynamic or Always pick), and
no card with a last action to repeat. Everywhere else the first press acts at
once, so the window costs nothing there.

When `needs_window` is true, the first press arms a **one-shot** window timer
(`SetTimer` on the main window, killed when it fires or a second press
arrives). It is armed only after a press, never while idle, so rule 5 holds
(no polling, nothing running between presses). Expiry means Single, a second
press inside the window means Double, a third press inside the same window is
ignored (no triple).

After the key dismisses or cancels something, presses are ignored for one
window length. Otherwise the second tap of a double on an answer card would
dismiss it and then immediately start a new Look.

### No latency on the common press

For "no card", waiting out the window before capturing would add its length
to every Look. Instead:

- The first press **captures and shows the options card shell immediately**,
  as it would anyway. The screenshot is taken at the press, so it is the
  screen the user meant.
- The router request (the billed part) starts when the window expires with no
  second press. A second press inside the window instead runs the last action
  on the capture already taken. A double press therefore never costs a router
  call.
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
  run before any capture is sent, same as the palette's router today).
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
model-free options plus the last used action, with no router call and so no
network.** The picker route still gives the user a card to act from, and the
Start menu cannot cost money. The hotkey (the hook) is the only route that
starts the router. The existing activation rate limit stays. Egress log check
(Manual check 3): launch the exe from the Start menu with Wingman running and
confirm no new egress line.

This is a deliberate narrowing of #308's "the picker's activation opens the
card": it does open a card, just without the router.

### Pause and Offline

While paused, the key passes through as it does today (the hook returns
before matching). Offline is covered above. `model_action_gate` still
decides busy and paused first for anything that would run a model.

## Config

All fields are optional with defaults, `#[serde(default)]`, so an existing
`config.toml` loads unchanged and no migration step is needed.

```toml
[hotkeys]
key_mode = "dynamic"        # "dynamic" | "pick" | "close"        (#393)
double_press_ms = 300       # 0 turns double-press off; clamped 150..600
primary_action = "options"  # "options" | "<action id>"           (#42, slice 5)

[options]
max_options = 4             # clamped 2..4
min_confidence = 0.2        # card inclusion bar; palette.router_threshold unchanged
click_away_dismiss = true
```

- Unknown `key_mode` or `primary_action` values fall back to the default and
  name the bad value in a card (rule 7), they do not stop startup. An id that
  is not in the catalogue is the same failure, shown the first time the key
  is pressed.
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
  skipping the router. The options card is then reachable from the palette
  chord and the tray. With a pinned action, `needs_window` is false for the
  no-card state only if there is nothing to repeat; a double press still
  repeats the last action.
- An action may declare a chord (`Action.hotkey`, already a field the
  override-merge spec refers to). The hook matches it like the palette chord
  (a locked snapshot, not the pause chord's atomic) and posts a new message
  carrying the action id. A per-action chord always runs that action
  directly: no router, no options card. It does not fire while paused.
  Conflicts with the primary, secondary, pause, palette and each other are
  checked at load by `hotkey_conflicts.rs`, and a conflict is a named card,
  not a silent loss.
- "Last action" is remembered in memory only (the id of the last action that
  completed or was started by the user, not a router guess). It is not
  written to disk, so a restart has none, and a double press with none behaves
  as a single press.

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
| No model ready | Static model-free card, no network. |
| A chosen option fails | That action's own error card. The options card is already gone. |
| `primary_action` names a missing action | A card naming the setting and the id, once. |
| Capture fails (secure desktop, password manager, payment page) | The existing capture-gate cards, unchanged, before any router call. |
| Press while paused | Passes through, nothing shown. |
| Key cancels an in-flight request | No card. Not an error. |
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

## Slices

Each ships alone and leaves the app working.

1. **Router shortlist (#306).** `RouterResult.options`, schema, validation,
   labels, fallbacks. The palette reads `options[0]`; nothing else changes.
   Done when the fixture test passes and the palette still pre-selects its
   top option (checked by hand).
2. **Options card (#307).** `CardState::Options`, keys 1 to 4 and Esc, click
   to pick, hide-before-capture. Reachable from the palette chord and tray
   only, behind no setting, since the bare key is unchanged. Done when Manual
   check 5 passes.
3. **Bare key opens the options card (#308).** The default flips, the static
   activation card, `primary_action` read in its minimal form (`"options"` or
   an action id, so the escape hatch ships with the change it exists for),
   release note. Done when the Done-when of #308 holds and Manual checks 3 and
   4 are recorded.
4. **The key picks (#393, #324).** `keypress.rs` and `on_key`, `key_mode`,
   the hook's auto-repeat filter and card-open key claim, click-away rules,
   preview survives focus loss, the Settings choice. Single press only at
   first (`double_press_ms = 0` behaviour), so Manual checks 6 and 7 can pass
   before timing is settled.
5. **Double press and per-action chords (#42).** The window timer, repeat last
   action, `double_press_ms`, per-action chords and their conflict checks.
   Gated on Manual checks 1 and 2.
6. **Per-app rules (#107).** Its own short spec first.

Slices 1 and 2 are pure additions. Slice 3 is the one user-visible behaviour
change and should be the last to merge before a release.

## Open questions

Each has a recommended answer; the first four affect slice 1 to 4 work.

1. **Card inclusion bar.** Should the options card use its own
   `options.min_confidence` (0.2) rather than `palette.router_threshold`
   (0.7)? Recommended: yes. #306 says to drop below the palette threshold,
   but its own example keeps 0.41 and 0.12, and at 0.7 the card would often
   hold one option.
2. **Key runs a low-confidence option 1?** Recommended: yes. Every option is
   read-only or ends in a preview that needs Enter, so a wrong guess costs
   one call and one dismiss, never an action. The floor guarantees option 1
   is at least a sane choice.
3. **Digits and Esc claimed by the hook while the card is open.** The
   alternative is to let the card take focus, which steals typing.
   Recommended: claim them in the hook while the card shows, and tell the user
   in the hint line. Revisit if the digit capture annoys anyone in testing.
4. **Static card on activation.** Recommended: yes, as above. The cost is that
   the picker route no longer runs the router; the hotkey route does.
5. **Double-press default.** 300 ms versus #42's 400 ms. Recommended: ship
   300 as a placeholder and set it from Manual check 1 before slice 5 merges.
6. **Should Always pick do anything on an answer card?** Recommended: no, it
   falls through to nothing, and Esc closes. A key that closes in a mode
   named "pick" would surprise.
7. **Auto-dismiss timers on answer cards (#324).** Out of scope here; the
   current timers and `PENDING_SAFETY_TIMEOUT_SECS` are unchanged. Recommended:
   a separate small change after slice 4, since a persistent card plus
   click-away dismissal covers most of #324's reason.
8. **Router timeout.** Recommended: 10 s, shorter than the 30 s the main
   action gets, since after that the user is better served by the floor.
   Needs the latency measurement (Manual check 4).
