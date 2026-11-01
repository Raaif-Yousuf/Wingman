# Reply to this message: draft into the compose box

Status: **proposed, 2026-11-01** (AGENTS.md rule 12: written before any code,
waits for the owner). Covers #56. Builds on the
[executor spec](2026-09-17-executor-design.md) (`Confirmed<P>`, `Undo`), the
`replace_text` executor (#32) and its shared `executors/target.rs` machinery, and
the single "Undo last action" slot (PR #1310, not merged when this was written).
Where they disagree on what an executor may do, they win.

## The problem

Replying to a message means reading the thread, thinking, and typing. Wingman can
read the thread and draft the reply. The last step is putting the draft where the
user can edit and send it: the compose box. The loop is Look, Propose, Confirm, Do,
and the one hard line is **Wingman never sends**. The draft appears in the box; the
user presses Send.

## Where it is today

- `replace_text` re-resolves a UIA element captured at Look (`TargetRef`: runtime
  id, automation id, name, control type, window handle), refuses a stale element
  (`is_stale`: the live text no longer equals the text the preview showed), writes
  through `ValuePattern.SetValue`, and returns an `Undo` that re-checks the field
  still holds what it wrote. It refuses password fields, forbidden names
  (`uia_guard::is_forbidden_target`) and payment-shaped names or values.
- `replace_text` never types. Only `fill_form` uses the typed-input fallback
  (`write_with_fallback`). `plan_typed_input` drops `\n`, `\r` and `\t`;
  `plan_typed_fallback` allows `Edit` only and refuses any line break, control
  character or more than 10,000 characters. A multi-paragraph reply therefore cannot
  be typed by that fallback at all.
- `generic_replace::replace_or_copy` tries the write and, when there is no target or
  the write fails, copies the confirmed text to the clipboard instead.
- Inputs: selection, `PageText` (the foreground document through UIA, #104) and the
  screen, in that order for `summarize-page`.
- `review_email` and `rewrite_tone` already put text back through `replace_or_copy`.

## Decisions

- **One new executor, `insert_text`, `Effect::Writes`.** It reuses `target.rs` (the
  resolve, stale check, write and undo) and adds the insert. It never calls
  `write_with_fallback`, as `replace_text` does not. A module guard test asserts the
  file mentions no `SendInput`, `keybd_event`, `VK_RETURN`, `InvokePattern` or `Invoke`.
- **Never sends, by construction, not by filtering.** The executor has no way to
  press a key or invoke a button: its only calls are resolve and `ValuePattern.SetValue`
  (and a clipboard copy). Enter, Ctrl+Enter, Alt+S and every other send chord cannot
  reach the app because no key is ever generated. This is stronger than a keystroke
  filter, so v1 has none to get wrong.
- **One catalogue action, `reply-to-message`** ("Reply to this message"), proposal kind
  `text_draft`, executor `insert_text`, options-card `effect` = `preview`.
- **The clipboard is the main path, not the edge case.** THEORY (unverified): most
  compose bodies do not expose `ValuePattern` (browser mail, the new Outlook, Teams,
  Slack are web views; classic Outlook uses a Word editor). Manual check 1 measures
  which apps work. The executor therefore always finishes in a result card that says
  what happened: "Draft added to the reply box" or "Draft copied. Press Ctrl+V in the
  reply box." Same `replace_or_copy` shape.
- **No synthetic paste in v1.** A synthesised Ctrl+V would reach every app and could
  land in the wrong window. It is slice 4 and an open question.

## Inputs

1. **The thread.** In order: the compose body's own text if the user already opened
   Reply (the quoted thread is usually below the caret, so the box contains the thread),
   else the selection, else `PageText` of the foreground window, else the screen.
   The card says which one was used ("Read from the reply box").
2. **The target.** The focused UIA element at the press, read on the worker (UIA can
   block on another app, as the selection read does) before the card is shown. If the
   focus is not an editable element, there is no target: the draft goes to the
   clipboard only, and the card says "Open a reply first for Wingman to fill it."
3. Redaction: the thread goes through the existing text redaction before any cloud
   send, as `review_email` does. A draft that comes back containing a redaction marker
   (`redact::contains_marker`) is refused, never inserted.

## Proposal and schema

Property order is load-bearing (AGENTS.md rule 3): the model states what the message
asks before it writes the reply.

```json
{ "properties": { "asks": {"type": "string"}, "text": {"type": "string"} },
  "required": ["asks", "text"], "additionalProperties": false }
```

`asks` is one line and is shown on the card as context. `text` is the draft. After
parsing, the executor's input is `{ target, text, mode, expected_current_text,
caret: Option<usize> }` (`caret` is a UTF-16 offset, as `replace_text` uses). `text`
is cleaned in code, not trusted: control characters other than `\n` removed,
`connectors::is_hidden_format` characters removed, trailing newlines trimmed, at most
8,000 characters (a longer draft is refused with a card).

## Insert versus replace

`insert_text` has two modes and **never replaces text it did not put there**:

| Field at Look time | Mode | Result |
|---|---|---|
| Empty (after trimming) | `Fill` | `SetValue(text)`. The same as `replace_text`'s `ReplaceAll` with an empty baseline. |
| Has text, caret known (`TextPattern` selection is collapsed) | `InsertAtCaret` | The draft is spliced in at the caret with `splice_utf16` (start equals end), followed by one blank line when text follows the caret. The previous text is untouched. |
| Has text, caret unknown, or a selection is active | refused for writing | The draft is copied instead. The card says why. A selection is never overwritten. |

`replace_text` refuses a collapsed range because an insertion nobody previewed would
be a surprise (`WriteRefusal::CollapsedSelection`). Here the insertion is the previewed
intent, so `insert_text` has its own planning function (`plan_insert`) and does not
loosen `plan_new_value`; `replace_text` is not changed.

## Identifying the compose box, and re-checking before writing

At Look, `is_compose_candidate(element)` (pure) accepts only `Edit` or `Document`, not a
password field, with a name that is not on a short deny list of single-line fields
(`to`, `cc`, `bcc`, `subject`, `search`, `address`, `from`). A name that reads as a
message body (`message body`, `body`, `compose`, `reply`, `write a message`) is a
positive hint but not required, since many bodies have no name. This is a heuristic,
named so, and the preview card shows the window title and field name so the user
confirms the place, not just the text. THEORY (unverified): that the focused element is
the body in the common mail apps; Manual check 1 records it per app.

At Do, in order, each failure a refusal followed by the clipboard copy:

1. The target window is still the foreground window (new check: `GetForegroundWindow`
   root equals the window handle in `TargetRef`). The user may have switched apps while
   the model ran or the preview waited.
2. Re-resolve the exact `TargetRef` (found, unique): `target::resolve_index`.
3. Not a password field; not `is_forbidden_target`; not payment-shaped by name or by
   the value (`payment_denylist`), exactly as `replace_text`.
4. `is_stale`: the live text equals `expected_current_text`. A user who keeps typing
   while the preview shows will hit this. That is the correct outcome: the draft is
   copied, never merged into text it was not planned against.
5. The write itself re-checks the baseline in the same walk (the #432 fix), so text
   typed between steps 2 and the write is caught.

## Undo

The write returns an `Undo::recording` through the single "Undo last action" slot
(`Undo::remember`, PR #1310). Its restore re-resolves the field, checks the text still
equals what this executor wrote, and refuses otherwise, then restores the prior text.
A clipboard copy keeps no undo, as PR #1310 decides (the restore could overwrite
something the user copied later), and its card never promises one. This spec depends on
#1310 merging first for the tray item and slot; the executor itself returns the `Undo`
either way.

## The preview card

A `Preview` card, the Do it and Edit kind (not key-pickable, not dismissed by
click-away, per the options spec). Headline "Add this reply to the message?". Detail:
the `asks` line, the destination (window title, field name, "empty" or "at the cursor"),
and the draft text, editable. Enter confirms; Esc or the key dismisses and nothing runs.
The Wingman key never confirms it. The result card says "Draft added. Not sent." and
names Undo only when one is held.

## Failure cards

| Case | Result |
|---|---|
| No thread text found | Info card: "No message found to reply to." |
| Cloud send guard, Paused, password manager | The existing gate cards, before anything is read |
| No editable target | The draft is copied: "Draft copied. Open a reply and press Ctrl+V." |
| Compose box changed, window changed, caret lost, selection active | Draft copied, with the reason in one line |
| Password or forbidden or payment-shaped target | Draft copied; "Wingman will not type into this field." |
| Draft too long, or holds a redaction marker | Error card, nothing written, nothing copied |
| Undo refused because the text changed | The existing refusal error card |

## Testing

Pure logic is unit-tested (rule 8); tests come first.

- `plan_insert`: empty to `Fill`; caret in the middle, at the start and at the end;
  text after the caret gets one blank line; a selection refuses; a surrogate pair at the
  caret is kept (refused if mid-pair); CRLF fields; baseline mismatch.
- Draft cleaning: control characters and hidden format characters removed, trailing
  newlines trimmed, the cap, a multi-byte draft cut on a char boundary.
- `is_compose_candidate`: `Edit` and `Document` yes; `Button`, `ComboBox` no; a field
  named `Subject`, `To`, `Search` no; a password field no.
- Executor with the fake `TextElementAccess`: a confirmed `Fill` writes once; a stale
  baseline writes nothing and the clipboard fake receives the draft; a changed foreground
  window writes nothing; password, forbidden and payment-shaped targets write nothing;
  undo restores, and refuses when the text changed after the write.
- Never-sends guard: a source scan (CRLF-normalised, file set asserted non-empty) that
  `executors/insert_text.rs` contains none of `SendInput`, `keybd_event`, `VK_RETURN`,
  `InvokePattern`, `.Invoke(`, and never calls `write_with_fallback`. The fake access
  records every call; the test asserts only `resolve` and `write` happened.
- Dispatch and registry: `resolve("insert_text")` is `Effect::Writes`;
  `auto_confirm_read_only` refuses it; the catalogue id has a dispatch arm.
- The schema's property order (`asks` then `text`) is asserted.
- Tests use no real UIA window and no named kernel objects (rule 9).

### Manual checks (each names the observable that differs if wired to nothing)

1. **Per-app coverage.** In classic Outlook, new Outlook, Gmail in Edge, Thunderbird,
   Teams and Slack, open a reply and run the action. Observable: the draft text is in
   the box, or the clipboard holds it and the card says so. Record which apps write.
2. **Never sends.** Observable: after the write, the message is still unsent (the
   Sent folder is empty and the Send button was never focused).
3. **Existing draft.** Type a line, put the caret at the top, run it. Observable: the
   typed line is intact below the draft. With a selection, nothing is overwritten.
4. **Stale.** Keep typing while the preview shows, then confirm. Observable: the box is
   unchanged and the clipboard holds the draft.
5. **Window switch.** Alt+Tab to another app before Enter. Observable: nothing is
   written into the other app.
6. **Undo.** Tray "Undo last action" restores the box to what it was.

## Slices

1. **Pure core.** `plan_insert`, draft cleaning, `is_compose_candidate`, tests.
2. **Executor.** `insert_text`, registry entry, the foreground check in `target.rs`
   (shared, so `replace_text` gains it too, behind its own tests), guard tests.
3. **Action.** `reply-to-message`, schema, dispatch arm, preview and result cards, inputs.
   Done when Manual checks 1 to 6 pass.
4. **Synthetic Ctrl+V** for apps with no `ValuePattern`, only if the owner agrees.

No new crate or `windows` feature. THEORY (unverified): about 10 KB.

## Open questions for the owner

1. For compose boxes with no `ValuePattern` (probably most), is "copied, press Ctrl+V yourself" enough, or may Wingman synthesise one Ctrl+V after re-checking focus (slice 4)?
2. Should the action ever click Reply for the user (an Invoke on the Reply button), or only fill a reply the user already opened?
3. With text already in the box, insert at the caret, or only ever fill an empty box and otherwise copy?
4. Add a signature or greeting line, or leave the draft as the model wrote it?
5. New action `reply-to-message`, or extend the existing review-email action?
6. This waits on PR #1310 for the Undo slot: merge it first, or ship without tray Undo?
