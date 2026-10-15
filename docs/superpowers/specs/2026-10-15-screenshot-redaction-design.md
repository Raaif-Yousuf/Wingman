# Screenshot redaction before upload

Status: **draft, waiting for owner approval** (AGENTS.md rule 12). Issue #103.
Supplements the [expansion plan](2026-09-16-expansion-plan-design.md) section 19
and the [running app spec](2026-09-14-copilot-ask-design.md).

## Why this is a spec and not a PR

Redaction is a new stage that every cloud request passes through. It adds an OCR
pass to the hot path, changes what leaves the machine, and needs a decision about
what happens when it fails. Those are product decisions, so this stops at the
design. The owner decisions are listed at the end.

## The problem

`docs/positioning.md` promises "Screenshots are redacted before upload". Today
nothing does it. A screenshot with a card number, a password field, an API key or
an email address goes to the cloud as is. The same data also leaves as text:
the non-vision fallback sends OCR text and UIA field values to a text-only model.

**Done when** (from the issue): a screenshot with a visible card number reaches
the provider with that region painted over, verified from the request body.

## Where it goes

Every request already has the same shape: `capture::grab_raw` gives a `RawShot`
(RGBA), then each worker calls `capture::encode(&raw)` and hands the PNG to the
chain. There are about eight `encode` call sites in `app.rs`. The stage goes
between the two, on the worker thread (never the message loop, rule 5/7):

```
grab_raw -> RawShot -> redact::apply(&mut raw, policy) -> encode -> provider
```

To make it impossible to forget at a call site, the call is added once, inside a
new `capture::encode_for_chain(raw, &chain)` that all eight sites switch to, and
the plain `encode` becomes private to `capture`. A test greps for any other
caller (source-scanning test, CRLF-normalised, non-empty candidate set).

### Which requests are redacted

Redact when **any ready provider in the chain is a cloud provider**, not only the
first. A chain can fall through from Ollama to OpenAI, and the same pixels would
then leave. Requests whose whole chain is local (Ollama, a loopback compat
endpoint, see `usage::is_loopback_url`) skip it: the pixels never leave the
machine, and skipping saves the OCR time. Offline mode refuses the network in
code, so it is the same case.

## Detection

One OCR pass (`ocr::recognize`) over the pixels. The result is reused: the
non-vision fallback already needs the same OCR text, so the pass is paid for once
per request, not twice (the fallback closure takes the cached lines).

Rules, all pure functions over text so they are unit-tested without Win32:

| Rule | Reuses | New work |
|---|---|---|
| Card numbers | `payment_denylist::contains_card_number` (Luhn, 12 to 19 digits) | a span-returning variant, `card_number_spans`, so a whole line is not painted for one number |
| IBANs | `payment_denylist::contains_iban` | span variant |
| Key-shaped tokens | the run rules in `egress::redact_opaque_tokens` (20+ chars, mixed upper, lower, digit) | split out a `find_opaque_tokens` that returns spans; the redactor becomes a user of it |
| Email addresses | nothing | a small hand-written matcher, no regex dependency |
| Password fields | UIA `IsPassword` | see below |

Matching is over the whole OCR line, then mapped back to word rectangles. This
needs `ocr::OcrLine` to expose its word rects (today it keeps only the union).
Painting the words that overlap a span is deliberately a little generous.

False positives paint too much, which is the safe direction. A long path or hash
can be painted; the model usually still has the rest of the screen.

### Password fields

One UIA `FindAll` with an `IsPassword = true` property condition, scoped to the
foreground window, returns the rectangles directly. No text is read. The screen
rectangles are mapped to image pixels using the monitor origin and the downscale
factor `grab_raw` already computes. UIA is bounded by the existing
`uia_automation` timeout helpers; on timeout this rule is skipped and the failure
policy below applies.

THEORY (unverified): a `FindAll` with a property condition on a busy browser
window can take hundreds of milliseconds. It is not measured here; the first
implementation PR must measure it before the stage is turned on.

## Painting

A solid opaque rectangle, not a blur. Small blurred text can often be recovered
or read by a model; a filled box cannot. The issue says "blurred": this is an
owner decision (below). The paint function is pure over `(&mut RawShot, &[Rect])`
and is tested by pixel comparison: inside the rectangle every pixel is the fill
colour, outside it is unchanged.

## Cost

MEASURED 2026-10-15: `ocr::recognize` on a real capture of the primary monitor,
fitted to 1402x876, five runs in a debug test build: 329, 227, 204, 206 and
221 ms (the first run is cold), 58 recognised lines. Reproduce with
`cargo test ocr_live_times_a_real_screen_capture -- --ignored --nocapture`
(prints timings and counts only, never text). The OCR itself is a WinRT call, so
a release build is not expected to differ much, but that was not measured.

So redaction adds roughly 0.2 to 0.3 s to every cloud request, on top of
capture and encode. Because the OCR lines are reused by the non-vision fallback,
a request that needed the fallback anyway pays nothing extra. Text-dense screens
will cost more; THEORY (unverified), not measured.

## When redaction fails

OCR can time out or be unavailable (no language pack). Options:

1. **Fail closed** (recommended for a P1 privacy feature): show a card, "Could not
   check the screenshot for private details, so nothing was sent." Rule 7: a
   failure ends in a card. A setting `privacy.redact = "auto" | "off"` lets the
   user opt out knowingly.
2. Fail open: send unredacted. This breaks the one promise the feature exists
   for, so it is not recommended.

## The text path

The same data leaves as text. The OCR text, UIA field values and selected text
that go to a text-only model, and the card text a repair request echoes, must go
through the same rules (replace a match with `[card number]`, `[key]`,
`[email]`). This is included in the design because redacting only pixels while the
fallback sends the same digits as text would not meet the done-when.

## Preview and egress log

The "show me what you're sending" image preview (section 19 of the expansion
plan) is not built yet. When it is, it should read the encoded `Shot`, which is
already redacted because redaction runs before `encode`, so it needs no extra
work. The egress log row gains a count, `redactions: n`, so the log can say
how many regions were painted without saying what they held (the log never stores
contents).

## Testing and the wired-to-nothing check

- Pure: span finders per rule, the paint function, the word-to-span mapping.
- Pipeline: render a card number with GDI (the helper in `ocr.rs`), run
  `redact::apply`, decode the PNG that `encode_for_chain` produces, and assert the
  region is the fill colour. This is the issue's "verified from the request body"
  check, at the level of the bytes the provider would receive.
- Wiring: the source-scanning test above, plus the egress row's `redactions`
  count being non-zero for that image.
- Live, ignored: one run on a real capture, as in the cost measurement.

## Slicing

1. Pure pieces: span finders, `OcrLine` word rects, paint function. Observable
   only in tests, so it is not shipped alone as a feature.
2. The stage and `encode_for_chain`, the `privacy.redact` setting, the egress
   count, the failure card. Turns it on for cloud requests.
3. Password-field rectangles via UIA (after measuring).
4. The text path.
5. Manual blur in the region tool (separate issue, needs UI).

Slices 1 and 2 land together (rule 8: no pipeline stage nobody calls).

## Owner decisions needed

1. On by default for cloud requests, and **fail closed** when OCR fails?
2. Solid box or blur? Recommendation: solid box.
3. Accept about 0.2 to 0.3 s added to every cloud request?
4. Include the text path in the first release, or ship pixels first and say so in
   the README?
5. Chain with any cloud provider counts as cloud (recommended), or only the
   first provider?
