# Manual redaction box on the send preview

Status: **proposed, 2026-10-17** (AGENTS.md rule 12). Issue #641. Slice 5 of the
[screenshot redaction design](2026-10-15-screenshot-redaction-design.md); read
that first for the paint rules, the fail-closed policy and the egress count.
Supplements the "Show me what you're sending" preview (#105, `src/ui/send_gate.rs`).

## The problem

Automatic redaction paints card numbers, IBANs, key-like tokens, emails and
password fields. OCR can miss text it cannot read (#507), and some private
content has no pattern: a name, a diagnosis, a chat message. The user needs a way
to say "not this part" before the pixels leave the machine, and to see that it
worked.

**Done when** (from the issue): a box drawn by hand is solid black in the image
that reaches the provider, checked from the request body, and it is counted in
the egress log redaction count.

## Open question and the proposed answer

The issue asks where the box is drawn (region picker or the Send preview) and
whether boxes apply to one request only.

**Decision (proposed): draw on the send preview, for that one request only,
painted solid before the request is sent.**

Why the preview and not the region picker:

- The preview is the moment the user has seen what is about to leave. It shows
  the image after automatic redaction, so the user can see what was already
  caught and cover only what was missed.
- The region picker is optional (most presses never open it), so a box there
  would be missed by most requests.
- It needs no new gate: the preview already blocks the worker until the user
  answers, and nothing is sent before then.

Why one request only: a box is a statement about this screen. The same screen
position holds different content next press, and a box that silently carried over
would hide the wrong thing and teach the user to trust a box that is not there.
Standing "never send this area" rules are a different feature (an app or window
list) and are out of scope; see Alternatives.

The preview is controlled by `config.egress_preview.enabled`, which is off by
default (a derived `Default`). With it off there is no preview and so no box. That is accepted: the box is part
of the preview, and a user who wants it keeps the preview on. The Settings text
for that toggle says so.

## What the user sees

Today the preview card shows only text rows. Its own comment (`RequestImagePreview`
in `src/ui/preview.rs`) notes there is no image surface, and that any thumbnail
must come from the same bytes that are sent.

1. **A thumbnail row** in the send preview card: the image from the request's PNG
   bytes, scaled to the card width, with the automatic boxes visible as black
   (they are in the pixels). Drawn with `StretchDIBits` into the card's paint
   path. This is also useful on its own: it answers "what exactly is leaving".
2. **A button, "Hide an area"**, beside Send and Cancel. It is a normal card
   button, reachable by Tab, so it has the same keyboard and UIA behaviour as the
   other two (`card_buttons.rs`, `card_uia.rs`).
3. Pressing it opens a **full-size overlay** on the monitor the card is on,
   showing the exact image to be sent at its real size (fitted to the monitor if
   larger), dimmed like the region picker. Dragging draws a box; drawn boxes are
   shown with a solid black preview fill and an outline. Enter accepts, Esc
   cancels and keeps the boxes the card had before.
4. Back on the card the thumbnail shows the new boxes in black and a line says
   "2 areas hidden by you". Send then sends the painted image. Cancel sends
   nothing, as today.

The overlay reuses the pure geometry in `src/ui/region.rs` (`normalize_drag`,
`clamp_to_desktop`, `enforce_min_size`, `Rect`) but not its capture logic: it
displays a bitmap that is already in hand and returns a list of rectangles.

### Keyboard path

A drag is not the only way. Mouse-free use is required (the card is fully
keyboard and UIA reachable today):

- In the overlay, a box is started with **Enter** at the centre of the image, or
  at the last mouse position if there was one.
- **Arrow keys** move the active box by one step (a tenth of the shorter image
  side, minimum 8 px); **Shift+Arrow** resizes it by the same step; **Ctrl+Arrow**
  moves by 1 px for fine placement.
- **Tab** cycles through the boxes, **Delete** removes the active one, **N**
  starts another box, **Enter** accepts all, **Esc** cancels.
- The overlay window has a UIA name and a status line (live-region text) that
  reads, for example, "Box 2 of 3, 420 by 80, left 120, top 300", so a screen
  reader hears each change. The hint strip at the bottom lists these keys the
  same way the region picker's hint strip does.

Rule 5: the overlay is event driven, no timers. Rule 7: the overlay failing to
open (bitmap too large, no window class) ends in a card, never a dialog.

## Coordinates, DPI and monitors

The least error-prone design avoids screen coordinates altogether.

- The boxes are stored in **image pixels of the PNG that will be sent**, as
  `redact::PixelRect` (the type the automatic stage already uses). They are
  painted onto that image with the existing `redact::paint`.
- The overlay maps a mouse point to image pixels with one pure function,
  `overlay_point_to_image(point, image_origin, display_scale)`, where
  `display_scale = displayed pixels / image pixels` (at most 1.0 after fitting).
  The inverse is used to draw. Both are pure and tested with scales of 1.0, 0.5 and
  a monitor at 150 percent and 200 percent DPI.
- The overlay is created per-monitor-v2 DPI aware like the rest of the app, on
  the monitor that holds the card (`MonitorFromWindow`), so one physical pixel in
  the overlay is one pixel on screen, and the card never has to guess the
  monitor's scale. The image is shown at `min(1.0, monitor / image)`.
- Multi-monitor: the image may come from the virtual desktop capture, which is
  larger than any monitor. It is fitted to the work area of the card's monitor,
  and the scale (below 1.0) is what maps back. Negative monitor origins never
  enter the mapping because the box is in image space, not screen space.
- Rounding: a box is rounded **outward** (floor of left and top, ceiling of right
  and bottom) after the inverse mapping, so a fitted-down overlay never leaves a
  one-pixel sliver of the area at the edge. Boxes are clipped to the image and a
  box with no area after clipping is dropped. Nothing indexes or slices on an
  unclipped value (brief rule on panics).

## Where it fits in the pipeline

Today `capture::encode_for_chain` paints, then encodes, then the chain asks the
gate with a `Request` whose `images` are encoded PNGs. The proposal keeps
that order and adds a second, small paint stage after the gate:

```
encode_for_chain -> Request (PNG) -> gate: preview, user draws boxes
                 -> manual boxes -> decode PNG -> paint -> encode -> send
```

- `SendGate::confirm` returns `Option<SendAuthorized>` today. It changes to
  return `Option<SendDecision>` where `SendDecision` holds the `SendAuthorized`
  and a `Vec<ManualBoxes>` (one list per image index). An empty list is the
  common case and leaves the bytes untouched.
- `Chain` applies the boxes to `req.images` before the request is built for the
  provider, in one function `redact::apply_manual(png, boxes) -> Result<Png>`.
  It decodes the PNG, paints, encodes. PNG is lossless, so every pixel outside a
  box is identical to what the preview showed.
- Painting from the encoded image (rather than keeping the raw capture alive on
  the worker) means no unredacted pixels are held longer than today, and the box
  can only ever add black, never reveal anything automatic redaction removed.
- The fill colour is the same black the automatic stage uses.
- The non-vision fallback builds text from OCR over the shot (`non_vision_request`).
  The OCR text is taken from the request built **after** the manual paint, so
  covered words are not read back as text. UIA field values, selection and
  clipboard text are not covered, exactly as the text-path note in the parent
  spec says (#505).
- A fallback to a second provider reuses the same painted bytes. The boxes are not
  asked for again within one request (one preview per request, as today).

## Failure

A failed manual paint must not send the unpainted image.

- If the PNG cannot be decoded, a box is out of range after clipping in a way that
  cannot be repaired, or the re-encode fails: **nothing is sent** and a card says
  "Could not hide the area you marked, so nothing was sent." (rule 7, fail
  closed, the same posture as the parent spec).
- If the overlay cannot open, the card says so and Send is still available for the
  image as it was (nothing was promised hidden). The user can cancel or send.
- If the card is dismissed while the overlay is open (click away, new run), the
  boxes are dropped with the preview, and the sender is dropped as today, so the
  worker wakes with no authorization.

## Egress count

`redact::note_image_redactions(kind, n)` (#508) is how the automatic stage adds to
the count the egress row shows. The manual stage calls it with kind `"manual"` and
the number of boxes after clipping. The row shows how many regions were painted
without saying what they held, as for the automatic kinds. `reset_image_counts`
runs at the start of each encode, so a count never leaks to the next request;
the manual note is made after the gate returns, in the same worker thread, before
the egress row is written.

## Cost

Idle cost is zero: no timer, no hook, no thread. The overlay window exists only
while it is open and is destroyed on close. A decode, paint and encode of a
typical 1402 by 876 image is a single pass on the worker thread (the screenshot
encode today takes the same order of time; this was not measured here, and the
implementation PR will measure it and put the number in this section).
Requests with no manual box pay nothing: the empty list returns before any
decode.

## Test plan

Pure, no Win32:

- `overlay_point_to_image` and its inverse at scales 1.0, 0.5, 0.75 and a DPI of
  96, 144 and 192, including a point exactly on an edge and a point outside the
  image (clamped).
- Outward rounding: a box whose mapped edges fall between pixels covers both.
- Clipping: a box partly outside, fully outside, zero area, and with swapped
  corners.
- The keyboard model: start, move, resize, fine move, tab, delete, accept, cancel,
  as a plain state machine with no window. Includes "cancel restores the old list".
- `SendDecision` with an empty list returns the same bytes (compare pointers or
  hash), proving the no-box path does nothing.

Pipeline, the issue's check:

- Render a known image (the GDI helper used by `redact_live`), encode it, build a
  `Request`, run the gate with a scripted decision that carries one box, and then
  **decode the PNG from the request bytes the provider would receive**. Assert every
  pixel inside the box is the fill colour and a sample of pixels outside is
  unchanged. Run it with the box at a scaled-down overlay mapping too, so the
  mapping and the paint are tested end to end.
- A box over a region the automatic stage already painted changes nothing and is
  still counted (it is the user's statement).
- The egress row for that request has `manual` count equal to the number of boxes
  (read from the row `egress` writes in the test, using a test-only log path per
  rule 9).
- Failure: a corrupt PNG with a box returns the card text and no request is sent
  (assert the provider double was never called).

Wiring (rule 8). The one observable that differs if this were wired to nothing: the
PNG in the request body has black pixels where the box was drawn. Wired to nothing
the unit tests would still pass, so the check is also by hand and named: press the
key on a screen with some text, open the preview, press "Hide an area", draw a
box over the text, press Send, and look at the image the preview shows after
the overlay closes, and decode the bytes the test hook captures, to see the box is
black. A source-scanning test (CRLF
normalised, non-empty candidate set) asserts that `Chain` calls
`redact::apply_manual` and that no provider call site builds a request from
`req.images` before it.

Live, ignored: one run on a real capture checking the overlay opens on a second
monitor at a different DPI.

## Alternatives

- **Draw on the region picker.** Rejected: most presses skip the picker, the user
  has not yet seen what automatic redaction caught, and it cannot show the
  outcome.
- **Blur instead of a box.** Rejected for the reason in the parent spec: small
  blurred text can be recovered; a filled box cannot.
- **Boxes that persist across requests.** Rejected for this slice: a position is
  not a durable meaning. A rule keyed to an app or window title is a separate
  design.
- **Draw directly on the card thumbnail.** Rejected: the thumbnail is about
  400 px wide, too coarse to cover a line of text or to work with the keyboard.
  The overlay at real size is the drawing surface and the thumbnail is the proof.
- **Keep the raw capture on the worker and paint from it.** Rejected: it keeps
  unredacted pixels alive during the wait and needs the raw shot threaded through
  the gate for no gain, since painting the encoded image is lossless.
- **A separate "mark private" hotkey before sending.** Rejected: one press, one
  action, one card; another entry point is another thing to learn.

## Open questions

- Open question: should "Hide an area" also be offered when the preview is off,
  as a small "Hide area first" option on the options card? The recommendation is
  no for this slice; a user who wants the box keeps the preview on.
- Open question: the step size for keyboard moves (a tenth of the short side,
  minimum 8 px) is a guess; a usability pass should set it.
- Open question: with several images in one request (a region plus a window), does
  the overlay open one image at a time (proposed, chosen with the row) or all
  together?

## Slicing

1. Pure pieces: the mapping functions, clipping, the keyboard state machine,
   `redact::apply_manual`, and the pipeline test with a scripted decision. Not
   shipped as a feature on its own (rule 8).
2. The thumbnail row in the send preview (useful alone).
3. The overlay window, the "Hide an area" button, `SendDecision`, the egress count
   and the failure card. This turns the feature on.
