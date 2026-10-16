# The in-place overlay

Status: **proposed, 2026-10-16** (AGENTS.md rule 12). Covers #325 (the
overlay), #326 (clickable), #329 (ghost text in form fields), #332 (red mark
on the wrong line), #333 (translate in place), #337 (underline the stack-trace
line), the overlay half of #334, #335 and #336 (labels for jargon and dates),
and settles the question in spike #383 (how to locate text on screen) as far as
the code and docs allow. Fits with the
[options card spec](2026-10-16-options-card-design.md) (draft, PR #661) and
supplements the [expansion plan](2026-09-16-expansion-plan-design.md) §7
"Outputs", which today lists card, insert at cursor, clipboard and executor
outcomes. This adds one: **annotations drawn on the screen, next to the text
they are about.**

## The problem

Some answers belong on the screen, beside the thing they are about. A card in
the corner that says "line 3 has a sign error" makes the user find line 3. A
label beside "Thu 9:00 PT" that says "Thu 17:00 your time" needs no finding.
Eight open issues want this, each with its own sketch of a window, an anchor
and a hide rule. Built separately they would be eight windows. This spec is
the one window, the one way of finding text, and the one proposal shape, so
each issue becomes a small slice on top.

It stays inside the product rules: one press, one action, one card, no chat.
Look, Propose, Confirm, Do. Never Send, Submit, Buy or Pay. Nothing runs while
idle. Every failure ends in a card. Redaction applies before any upload.

## Where it is today

- `src/ocr.rs` **already returns word boxes.** `OcrLine { text, x, y, width,
  height, words }` and `OcrWord { text, x, y, width, height }`, in the pixel
  space of the image passed in. `recognize_inner` multiplies every rect by
  `(sx, sy)` to undo any downscale to `OcrEngine::MaxImageDimension`
  (10000, so a real screen is never downscaled). Reading order is the engine's.
  The line rect is the union of its words.
- `src/redact.rs` already turns text spans into word-box regions:
  `word_byte_ranges` and `union_of_overlapping` map a byte span in a line to the
  union of the words it touches, and fall back to the whole line when words and
  text disagree. The anchor matcher below is the same idea for quoted text
  and should share these helpers rather than copy them.
- `capture::RawShot.screen: ScreenMap { origin_x, origin_y, scale_x, scale_y,
  .. }` says where pixel (0, 0) sits in virtual-screen physical pixels and how
  many screen pixels one image pixel covers. The process is per-monitor-v2
  aware (`app.rs`), and `capture.rs` documents that UIA rectangles and
  `GetMonitorInfoW` agree with this space. So image pixel to screen pixel is
  one multiply and one add, and a UIA rectangle needs no conversion at all.
- `src/inputs/uia.rs` snapshots each field's bounding `Rect` (screen
  pixels). `src/inputs/page_text.rs` and `selection.rs` read `TextPattern`
  text. **Nothing calls `GetBoundingRectangles` on a text range today.**
- `src/ui/region.rs` is the only full-screen overlay: one topmost window over
  the virtual desktop, drawn from a frozen frame. It is opaque (dimmed
  screenshot), not layered, so it does not answer the layered-window questions.
- **No code uses `SetWinEventHook`, `SetWindowDisplayAffinity`,
  `WS_EX_LAYERED` or `UpdateLayeredWindow`.** (grep, 2026-10-16.) The cards are
  ordinary topmost popups; they stay out of captures only because the flows
  hide them first.
- `ui/card.rs`: `CardState { Hidden, Pending, Collapsed, Expanded, Preview }`,
  drawn from the `ui/theme.rs` tokens in light, dark and high contrast, with a
  gallery (`ui/gallery.rs`) that renders each state per theme.
- `dismiss.rs`: a `WH_MOUSE_LL` click watcher, armed only while an answer shows,
  that posts `WM_APP_DISMISS` and never swallows a click.
- `actions/schema.rs`: proposal kinds by name. `verdict` requires
  `["detail", "headline"]` in that order (a test pins it), `scam_check` is
  `signs: [string]` then `verdict`, `time_list` lists the times read off the
  screen. **`Action` has no `output` field**; `executor = "none"` means the card
  is the result.

- **Capture backend.** `Cargo.toml` takes `xcap = "0.9"` with default
  features, and xcap 0.9.8's `wgc` feature is off by default, so a monitor
  capture here goes through `gdi::capture_monitor`: `BitBlt` with `SRCCOPY`
  and no `CAPTUREBLT`. THEORY (unverified, read from the crate source):
  `BitBlt` without `CAPTUREBLT` skips layered windows, so today's capture may
  not contain a layered overlay even without the affinity flag. The flag is
  still required (below), because the user's screen share, a recorder and any
  future switch to the `wgc` feature all read the composed desktop. Slice 0
  measures both.
- **Exe size.** `perf-budgets.json` records 3,800,576 bytes measured against a
  gate of 3,900,000, about 99 KB of headroom, shared with the options card,
  Settings dark mode and knowledge specs. AGENTS.md's "about 3.5 MB" is stale.
  See "Size budget".
- `Action` (in `actions/mod.rs`) has `id, name, group, inputs, proposal,
  executor, confirm, prompt, prefer, hotkey, rate_difficulty, enabled` and no
  `output` or `detector` field. `Action.hotkey` exists and is inert (#535).

## Decisions

- **One overlay window type, owned by the main thread, never focused.**
- **The model never gives coordinates.** It quotes text. Wingman finds the
  quote on screen with its own OCR (or UIA) and draws at the box it found. A
  quote that cannot be found drops that one annotation, never the overlay.
- **Hide on any change, do not track.** Scrolling, typing, moving the window,
  switching tab or losing the window hides the overlay. A press re-draws it.
- **The overlay is never the only carrier of information.** The summary card
  repeats every annotation as text. This is the accessibility floor and the
  fallback when anchoring fails.
- **Read-only unless a slice says otherwise.** Only #326 adds clicks, and only on
  labels, never on the screen underneath.
- **The overlay is excluded from capture.** If that cannot be arranged, no
  overlay is shown.

- **The overlay belongs to a card.** Its key, Esc, click-away and lifetime rules
  are the options card spec's "Shared rules for every surface". The overlay is
  not a surface, not a `CardKind`, and never outlives its summary card or the
  preview it annotates.
- **Hook callbacks never decide.** A low-level hook callback only posts a
  message; the main thread decides whether the input matters. See "When the
  screen changes".
- **A guard that cannot run means no overlay.** The affinity call, the hooks,
  OCR and the main-thread decision all fail closed to the summary card alone.

## User-visible behaviour

1. The user presses the Wingman key. The usual Pending card shows.
2. When the action finishes, labels appear beside their text and a summary card
   appears in the corner as it does today ("3 things marked. Esc to close.").
   Appearing raises a system alert event named by the summary, so a screen
   reader user learns something is there.
3. The overlay goes away with its summary card: on Esc, on the key, on a click
   that is not on a label (the overlay always hides; the card follows
   `options.click_away_dismiss`), on any
   change listed under "When the screen changes", or after 30 s.
4. If nothing could be anchored, the user gets the summary card alone. If the
   card is the only surface, it says so in one line ("Could not place the marks
   on screen, here they are").

**Keys, Esc and click-away follow the options card spec's "Shared rules for
every surface".** In short: the key acts on the highest surface only (preview,
then options card, then answer card), and whatever it dismisses takes its
overlay with it. A press never runs anything while an overlay is showing, in
every key mode, for the same reason a preview is never confirmed by the key.
The overlay adds `overlay: bool` to the key state machine's `Context`, not a
card kind.

**Esc reaches the summary card by a bounded claim.** The summary card never
takes focus, so Esc has to be claimed in the hook, the same way the options card
claims 1 to 4 and Esc: only with no modifiers down, only while an overlay is
showing, and with an expiry tick (overlay age cap plus 1 s) that lapses by
itself, cleared on every hide path. The claim covers Esc only and no digits.

## The overlay window

### One window class, one window per monitor

A new module, `src/ui/overlay.rs`, owns class `Wingman.Overlay.<guid>` and
test-only class `...Test` (rule 9). Styles:

`WS_POPUP`, `WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW |
WS_EX_NOACTIVATE`, and `WS_EX_TRANSPARENT` when the annotation set is
read-only. It is shown with `SW_SHOWNOACTIVATE`, handles `WM_MOUSEACTIVATE`
with `MA_NOACTIVATE`, and is created on the main thread, which already pumps
messages for the card and the hooks. It never takes focus, so typing keeps
going to the app underneath and a clicked label does not pull the target out of
the foreground (#326 needs that: the replace executor writes to it).

**One window per monitor that has annotations, sized to the bounding box of that
monitor's annotations**, not to the monitor. The pixels are a premultiplied
32-bit DIB pushed with `UpdateLayeredWindow`. A full 4K monitor would cost
33 MB for one label near a corner; a bounding box of the marks costs a few
hundred KB. The memory is released on hide (rule 5: under 10 MB idle).

The capture is of the active monitor (one press, one monitor), so annotations
exist on one monitor. Multiple windows are supported anyway because #337's
terminal and editor can sit on either side of a monitor boundary only if the
capture is widened later. Windows are positioned in physical virtual-screen
pixels and never straddle a monitor edge: an annotation rect is clipped to its
monitor, and a label that would cross the edge is moved inward (see
"Layout").

### Per-monitor DPI

The process is per-monitor-v2, so `SetWindowPos` and `UpdateLayeredWindow` take
physical pixels and nothing is rescaled. What does scale is **drawing**: label
font, padding, border and mark thickness are design-pixel constants (96 DPI)
multiplied by `GetDpiForMonitor(MDT_EFFECTIVE_DPI)` of the monitor the window
is on, and by the text scale setting the card already honours
(`set_text_scale`). `region.rs`'s `scale_for_dpi` is a private fn; it moves to
`ui/theme.rs` (pure, already tested for the hint pill) so both use one copy.

### Drawing, and the alpha trap

GDI text drawn into a 32-bit DIB writes alpha 0, so text on a per-pixel-alpha
layered bitmap comes out invisible. The overlay avoids the trap by drawing
every label as an **opaque pill**: fill the pill with GDI, draw the text on it,
then run a pure pass that sets alpha to 255 and premultiplies colour over the
pill's rectangle. Marks (underlines, circles, ticks) are drawn from filled
rects the same way, with a 1-pixel halo in the surface colour so they read on
any page background. Nothing is semi-transparent except the soft edge of a
pill, which is generated, not drawn by GDI. The alpha pass takes a pixel
buffer and a list of rects and is unit-tested (it is the one place a wrong
byte order would make the overlay vanish without an error).

### Click-through and clickable regions

- **Read-only set**: `WS_EX_TRANSPARENT` on the window. Every click goes to
  the app underneath. This is the default and the whole of #325, #329, #332,
  #333, #334, #335, #336, #337.
- **Clickable set (#326)**: no `WS_EX_TRANSPARENT`. A per-pixel-alpha layered
  window only hit-tests where alpha is non-zero (Microsoft's layered-window
  documentation says areas "whose alpha value is zero will let the mouse
  messages through"), so the empty parts of the bitmap pass clicks on. As
  belt and braces `WM_NCHITTEST` returns `HTTRANSPARENT` for every point not
  inside a label rect, which is also the rule #326 states. THEORY (unverified):
  that the alpha-0 pass-through holds on this machine under DWM; Manual check 2.
- A click on a label posts `WM_APP_OVERLAY_CLICK` with the annotation's index
  and the overlay generation (the same stale-click guard the options card uses)
  and the window does not activate.

### Z-order

Topmost, like the cards. The summary card is shown **after** the overlay so it
sits above it, and `SetWindowPos(HWND_TOPMOST)` on the card after the overlay
appears keeps it so. An overlay does not draw over a true exclusive-fullscreen
app (games); `UpdateLayeredWindow` succeeds there but nothing shows. THEORY
(unverified), not measured. The summary card carries the result in that case,
which is why it is never optional.

### Excluding the overlay from capture

`SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` (Windows 10 2004 and
later; the target here is Windows 11) on every overlay window, so the next
press, a region capture, and any later awareness capture (#330) never read
Wingman's own labels as screen content. It also keeps labels out of the user's
screen share and recordings.

- **Slice 0 gate, before this spec is approved.** The whole design hangs on
  this working for the exact window kind used here: a `WS_EX_LAYERED` popup
  updated with `UpdateLayeredWindow`, not any window. A 20-line throwaway
  program records, for that window with and without the flag: (a) whether
  `xcap`'s monitor capture as Wingman uses it (the GDI `BitBlt` path) contains
  the labels, (b) whether a `wgc` or DXGI capture does, since the `wgc` feature
  could be enabled later and a screen share uses the composed desktop, and (c)
  whether `PrintWindow` on the overlay returns the labels, for the gallery. The
  result is written into this spec, replacing the theories. If the flag does
  not hold for a layered window, the design changes (see open question 3)
  before anything else is built.
- **Fail closed.** If the call fails, the overlay is not shown and the summary
  card says "Could not show marks on screen here, here they are". A visible
  overlay that a later capture can read is a privacy and feedback problem
  (a translation of private text re-uploaded as "the screen").
- **Belt and braces.** The key dismisses the overlay before a press captures
  anything (the shared key rule), and every Wingman-initiated capture path
  hides the overlay first, exactly as the palette hides itself first.
- **Demos.** A screen recorder that honours the flag will not show the labels,
  which defeats the #332 demo GIF. A config field `overlay.capturable` (default
  `false`, documented as "for recording a demo") skips the flag, and the
  gallery sets it so overlay states can be screenshotted.

## When the screen changes

An annotation is only true for the pixels it was found in. If those pixels move,
the overlay lies. `SetWinEventHook` and the existing mouse and keyboard hooks
make the check event-driven, installed **only while an overlay is visible** and
removed on every hide path. No polling timer, no `timeBeginPeriod` (rule 5).

### What fires and what does not

This matters, because #325's "scrolling hides it" is not covered by the hook it
names:

- `EVENT_OBJECT_LOCATIONCHANGE` on a window object (`idObject ==
  OBJID_WINDOW`) fires when a **window** moves, resizes, maximises or restores.
  It also fires continuously for the caret and the cursor (`OBJID_CARET`,
  `OBJID_CURSOR`), system wide.
- **Scrolling inside a window does not move the window.** A native scrollbar
  fires location events for its own child objects, but a browser, Electron app
  or PDF viewer renders its own content and fires nothing unless an
  accessibility client has asked for it. So the hook alone cannot see a page
  scroll. THEORY (unverified) for each app; Manual check 3.
- A browser **tab switch** or an in-app navigation changes the content with no
  window event at all.

So the staleness rules are the union of events plus the input that causes them:

| Trigger | Source | Hides the overlay when |
|---|---|---|
| Window moved, resized, minimised, restored | `EVENT_OBJECT_LOCATIONCHANGE` (`OBJID_WINDOW`) | the window is in the target set |
| Window destroyed | `EVENT_OBJECT_DESTROY` (`OBJID_WINDOW`) | the window is in the target set |
| Minimised | `EVENT_SYSTEM_MINIMIZESTART` | the window is in the target set |
| Another app comes forward (including Alt+Tab) | `EVENT_SYSTEM_FOREGROUND` | the new foreground window is **not** in the target set and **not** one of Wingman's own windows |
| Wheel | `WH_MOUSE_LL` (existing watcher, `WM_MOUSEWHEEL`/`WM_MOUSEHWHEEL` added while armed) | the pointer is over a target window |
| Click not on a label | the same watcher | the click lands anywhere (a tab, a scrollbar, a link all change content) |
| A key that can scroll or edit | `WH_KEYBOARD_LL` (existing) | any non-modifier keydown while the foreground window is in the target set |
| Age | one `SetTimer` that fires once, killed on hide | 30 s (`overlay.auto_dismiss_secs`) after the overlay appeared |

### Where each decision is made

- **The hook callbacks never decide.** `GetForegroundWindow`, `WindowFromPoint`
  and set lookups inside a `WH_KEYBOARD_LL` or `WH_MOUSE_LL` callback risk the
  hook timeout, and Windows unhooks a hook that is too slow. The callbacks
  only post a message (`WM_APP_OVERLAY_INPUT`, carrying the kind of input and,
  for the mouse, the point). The main thread then asks the pure `state` machine
  whether it hides the overlay. The overlay therefore hides a few
  milliseconds after the input reached the app, which is fine: it is a
  staleness guard, not a gate. The one exception is the click on a label, which
  is a hit-test against a snapshot of the label rectangles held in an atomic
  read-only structure, so a click that lands on a label is never dismissed at
  button-down before the label's own handler sees it.
- **The `SetWinEventHook` callbacks run out of context** (`WINEVENT_OUTOFCONTEXT`)
  on the main thread's message loop, so they are not under the hook timeout, but
  a location event fires for the caret and the cursor constantly. To keep that
  cheap: install the narrow ranges only (one hook for `LOCATIONCHANGE` per target
  process, `idProcess` set to the process of each target window; one each for
  `DESTROY`, `MINIMIZESTART` and `FOREGROUND`), use `WINEVENT_SKIPOWNPROCESS`,
  and make the callback's first test `idObject == OBJID_WINDOW` and `hwnd` in the
  target set, before anything else. A typing user hides the overlay on the first
  key, so any caret event flood is bounded to one.
- **A single hide function** removes the hooks, kills the age timer, clears the
  Esc claim and frees the bitmap, and every hide path (key, Esc, click, event,
  age, owner card gone) calls only it. A unit test over the pure state asserts
  the registry of installed hooks and timers is empty after each path.
- The one-shot age timer is not polling: it is set once, never rearmed, and is
  the same mechanism as the card's `auto_dismiss_secs`. It bounds the residual
  case the table cannot see (a chat that keeps appending, a page that rewrites
  itself while the user watches).

### Notes

- "Not one of Wingman's own windows" matters for #329. A form preview card takes
  focus, so the target legitimately loses the foreground, and ghost text must
  stay. The same filter applies to keys: a key typed into Wingman's own preview
  card (Enter, Esc, Tab) is not a change to the target.
- **Alt+Tab to another app hides the overlay and it is not restored when the user
  returns.** A preview card (options card spec) survives Alt+Tab with its edits;
  the ghost text it annotated does not. The preview card stays the source of
  truth, and a new press redraws.
- The target set is **every top-level window under an annotation's rect**,
  found with `WindowFromPoint` + `GetAncestor(GA_ROOT)` at the rect's centre
  when the annotation is resolved, normally one window, two for #337 (terminal
  and editor). Wingman's own windows are never in it.
- **Fail safe.** If a hook cannot be installed (it returns null), the overlay is
  not shown and the summary card carries the result. An overlay with no way to
  go stale is the one outcome to avoid.

### Why not follow the window

Tracking means re-resolving text positions after every scroll, which means a
fresh capture and OCR (0.2 to 0.3 s) per event, or UIA calls into another
process on each event. Both are work done while the user does something else,
and both can show a label on the wrong word during the catch-up. Moving the
whole overlay by the window's delta on a drag would be cheap and correct for a
pure move, but a move is the rare event; a scroll is the common one, and it
cannot be followed. **Recommendation: dismiss on change, no tracking.** One
press redraws.

## The anchor model

### Types

```rust
/// What the model (or a detector) says to point at. Never coordinates.
pub struct AnchorRef { pub text: String, pub occurrence: u32 }   // 1-based

pub enum Located {
    Found(Vec<ScreenRect>),     // one rect per line fragment
    NotFound,                   // no match; drop the annotation
    Ambiguous,                  // see below
}

pub struct Annotation {
    pub rects: Vec<ScreenRect>,        // where the anchored text is
    pub kind: AnnotationKind,
    pub label: String,                 // already sanitised and clamped
    pub action: Option<AnnotationAction>,  // #326 only
}

pub enum AnnotationKind { Note, Ok, Wrong, Unsure, Translation, Ghost, Skipped }
pub enum AnnotationAction { Copy(String), ReplaceText { expected: String, replacement: String }, AddToCalendar(..) }
```

Pure code in a new `src/overlay/` (or `src/annotate/`) module: `anchor.rs`
(matching), `layout.rs` (placement) and `state.rs` (the staleness state
machine). Only `ui/overlay.rs` touches Win32.

### Three anchor sources

1. **OCR word boxes** (`ocr.rs`). Universal: web pages, PDFs, images, games,
   remote desktops, anything that is pixels. This is the default for every
   read-only overlay.
2. **UIA text ranges** (`TextPattern`, then `GetBoundingRectangles`). Exact,
   no recognition errors, works for text that is off-screen-adjacent or
   tiny, and is the only source that can also *write* (#326's fix in place goes
   through the existing `replace_text` executor, which already needs a UIA
   range). Only where the app implements `TextPattern`: Notepad, Word,
   most WinForms/WPF edits. THEORY (unverified): browsers and Electron expose
   `TextPattern` on documents only when accessibility is active, and whether
   `GetBoundingRectangles` is complete there is unknown (Manual check 4).
3. **UIA field rectangles** (`FieldSnapshot.rect`). No matching at all: the
   field is the anchor (#329). Already read by `inputs::uia`.

### Matching quotes to OCR words (the pure core)

Input: the OCR lines of the capture, an `AnchorRef`. Output: `Located`.

1. **Flatten.** Join all words in reading order with single spaces, remembering
   for each word its line, its rect and its byte range (a cross-line version
   of `redact::word_byte_ranges`).
2. **Normalise** both sides the same way, with no new dependency: lowercase
   (Unicode simple case), collapse whitespace, map typographic quotes and
   dashes to ASCII, drop zero-width characters and soft hyphens, strip a
   trailing hyphen at a line break so "recei-" + "ve" can join.
3. **Exact pass.** Find the n-th (`occurrence`) match of the normalised quote on
   word boundaries. A match may span lines; it yields one rect per line
   fragment (the union of that line's words in the match), never one rect that
   covers the gap between lines.
4. **Fuzzy pass, only if the exact pass found nothing.** A quote of at least
   4 characters may match a window of the same word count with edit distance at
   most 1 per 8 characters (so `rn`/`m`, `l`/`1`, `O`/`0` OCR misreads survive).
   It is accepted only if exactly one window qualifies. Two or more is
   `Ambiguous`.
5. **Occurrence.** If `occurrence` exceeds the number of matches, the result is
   `NotFound` (never "the last one"). `occurrence` 0 or missing is read as 1.
   Models count occurrences badly, so the prompt also asks for enough context
   in the quote to make it unique, and a quote that is unique on screen ignores
   `occurrence`.
6. **Guard rails.** A quote shorter than 2 characters, longer than 400, or
   containing a redaction marker (`[card number 1]`, see Privacy) is
   `NotFound`. No indexing or slicing on untrusted text: char boundaries
   throughout (rule: release builds abort on panic).
7. **To screen.** `screen = origin + image * scale` from `RawShot.screen`, rounded
   outward, then clipped to the capture's monitor. A rect that clips to nothing
   is `NotFound`.

All of this is a function over `&[OcrLine]` and strings, tested with synthetic
lines, no Win32.

### Prompt side: quote from the text

The model sees a downscaled screenshot (`fit_for_model`), and may write "recieve"
when OCR read "recieve." or `1` where OCR read `l`. To make exact matches the
norm, a model-backed overlay **includes the numbered OCR lines in the prompt**
(`1: ...`, `2: ...`) and asks the model to quote a span verbatim from them. The
text is built through the existing text path (`Redactions::redact_text`) so it
carries the same markers and rules as any other screen text sent off the
machine. The line numbers are a hint the matcher uses to prefer a match on
that line; they are never trusted alone. Cost: about 10 tokens per line, a
dense screen is 60 lines, so roughly 600 more input tokens (THEORY, unverified,
a count on a real capture decides). Without a usable text path (OCR failed) the
action falls back to the card.

### What happens when anchoring misses

- One annotation not found: dropped. The summary card still lists it as text
  (the user loses the mark, not the finding). The card says "3 marked, 1 could
  not be placed".
- None found, or OCR failed: summary card only. This is a normal outcome, not
  an error card, because the answer is still there. OCR failing outright is
  an error card as today (rule 7).
- More than 20 found: the first 20 in reading order, and the card says how many
  were left out (`overlay.max_annotations`, default 20, clamp 1 to 40).

## #383: how to locate text, and what is still owed

The spike compares Windows OCR boxes, Florence-2, PaddleOCR-VL and
Qwen3-VL grounding. **Recommendation: Windows OCR word boxes plus local text
matching is the default and ships first; UIA ranges are the exact source where
an app offers them; no grounding model is adopted.** What the code and docs
already show:

| | Windows OCR + matching | UIA `TextPattern` | Model grounding |
|---|---|---|---|
| Already in the repo | yes, word rects at full resolution | field rects yes, text range rects no | no |
| New dependency | none | none | an ONNX or other runtime, a model download, a licence to check |
| Cost | MEASURED 2026-09-17: 22 to 36 ms on a synthetic image. MEASURED 2026-10-15: 204 to 329 ms on a real 1402x876 capture, debug build, cold first | per call into another process, can take seconds in a hung app | seconds to tens of seconds on this CPU/iGPU (THEORY, unverified) |
| Works offline | yes | yes | only if the model is local |
| Known failure | MEASURED 2026-10-15 (`redact.rs`): no line at all returned for a rendered "Card 4111 1111 1111 1111", though a different number read fine, so digit runs can be missed; handwriting weak (THEORY, unverified) | browsers and Electron (THEORY, unverified) | box accuracy on small text (THEORY, unverified, why #325 keeps pixel coordinates out of the model's hands) |
| Can write | no | yes | no |

Why the default holds without more measurement:

- The anchor needs a *word box for text we can name*, and Windows OCR returns
  exactly that. A grounding model solves a harder problem (find something
  describable) than the one posed (find these words).
- The exe gate is 3,900,000 bytes with 3,800,576 measured (perf-budgets.json;
  AGENTS.md's about 3.5 MB and the spike text's about 2 MB are both out of date),
  and any model runtime must stay optional. Adopting a
  runtime for the sake of a fallback is a large cost for the cases it rescues.
- One OCR pass can serve both redaction (`redact::apply_lines`) and anchoring,
  so a cloud overlay pays for OCR once.
- Florence-2 and PaddleOCR-VL are licence-clean (MIT, Apache-2.0) but need an
  ONNX path that is not in the dependency list; Qwen3-VL's licence is unverified;
  Moondream 3 is excluded by rule 2 (BSL).

What this does not settle, and the measurement that would:

1. **Handwriting, dark mode, small UI text, charts.** Where does Windows OCR
   fail, and does a grounding model do better enough to justify a runtime? The
   spike's 20-fixture set, scored by **anchor hit rate** (the share of model
   quotes that resolve to the intended word box) as well as IoU, because hit
   rate is what the user sees. A fixture set under `tests/fixtures/overlay/`
   with hand-labelled expected boxes also becomes the regression test for the
   matcher.
2. **UIA `GetBoundingRectangles` on Chromium and Electron**, with and without
   an accessibility client present, for scrolled and wrapped text. A throwaway
   ignored test that prints counts and rect sizes only (never text), run against
   Notepad, Word, Edge and VS Code.
3. **Full-resolution OCR cost** on a 4K capture in a release build (the figures
   above are 1402 wide and debug).

If (1) shows handwriting anchoring below an acceptable hit rate, #332 keeps the
card fallback for handwritten work and the spike stays open for that case only.
Nothing in slices 1 to 5 waits for it.

## The typed proposals

### Where the overlay hangs off the action model

A new `Action` field, `output = "card" | "overlay"` (default `"card"`, so every
existing action is unchanged). `"overlay"` means the proposal is mapped to
annotations after the model replies, and a summary card is shown. The executor
stays `"none"` for read-only actions.

**A deterministic overlay (units, relative dates) has no proposal and no
model.** A second new field, `detector = "units" | "dates" | ..`, names a
built-in detector over the OCR text, and is accepted only on built-in actions
(a user-authored `actions.toml` entry cannot name one). Such an action declares
`inputs = ["screen"]`, `output = "overlay"`, a `detector` and no `prompt`.
The generic runner (`run_generic_action`) needs a new model-free branch for
it: capture on the main thread, OCR on a worker, run the detector, show the
overlay, with no provider, no readiness gate and no `Mode` consulted (the same
shape as `run_model_free_screen_action`), and Paused as the only gate. Both
fields and the runner branch land in slice 2, with a debug-only action as the
first user, so slice 3 only adds a detector.

**How a user invokes a model-free overlay.** The router needs a model to offer
an action, so it cannot be the only door. A model-free overlay action is a
catalogue entry like "Copy text from screen": it is a palette row, a tray item,
and, because it is model-free, it appears in the options card's static floor
(no model, or Offline). With a model ready the router may also offer it.

`actions.toml` entries opt in to `output = "overlay"` with the generic list
below, so a contributor can add an annotating action without touching Rust.

### The generic schema, `annotation_list`

```json
{
  "type": "object",
  "properties": {
    "annotations": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "anchor_text": { "type": "string" },
          "occurrence":  { "type": "integer" },
          "kind":        { "type": "string", "enum": ["note", "ok", "wrong", "unsure"] },
          "label":       { "type": "string" }
        },
        "required": ["anchor_text", "occurrence", "kind", "label"],
        "additionalProperties": false
      }
    },
    "summary": { "type": "string" }
  },
  "required": ["annotations", "summary"],
  "additionalProperties": false
}
```

**Property order is load-bearing** (AGENTS.md rule 3, `preserve_order`):

- Within an item, `anchor_text` first: the model quotes before it judges, so it
  commits to *which* text it is talking about. `kind` before `label`: it picks
  the judgement before it writes the note that has to agree with it.
- Top level, `annotations` before `summary`: the summary is written after the
  work, not before.
- A unit test asserts the key order of every schema built, as the router and
  `verdict` tests do, so a refactor cannot reorder it silently.

No `minItems`/`maxItems`: providers differ on array keywords, and a prompt-only
limit is not trusted (#301). Caps are enforced in code.

### Domain schemas, one per issue

Each is its own proposal kind in `schema_for`, with a pure
`to_annotations(&Value, &[OcrLine], &ScreenMap) -> Vec<Annotation>` beside it.

| Issue | Kind | Shape (in property order) |
|---|---|---|
| #332 Check my work | `verdict` gains optional `steps` | `steps: [{ anchor_text, occurrence, status: ok / wrong / unsure, note }]`, **then** `detail`, `headline` (and `difficulty`) |
| #333 translate in place | `translation_lines` | `source_language`, `lines: [{ anchor_text, occurrence, translation }]` |
| #334 scam evidence | `scam_check` `signs` items gain optional anchors | `signs: [{ anchor_text, occurrence, sign }]` then `verdict`. An empty `anchor_text` means card only |
| #335 jargon | `term_list` | `terms: [{ anchor_text, occurrence, meaning }]` |
| #337 stack trace | `error_location` | `file`, `line`, `explanation`, `fix` (the model names the line; Rust finds it, see below) |
| #329 form fill | the existing `form_fill` proposal | no change: the field is the anchor |
| #336, units, #315 | none | deterministic detectors |

On #332: the issue says "keeping `headline` first". The code does the opposite,
and for a reason: `verdict` requires `["detail", "headline"]` so the working
comes before the verdict. `steps` is more working, so it goes **before** both.
The headline is then a conclusion over the marked lines, not a guess the lines
have to agree with.

### What the code does with model text

Every `label`, `note`, `meaning` and `translation` is untrusted:

- `sanitize_model_text`, markdown stripped, an em dash replaced by a colon and a
  space (rule 11), whitespace collapsed.
- Clamped **in code** on a char boundary: labels 80 characters (the #335
  figure), notes 120, translations by the box they replace.
- An unknown `kind` becomes `note`. An empty label falls back to the action
  name.
- `translation` text is drawn, never executed or put on the clipboard unless a
  #326 `Copy` action is clicked.

### #337: finding the line deterministically

The model returns `{ file, line, explanation, fix }`. Wingman finds the line on
screen by matching OCR text, with no model coordinates: locate the file name in
the editor's tab or title (an `AnchorRef` on the name), then find the gutter
numbers, which are the left-most column of short digit-only words sharing an x
range, and pick the word equal to `line`. If the gutter cannot be read, or that
line is not visible, the card says "line 42 of parser.rs is not on screen" and
nothing is guessed. The target set here is two windows (terminal and editor).

### #333: lines, not words

Each OCR **line** is an anchor, and the translation is drawn in a box the size of
the line's rect, with the font scaled down to fit (never up past the original
height) on an opaque background. Lines are batched into one proposal; the cap of
20 annotations does not apply (a page can be 60 lines), a separate
`overlay.max_translation_lines` (default 80) does, and a model that returns
fewer lines than sent leaves those lines unlabelled. When Windows OCR does
not read the source script (no pack installed, `ocr::missing_pack_message`),
the card explains the language pack, not an empty overlay. A cloud call for
this sends the whole screen text, so the redaction rules above apply in full.

## Layout (pure)

`layout.rs` turns annotations into drawn pieces and is unit-tested:

1. Candidate positions for a label, in order: above the anchor, right of it,
   below it, left of it. Never over the anchor's own rects (#325 rule).
2. A candidate is taken if it is inside the monitor, does not hit another
   annotation's anchor, and does not overlap a label already placed.
3. If none fits, the annotation is dropped from the screen and counted for the
   card.
4. Marks use the anchor rect directly: an underline is a line under the rect,
   a wrong-mark is a boxed underline with the "x" glyph, an ok-mark a tick
   to the left of the line.
5. Ghost text (#329) is drawn inside the field rect, clipped to it, in the muted
   text colour on a field-coloured pill. "Skipped" tags (payment, password,
   #263, #267) sit at the field's right edge.

## Theme, high contrast and accessibility

- **Tokens.** Labels use the existing `surface`, `text` and `border` tokens.
  Three more tokens, `mark_wrong`, `mark_ok`, `mark_note`, join `ui/theme.rs`
  with the same contrast tests as the other tokens (at least 3:1 against both
  the surface colour and the pill's own background in light and dark), plus the
  halo colour for marks drawn straight on the page.
- **Never colour alone.** Wrong is a boxed underline with an "x", ok a tick,
  unsure a dotted underline with "?", so the marks read for colour-blind users
  and in high contrast.
- **High contrast.** System colours (`COLOR_WINDOW`, `COLOR_WINDOWTEXT`,
  `COLOR_HIGHLIGHT`), 2-pixel borders, 3-pixel marks, no soft edges, as the card
  does.
- **Text scale and DPI** as above. The overlay does not animate, so there is
  nothing to reduce.
- **Screen readers and keyboard.** A layered window with no focus is not usefully
  in the UIA tree, so the overlay is not the interface. The summary card carries
  the result as text, and its appearing raises a system alert event named by the
  summary (full UIA is #119). The card's collapsed line gives the counts ("3
  marked, 1 could not be placed"); its expanded detail lists every annotation in
  reading order, and that list is scrollable, so a #333 translation of 80 lines
  fits (the collapsed card never lists them all). Every #326 action (Fix, Copy,
  Add to calendar, Fix all) is also a button or a list entry on the card,
  reachable with the keyboard (#119, #606). Esc closes. This is why the summary
  card is mandatory.
- **Do not trap keys.** The overlay claims Esc only, only with no modifiers,
  only while showing, and with an expiry that lapses by itself (see "User-visible
  behaviour"), through the same lock-free claim and pure "which keys are
  claimed" function the options card uses.

## Privacy

- **Redaction first.** A model-backed overlay uploads the same redacted
  screenshot and the same redacted OCR text as any other action. If redaction
  or OCR cannot run, the request fails closed with the existing card and
  nothing is sent (fail-closed already the policy in the redaction spec).
- **Local detectors upload nothing.** Units and relative dates run on local OCR
  text; no key, no provider, no egress row.
- **No echo of redacted text.** A quote containing a marker is dropped; a label
  never restores a marker to the original. The model cannot point at what it
  never saw.
- **Not in the log.** The egress log and diagnostics never store annotation text
  (the log records counts, per the redaction spec). Annotation counts are
  fine; labels are not logged.
- **Memory.** The OCR lines and annotations live only for the overlay's lifetime
  and are dropped on hide.
- **Off-screen leakage.** `WDA_EXCLUDEFROMCAPTURE` also keeps labels out of the
  user's screen share, which is the safe default for translations and
  explanations of private text.
- **Never a final button.** The overlay never offers to click or open anything
  on the underlying page. #334 explicitly: nothing on the scam overlay is
  clickable, and a flagged link is never opened.
- **When a guard cannot run** (affinity call fails, hooks cannot install, OCR
  times out), the safe outcome is *no overlay*, and the card carries the answer.

## #326: clickable labels

Slice 7, built after the read-only slices have proven the window. **It does not
start until open question 13 is answered**, because it can write without an
Enter confirm, which the product rule (Look, Propose, Confirm, Do) does not
allow as drafted.

As #326 is written, a click on a label writes the fix at once: "the label is the
preview, the click is the confirmation". The options card spec states that every
writing option goes through the preview card and waits for Enter, so this is an
exception to a fixed rule, not a design detail. Two further risks: labels sit
over neighbouring text, so an accidental click on one is a write; and a click is
easy to land on a label that moved.

**Recommended design: Enter confirm.** A click on a label is the Propose step: it
opens the existing preview card for exactly that change (the old and new text,
Do it focused), and Enter or the button is the Confirm. Preview equals
execution still holds, one extra keypress. If open question 13 is answered the
other way, the single click write ships only with every guard below plus the
rule documented as an exception in AGENTS.md.

What #326 adds, under the executor rules either way:

- **Preview equals execution.** The label is the exact change ("recieve ->
  receive" drawn as `recieve` with `receive` above), and what is written is
  exactly that string. A Fix all button is the only multi-write, and it is shown
  as a list on the summary card first.
- **Only through UIA.** A fix-in-place requires an anchor from a UIA
  `TextPattern` range and goes through the existing `replace_text` executor
  and its guards (`uia_guard`, the payment and password denylists, the
  `IsPassword` fail-closed check). OCR-anchored text offers **Copy** instead.
- **Staleness check at write.** Re-read the range and write only if it still says
  `recieve`; otherwise a card says it changed (the #266 bug class). The overlay
  was already hidden by any edit through the staleness table, so this is the
  second line.
- **Undo** is the target app's own Ctrl+Z, said on the result card.
- Clicking a label never activates Wingman, so the target stays foreground for
  the write.
- **The click handler hit-tests the label rectangles before the dismiss rule
  runs**, so a click on a label is a pick, not a dismissal (see the shared rules).

## #329: ghost text in form fields

Uses `FieldSnapshot.rect` (no OCR) and the existing fill executor's resolved
field list, so the ghost value is the exact string the executor will write for
an unedited field, including values a refusal rule changed. Fields it will
skip show "skipped". The form preview card still takes Enter and Esc and holds
focus; the overlay stays up while the card has the foreground (the "not one of
Wingman's own windows" filter) and goes on any change to the form's own window,
and on Alt+Tab to another app (not restored). A field whose rect is empty,
off-screen or clipped is not drawn and is listed on the card only.

**Editing a value in the preview card hides that field's ghost text.** The
preview's fields are editable, so after an edit the ghost would show a string
the executor will not write. The card posts "field edited" with the field
index, and the overlay drops that one ghost (it does not try to mirror the
edit; the card is the source of truth). Updating the ghost live is a later
refinement, not part of this slice.

Key, Esc and click-away follow the shared rules: with the overlay and the
preview card up together, the key dismisses the preview, nothing runs, and the
ghost text goes with it. This slice depends only on slice 2, not on the matcher,
and validates the field anchor source early.

## Fit with the options card (PR #661)

- **Separate window, same rules.** The overlay is not a `CardState` and not a
  `CardKind`. It is its own window the `App` owns, with the summary card as an
  ordinary answer card beside it. The options card is untouched. The shared key,
  Esc and click-away rules live in the options card spec ("Shared rules for every
  surface"); this spec does not restate or differ from them.
- **Precedence.** Highest surface first: preview, options card, answer card
  (with its overlay). With the #329 overlay and a preview card showing, the key
  dismisses the preview and the ghost text goes with it.
- **An option can start an overlay action.** "Mark the errors on screen" is a
  catalogue action like any other, so the router can offer it and
  `dispatch_palette_action` runs it. The options card hides first, before the
  capture, as it already does, so neither card is in the OCR.
- **Click-away.** The overlay always hides on a click that is not on a label,
  whatever the setting, because a click on a tab, a link or a scrollbar changes
  the content and a stale overlay would lie. Its summary card follows
  `options.click_away_dismiss` (default true) like any answer card. If the card
  stays (setting off), its line is rewritten to "Marks hidden, the screen
  changed. Press the key to mark again" and the findings text stays, so the card
  never claims marks that are gone and the accessibility floor is kept. The same
  rewrite applies when any other staleness row hides the overlay.
- **Shared guards.** The auto-repeat guard on the key and the claim (an expiry
  tick, no modifiers, cleared on every hide path) are shared with the options
  card; this spec adds an Esc-only state to the pure function rather than a
  second mechanism.
- **New `WM_APP_*` ids** (`WM_APP_OVERLAY_CLICK`, `WM_APP_OVERLAY_STALE`,
  `WM_APP_OVERLAY_INPUT`) join `ALL_WM_APP_IDS` in `app.rs` (the registry test
  enforces it).

## Config

```toml
[overlay]
max_annotations = 20            # clamp 1 to 40
max_translation_lines = 80      # #333
auto_dismiss_secs = 30          # clamp 5 to 300
capturable = false              # true only to record a demo
```

No master switch: an action that does not declare `output = "overlay"` never
draws one, and Settings' action toggles already turn an action off. A Settings
row for `capturable` is not planned; it is a config-file and gallery knob.

## Testing

Pure logic is unit-tested (rule 8); each Win32 piece names its observable.

**Pure, `cargo test overlay`:**

- `anchor`: exact match, second occurrence, occurrence past the end is
  `NotFound`, a match across a line break gives two rects, hyphenated line
  break, quote with a typographic quote or dash, case difference, fuzzy match
  unique vs `Ambiguous`, quote too short or too long, quote with a redaction
  marker, a quote that is a multibyte string (char boundaries), empty OCR,
  words that disagree with the line text.
- Coordinate mapping with a scaled `ScreenMap`, a negative origin (monitor left
  of the primary), clipping to nothing, rounding outward.
- Schema key order for `annotation_list` and each domain kind.
- `to_annotations` on canned model JSON: unknown `kind`, empty label, over-long
  label, em dash in a label, more than the cap, one anchor missing.
- `layout`: label above, falls right when above is off-screen, avoids another
  label, drops when nothing fits, never covers its anchor, stays inside the
  monitor.
- `state`: every row of the staleness table against a fake event stream, "own
  window" foreground does not hide, a key typed into a Wingman card does not
  hide, hook install failure refuses to show, and **no hook or timer remains
  after any hide path** (the registry of installed hooks is empty).
- The alpha pass: every pixel inside a pill is opaque and premultiplied, pixels
  outside are untouched.
- The deterministic detectors: unit conversion over OCR text (`5 km` to `3.1 mi`),
  and the relative-date resolver pinned to fixed reference dates, including a
  month and a year boundary and an ambiguous "next Friday" on a Thursday (#336).
- A source-scanning guard that no `SetTimer` with a repeating interval exists in
  `ui/overlay.rs`, normalising `\r` and asserting its candidate set is not empty
  (the CRLF trap).
- The main-thread decision path: a posted `WM_APP_OVERLAY_INPUT` for each row of
  the staleness table hides or keeps the overlay, a click on a label rectangle is
  never dismissed, and a source-scanning guard (CRLF-normalised, candidate set
  asserted non-empty) that no hook callback in `ui/overlay.rs` calls
  `GetForegroundWindow` or `WindowFromPoint`.
- Precedence: overlay with a preview card, key dismisses the preview and the
  overlay together; overlay with `click_away_dismiss` off stays with its card.
- The Esc claim: only with no modifiers, only while an overlay shows, lapses
  at its expiry tick, cleared by the single hide function.
- Ghost text: an edited field's ghost is dropped, the others stay.
- Source-scan: no `allow(dead_code)` remains under `src/overlay/` after slice 2.

**Gallery** (`ui/gallery.rs`): new states `overlay-notes`, `overlay-wrong-ok`,
`overlay-ghost`, `overlay-clickable`, `overlay-translation`, each in light,
dark and high contrast, over a GDI-drawn fixture page (the gallery sets
`capturable`, or the screenshot would be empty). This is where contrast and
placement are looked at.

**Fixtures.** `tests/fixtures/overlay/`: a screenshot with "Thu 9:00 PT" and
"5 km", a typed worked problem with one wrong line (#332), a fake invoice (#334,
shared with #312), a finance paragraph (#335), a French paragraph (#333), a
compiler error beside a numbered source file (#337). The matcher is run
against the **real** OCR in an `#[ignore]`d live test (the pattern of
`ocr_live_*`), printing hit counts only.

**Manual checks** (named, each differs if the feature is wired to nothing):

1. Press the key with an overlay showing, then read the saved capture: it
   contains no overlay pixels. *Without the affinity flag the labels are in the
   capture.* Prerequisite for slice 2.
2. In Notepad with a clickable label, a click on empty space reaches Notepad,
   and a click on the label does not move the caret or the foreground.
3. For Edge, VS Code, Word and Notepad: scroll with the wheel, the scrollbar,
   the keyboard, and drag-move the window. The overlay hides each time. *If a
   case does not hide, that app needs its own row in the staleness table.*
4. `GetBoundingRectangles` on Notepad, Word, Edge and VS Code (#383).
5. Two monitors at different scales: a label sits beside its word on each, at
   the right size.
6. With the overlay showing and no input, idle CPU and wakeups are zero
   (Process Explorer or `powercfg`), and after hide the hooks are gone.
7. With the form preview showing ghost text, Alt+Tab away and back: the ghost
   text is gone and the preview card is intact with its edits. Observable: the
   field edits survive, the ghost does not reappear.
8. Narrator: the summary card appearing is announced, and its expanded list
   reads every annotation.

## Costs

- **Deterministic overlay (units, dates):** capture, one OCR pass (0.2 to 0.3 s
  MEASURED on a real capture, see the table; full-resolution release cost is not
  measured), pure detection, one window. No network, no billing.
- **Model-backed overlay:** the action's usual vision call, plus about 600 input
  tokens of OCR text (THEORY, unverified, a real count decides) and up to about
  20 short annotations out (cap `max_tokens` with the proposal's size, and
  decide the figure when the first kind is built). With redaction on, the OCR
  pass is shared, not repeated.
- **Overlay itself:** a bitmap the size of the bounding box of the marks,
  freed on hide, no work while visible, four event hooks and one one-shot
  timer, all removed on hide.

## Slices

Each ships alone and has an observable.

0. **Spike, before approval (no product change).** The throwaway program from
   "Excluding the overlay from capture": a layered window with
   `WDA_EXCLUDEFROMCAPTURE`, captured through `xcap` as Wingman uses it, through
   a `wgc` or DXGI capture, and through `PrintWindow`, with and without the flag.
   Written into this spec. Gate: if the flag does not hold for a layered window,
   this design is revised before slice 1 starts. Cheap enough to do now rather
   than after merge.
1. **Anchor core and layout (pure).** `anchor`, `layout`, the alpha pass,
   `state`, the shared span helpers extracted from `redact.rs`. It has no
   runtime caller until slice 2, so the new modules carry
   `#[cfg_attr(not(test), allow(dead_code))]`, and slice 2's done criterion is
   that no `allow(dead_code)` remains under `src/overlay/` (a source-scan test,
   CRLF-normalised, candidate set asserted non-empty). The extracted `redact`
   helpers do have a caller from the first commit. Observable: the unit tests,
   and `redact` still passes after the extraction. Nothing user-visible.
2. **The window, and one real annotation.** `ui/overlay.rs`, the hooks and the
   main-thread decision path, the affinity flag and its fail-closed rule, the
   gallery states, `Action.output` (default `"card"`) and `Action.detector`,
   the generic runner's model-free overlay branch, and a debug-only action that
   marks a word. Observable: Manual checks 1, 3, 6 and 7. Gate: Manual check 1
   must pass or this design is wrong and is revised.
3. **Units overlay (#325's first user).** `5 km` gets `3.1 mi`, `72 F` gets
   `22 C`, via `calc/units.rs` and `detector = "units"`. No model, works
   offline, reachable from the palette, the tray and the options card's static
   floor. Currency is left out: it needs a rate source, which is a network call
   and a privacy decision of its own.
4. **Dates (#336, then #315).** The relative-date resolver (pure, tested against
   fixed dates), then the time-zone labels from the existing `time_zones`
   conversion.
5. **#329 ghost text.** Field anchors only. Validates the "own window" filter,
   the edit-hides-ghost rule and the Enter/Esc interplay with the preview card.
6. **Model-backed read-only marks**, in this order: #334 (smallest, `signs[]`
   gains an anchor, nothing clickable), #332 (the demo; needs the handwriting
   fallback), #335 (needs the code clamp), #337 (needs the gutter matcher and a
   two-window target set), #333 (heaviest: line boxes, font fitting, language
   packs, a local model that translates well).
7. **#326 clickable.** UIA text anchors, `Copy`, `replace_text`, `Add to
   calendar`, Fix all on the card. Last, because it is the only slice that
   writes, and it waits for open question 13.

#383's measurements run alongside slices 1 to 3 and feed the hit-rate figure
slice 6 needs.

### Size budget

The gate is `perf-budgets.json` `exe_size_bytes.max` 3,900,000 against a last
measurement of 3,800,576, about 99 KB of headroom, shared with the options card,
Settings dark mode and knowledge specs. Estimates for this spec, each THEORY
(unverified) until a CI measurement replaces it: slice 1 about 12 KB (matcher,
layout, edit distance, state), slice 2 about 25 KB (window, three hook kinds,
layered bitmap, the runner branch), slice 3 about 8 KB, slice 4 about 12 KB,
slice 5 about 6 KB, slice 6 about 30 KB across the five proposal kinds, the
gutter matcher and font fitting, slice 7 about 10 KB. That is 60 to 120 KB in
all, so **this spec cannot be absorbed by the existing headroom alone**: the
pull request that crosses the gate raises it with the measured size and the
reason, as the QR decoder and the live model lists did, and the order the
specs merge in decides which one does. No new crate is planned (a `windows`
feature may be: `Win32_UI_Accessibility` for the event hook is already on). Each
slice's PR records the CI-measured size.

## What this deliberately does not do

- Follow a window, scroll or tab. (Dismiss on change.)
- Show an overlay on its own. Triggers (#331) and Watch mode (#330) may later draw
  the deterministic ones automatically, but only after a Watch spec decides how,
  and the same window and staleness rules would apply.
- Offer chat, follow-ups or editing of an annotation in place. Editing before
  confirming is #326's later work on the card.
- Adopt a grounding model. See #383 above.
- Convert currency.
- Draw over exclusive-fullscreen apps.

## Open questions

Each with a recommended answer. Where a counter-argument exists it is stated, so
the question stays open.

1. **One window per monitor sized to the marks' bounding box, or one per
   annotation?** One per monitor. Per-annotation windows make z-order and
   hit-testing simple but cost 20 window creations and make "the overlay" harder
   to hide atomically.
2. **Put the OCR text in the model prompt?** Yes for text-anchored model
   overlays, redacted through the existing text path. It turns most matches
   into exact matches. The token count must be measured on real captures before
   slice 6; revisit if it is larger than 600.
3. **Does the display affinity flag hold for a layered window under `xcap`?**
   Unknown, and the whole design hangs on it, so it is the Slice 0 spike, run
   before this spec is approved rather than before slice 2 merges. If it does
   not hold, hide the overlay before every Wingman capture and treat awareness
   capture (#330) as blocked on a different exclusion.
4. **Any key dismisses, or only keys that can change content?** Any non-modifier
   key while the foreground window is in the target set, decided on the main
   thread, never inside the hook callback. A narrower list would miss an app's
   own shortcuts. The cost is that a stray key press hides the overlay, which is
   the safe direction.
5. **How long is the age cap?** 30 s. A person reads a screen of labels in
   seconds. The cap exists for the pages that change under the user. A one-shot
   timer set at show and killed in the single hide function is the same pattern
   as the card's `auto_dismiss_secs`, not a polling timer.
6. **Should the overlay honour `options.click_away_dismiss`?** Revised answer: the
   overlay never does, its card does. The overlay always hides on a click not on a
   label (the staleness table already says so), and the card follows the setting;
   if the card stays it says the marks are hidden. Counter-argument for one
   switch governing both: a user who turned click-away off wants a persistent
   card, and a card that edits its own line is surprising. Recommended: keep the
   two separate, because a stale overlay is a correctness problem and a stale
   card line is not.7. **Fuzzy matching in the first slice?** Yes, with the unique-match rule. OCR
   misreads (`l`/`1`, `rn`/`m`) are the common failure and a model quoting
   from the image rather than the OCR text will hit them. Tune the threshold
   from the fixture hit rate.
8. **Order in #332's schema, `steps` before `detail`?** Yes, see "Domain
   schemas". The issue text says headline first, which conflicts with the code
   and with rule 3.
9. **Handwriting (#332)?** Anchor with OCR, fall back to the card when a step
   cannot be placed, and say so on the card. Do not add a model for it before the
   spike measures how bad it is.
10. **Currency in the units overlay?** Not in slice 3. It needs a rate source;
    that is a network call with privacy and staleness questions of its own, so it
    is its own issue.
11. **A recording-friendly overlay (`overlay.capturable`)?** Config only, default
    off, documented for demos. A Settings toggle would invite turning off the
    protection by accident.
12. **Do UIA text ranges replace OCR where both exist?** No. OCR is the default
    for read-only marks (one code path, works everywhere). UIA is used where the
    action must also write (#326) or the anchor is a field (#329).
13. **#326: may a click write without an Enter confirm?** This needs a decision
    against Look, Propose, Confirm, Do. Recommended: no. A click on a label opens
    the preview card for that exact change and Enter confirms. Counter-argument:
    one click is the point of #326, and the label already shows the exact change.
    The risks are an accidental click on a label that sits over other text, and a
    click that lands on a label that has just moved. If the answer is yes, the
    exception is written into AGENTS.md and slice 7 keeps every guard listed in
    "#326: clickable labels".
14. **Do ghost text and marks survive Alt+Tab?** Recommended: no, hide and do not
    restore, with the preview card as the source of truth (stated in "When the
    screen changes"). Counter-argument: the preview survives Alt+Tab by design,
    so a user who tabs back finds a preview with no ghost text. Restoring needs
    the overlay to re-verify that the form window has not changed, which is the
    tracking this spec rejects.
