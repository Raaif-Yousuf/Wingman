# Read the screen aloud

Status: **proposed, 2026-11-01** (AGENTS.md rule 12: written before any code,
waits for the owner). Covers #1275. Builds on #77 (`src/speech.rs`, the card's
Read aloud button), #30 (`src/ocr.rs`), the [options card spec](2026-10-16-options-card-design.md)
(key table, per-action chords) and the [screenshot redaction spec](2026-10-15-screenshot-redaction-design.md).
Where they disagree on what the key or a card does, they win.

## The problem

A screen reader cannot reach an image-only PDF, a picture of text, a remote
desktop or a video frame. Wingman already has the two halves: Windows OCR reads
pixels, and SAPI speaks text. Nothing joins them. The card's Read aloud button
speaks only the card's own answer.

## Where it is today

- Model-free screen actions share `App::run_model_free_screen_action`: gate on
  Paused, `refuse_if_password_manager`, hide the card and capture on the main
  thread, then `spawn_screen_worker` runs the work and posts `WM_APP_RESULT`. Copy
  text from screen (`DispatchTarget::ExtractText`) and Copy region
  (`DispatchTarget::CopyRegion`, `ui::region::select_region`) are the pair this
  one mirrors. They work with no provider, in every mode.
- `ocr::recognize` runs on a worker and returns `OcrLine`s (text, rect, words) in the
  engine's reading order. MEASURED 2026-09-17: cold call 36 ms, warm 22 ms on a
  small synthetic image.
- `speech::Speaker` wraps `ISpVoice`. It lives in the `Card`, on the main STA
  thread, speaks with `SPF_ASYNC` plus `SPF_PURGEBEFORESPEAK`, and posts
  `WM_CARD_SPEECH_END` through `SetNotifyWindowMessage`, so nothing polls. It has
  `speak`, `stop`, `is_speaking` and `ended_streams`. It has no pause and no queue.
  MEASURED 2026-10-17: first `Speaker::new` took 8.6 to 14.5 ms cold.
- Hiding a card stops its speech (`stop_reading`).
- The options spec's key table: on an answer card the Wingman key dismisses; the key
  never starts a Look over a card.

## Decisions

- **A catalogue action, not a separate command, and not a new "no model" path.**
  The model-free path already exists and is shared by four built-ins. Two new
  built-ins join it: `read-screen-aloud` ("Read this window aloud") and
  `read-region-aloud` ("Read a region aloud"), the same pair shape as extract-text
  and copy-region. Being in the catalogue gives palette rows, the router's
  candidates, the tray, `primary_action` and later per-action chords (`Action.hotkey`,
  #42) for free. That satisfies "an action id that can be bound to a key". Two new
  `DispatchTarget` arms, `ReadAloud` and `ReadRegionAloud`, covered by the existing
  every-built-in-has-an-arm test.
- **The only difference from Copy text from screen is the sink.** The worker
  returns a speech plan instead of an `Answer` for the clipboard, and the main thread
  starts the voice. Nothing else about the gates changes.
- **No model, no provider, no network, no egress row.** The pipeline never touches
  `provider`, `egress` or `Mode`. It works in Offline mode and with no key.
  (THEORY, unverified: a third-party SAPI voice a user installed could itself send
  text to a cloud service. Wingman cannot see that. The default Windows voices are local.)
- **Pause means nothing runs.** The Paused gate (#1256) applies before capture, and
  pausing while reading stops the speech.
- **Nothing runs while idle.** OCR and the voice start from a press. Progress comes
  from SAPI's end-of-stream notification, never a timer.

## What the user sees

| State | What the user sees |
|---|---|
| Capturing | Nothing: the card hides, the window is captured (as every action does). The region picker shows its own overlay. |
| Working | The existing Working card ("Reading the screen") while OCR runs. |
| Reading | The **Reading card**: the first words of the text, "Reading 2 of 9", and two buttons, Pause and Stop. |
| Paused | The same card, "Paused", button Resume. |
| Finished | "Finished reading", auto-dismissed after 3 s (the answer cards' one-shot timer). |
| Nothing found | Info card: "No text found to read." |

## Capture and reading order

**What is captured.** Window: the foreground window's rectangle, cropped from the
active-monitor capture (`RawShot::cropped`), taken at full size, because OCR
accuracy drops on downscaled pixels. THEORY (unverified): the rectangle is the
DWM extended frame bounds, to avoid the invisible resize border. If the rectangle
cannot be read, the whole monitor is used and the card says so. A window spanning
monitors is cropped to the active monitor only (open question). Region: the
existing selector; a cancel shows nothing.

**Reading order is a pure function.** `read_aloud::plan(lines: &[OcrLine], opts) ->
Plan`, with `Plan { paragraphs: Vec<String>, skipped_private: usize }`. Steps, each
a named test:

1. Drop noise lines: no letter or digit, or only bullet and mask characters.
2. Group lines into **blocks**: a line joins the block above when it overlaps in x
   and the vertical gap is under 1.5 times the median line height; a larger gap starts
   a new block (a paragraph).
3. Order blocks: **columns first** when two or more blocks share a vertical band
   and are separated by a gutter, left to right then top to bottom within a column;
   otherwise top to bottom. Right-to-left languages are out of scope for v1.
4. Join a block's lines into text: a hyphen at a line end joins the word, any other
   line break becomes a space.
5. Split a paragraph longer than 1,000 characters at a sentence end, else at a
   space, always on a char boundary (never a byte slice of OCR text).
6. Cap the whole plan at 20,000 characters; the card says it stopped there.

THEORY (unverified): that the engine's own line order is wrong on two-column
pages and step 3 is needed. Manual check 2 measures it on a two-column image-only
PDF first; if the engine's order is already right, step 3 is dropped.

## Redaction

`capture.redact` is the setting that decides what is private. Nothing is uploaded
here, but the text is spoken in the room and the user asked for the same promise.

- With `capture.redact` on, each line's words that overlap `redact::sensitive_spans`
  (card numbers, IBANs, key-shaped tokens, emails) are **removed from the plan**,
  using `redact::word_byte_ranges` to map spans to words. They are not spoken and are
  not on the card. The card says "2 private details were skipped".
- Words inside a password field's rectangle are dropped too, using the same
  password-field rectangles redaction uses (THEORY, unverified: the lookup can run on
  the main thread at capture time and be handed to the worker without a second OCR
  pass or any repainting).
- With `capture.redact` off the whole text is read. The setting is the user's choice
  and is not overridden here (open question).
- `refuse_if_password_manager` runs first, as for every model-free screen action, so
  a password manager in front is refused before any pixel is read.
- The text and the plan are never written to the diagnostics log. The log records
  the count of paragraphs and skipped details only.

## Speech: threading and the voice

- **OCR and ordering run on a worker thread**, through `spawn_screen_worker`'s
  pattern. The worker posts the `Plan` to the main thread, like any result. It never
  speaks.
- **SAPI stays on the main STA thread**, where the `Speaker` already lives (apartment
  bound objects, the measured 8.6 to 14.5 ms cold creation). `Speak` is `SPF_ASYNC`, so
  the UI thread never waits for the voice.
- **The keyboard hook is never involved in speech.** The hook only posts
  `WM_APP_HOTKEY`, as today. It does no SAPI call, no allocation and no wait.
- **A queue, not one huge string.** `Speaker` gains `enqueue(text)`, which speaks
  without `SPF_PURGEBEFORESPEAK` so SAPI queues it behind the current paragraph, plus
  `pause()` and `resume()` (`ISpVoice::Pause` and `Resume`). The session enqueues
  paragraphs one at a time as each end event arrives (so memory stays small and Stop
  has one paragraph to drop). Each `WM_CARD_SPEECH_END` advances the count shown on
  the card. `stop()` keeps its purge and clears the session.
- **Ownership.** The `Card` owns the `Speaker` and the `ReadingSession { paragraphs,
  next, generation }`. A new `CardState::Reading` shows it. The generation pattern
  `preview.rs` uses drops a late OCR result if the user stopped or started another run.
- **Voice language.** v1 uses the default voice. The card shows the OCR language. Matching
  the voice to the OCR language is slice 4 (open question).

## Pause, Stop and the key

- **Stop** button, and **Pause and Resume** button, are owner-drawn through
  `card_buttons.rs`, real child buttons so #119 does not get harder.
- **The Wingman key stops it.** The Reading card counts as an answer card in the key
  table: the key dismisses it, `hide` calls `stop_reading`, and speech is silenced at
  once. The key never starts a Look while Reading shows, and the options spec's settle
  rule (presses ignored for one window after a dismiss) applies. In the "Always picks"
  key mode it does nothing, like every non-options card; Stop remains.
- **Esc is not claimed.** The card never takes focus, and swallowing Esc from the app
  underneath for a minutes-long reading would break dialogs there. Stop, Pause and the
  key are the controls.
- **Click-away does not stop it.** Unlike an answer card, the Reading card ignores a
  click elsewhere (the user clicks into the PDF to scroll while it reads), so the
  `dismiss.rs` watcher is not armed for it. It closes on Stop, the key, finishing, a
  newer card replacing it, or Pause (the global one).
- **A paused reading is bounded.** Pausing arms one one-shot timer (10 minutes) after
  which the reading stops and the card closes, so a forgotten pause does not hold the
  card forever. No timer runs while speaking or idle.
- **Busy.** OCR holds `busy`; speaking does not, so other actions still start. Starting
  any other card hides this one and stops the voice.

## Failure cards

| Case | Result |
|---|---|
| Paused, or a password manager in front | The existing gate cards, before capture |
| Capture fails | "Couldn't capture the screen" (existing) |
| OCR language pack missing, or timeout | The existing OCR messages (`ocr::missing_pack_message`) |
| No text recognised, or all of it was private | "No text found to read." |
| No voice installed, or the voice fails | The existing "Can't read aloud" card |
| Region selector cancelled | Nothing shown |
| Text cut at 20,000 characters | The Reading card says "Reading the first part" |

## Entry points

- **Tray:** two items, "Read this window aloud" and "Read a region aloud".
- **Palette:** both appear as catalogue rows.
- **Options card:** they are model-free, so they can join the static no-model card.
  Whether they do is an open question; the router can offer them by name.
- **A key:** `primary_action = "read-screen-aloud"` now; a per-action chord once
  `Action.hotkey` is wired (#42, options spec slice 5).

## Config

```toml
[read_aloud]
max_chars = 20000     # clamped 1000..100000
```

Optional with a default, `#[serde(default)]`. The voice and rate stay in Windows
Speech settings; Wingman adds none.

## Testing

Pure logic is unit-tested (rule 8); tests come first.

- `read_aloud::plan`: one column, two columns, a hyphen join, a mid-sentence break,
  noise lines dropped, a long paragraph split on a sentence and on a space, a
  multi-byte paragraph split on a char boundary (never panics), the character cap,
  empty input, and the order for a title above two columns.
- Redaction filter: a card number, an email and a key-shaped token are dropped and
  counted; ordinary numbers are kept; with `capture.redact` off nothing is dropped;
  a word inside a password rectangle is dropped.
- Gates: Paused refuses; a password manager refuses; neither reads a pixel. A test
  that the nothing-found and all-private cases give the info card.
- Key table: `Reading` yields Dismiss for both presses in Dynamic and Close modes, never
  Look and never RunOption; a double press does not start a Look.
- Session: ends advance `next`; a stale generation is dropped; Stop clears the queue;
  Pause then the timer stops it (the timer logic as a pure expiry function).
- A source-scanning guard (CRLF-normalised, file set asserted non-empty) that the
  read-aloud module imports no `provider`, `egress` or `ureq`, and that
  `keyboard` hook code never calls into `speech`.
- A guard that every hide path of the Reading card calls `stop_reading`.

### Manual checks (each names the observable that differs if wired to nothing)

1. **Image-only PDF.** Open a scanned page, press the action. Observable: the voice
   reads the page start to finish, and the progress count reaches the last paragraph.
2. **Reading order.** A two-column image-only PDF. Observable: the order spoken is
   column by column; this decides whether step 3 stays.
3. **Stop.** While it reads, click Stop, then separately press the Wingman key.
   Observable: audio stops within a fraction of a second and `is_speaking` is false.
4. **Pause and Resume.** Observable: the voice goes silent mid-sentence and resumes
   there.
5. **Offline and no key.** Observable: it works with Offline mode and no provider,
   and the egress log gains no row and the usage log no entry.
6. **Redaction.** A page with a card number and an email. Observable: neither is
   spoken and the card says details were skipped.
7. **Idle.** After finishing, no timer is armed and CPU is flat (rule 5).
8. **Hook latency.** Press the key while reading. Observable: the hook returns at
   once and the voice stops from the message, not the hook.

## Slices

1. **Pure core.** `read_aloud::plan`, the redaction filter, the session logic, tests.
2. **The action.** Catalogue entries, dispatch arms, `Speaker::enqueue/pause/resume`,
   the Reading card, tray items, gates. Done when Manual checks 1, 3, 4, 5, 6 pass.
3. **Key and Pause integration.** The key table row, the global Pause stop, the chord.
4. **Voice matching.** Choose a SAPI voice that matches the OCR language, if installed.

Size: no new crate and no new `windows` feature (`Win32_Media_Speech` is in use).
THEORY (unverified): about 8 KB.

## Open questions for the owner

1. Two actions (window and region), or one that asks which?
2. With `capture.redact` off, read everything, or always skip private details when speaking?
3. Should the two actions join the static no-model options card, which is capped at 3?
4. A window spanning monitors: crop to the active monitor, or capture the whole window?
5. Is a 20,000-character cap right, or no cap with Stop as the only limit?
6. Match the voice to the OCR language in v1, or leave the default voice?
