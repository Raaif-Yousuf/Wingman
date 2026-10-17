# Overlay staleness between capture and draw (#1139)

Status: **proposed, 2026-10-17**

An addendum to the [overlay design](2026-10-16-overlay-design.md). Its staleness
table covers the overlay once it is on screen. This covers the gap before that:
the time between the capture and the first drawn mark.

Scope: `app.rs` (the run start, `on_result`, `show_overlay`), `dismiss.rs`
(`arm_overlay`), `hotkey.rs` (`arm_overlay_keys`), `overlay/state.rs` and
`overlay/session.rs`. No new dependency, no new hook kind, no polling.

## Problem

A detector run captures the screen, then OCR runs on a worker thread, then
`on_result` takes `overlay::handoff::take` and `show_overlay` draws. The
staleness hooks go in only inside `show_overlay`, through `session::start`
(which calls `dismiss::arm_overlay` and `hotkey::arm_overlay_keys`). So for the
whole OCR and detector time, nothing is watching. If the user scrolls, drags the
window or switches tab in that time, the marks are placed from rectangles that
no longer match the screen, and they draw on top of the wrong text. Nothing
tells the user, and the overlay design's rule ("an overlay with no way to go
stale is the one outcome to avoid") is broken for exactly the window where
staleness is most likely: OCR is the slowest step, and a person who has pressed
the key is often still reading the page.

## Goals

- A scroll, click, key or foreground change after the capture and before the
  draw marks the run stale.
- A stale run shows its summary card with every mark as text (the existing
  `words::fallback` path), and no overlay window.
- The watch exists only while such a run is in flight, and is gone on every exit.
- If the watch cannot be armed, the run does not draw marks.

## Non-goals

- Following the content or re-running OCR. A stale run is not retried.
- Watching runs that never draw marks (the card-only actions). They have no
  overlay to be wrong about.
- A new "stale" card style. The fallback text is enough, with one added line.

## Design

### The pre-draw watch

Armed on the main thread where the capture succeeds, only for a run whose action
will go through `overlay::run` (the run that will put a `handoff::Ready`).
Reuses the two existing hooks and adds nothing that runs while idle.

- **Mouse and wheel:** `dismiss::arm_overlay(main_hwnd)`. The hook callback
  already only posts `WM_APP_OVERLAY_INPUT` and never decides.
- **Keys:** `hotkey::arm_overlay_keys` with a report-only mode. Today it claims
  Esc until an expiry. Before the draw there is no overlay to dismiss, so the
  pre-draw watch must not swallow Esc: a new argument (or a sibling function)
  arms reporting with no Esc claim. Esc then reaches the app and counts as a
  key like any other.
- **Foreground and window move:** checked, not hooked. The main thread records
  the foreground window and its rectangle at capture time. `on_result` compares
  both before drawing. This costs two Win32 calls once per run and needs no
  `SetWinEventHook`. It misses an Alt+Tab away and back with no other input,
  which is acceptable: the content on screen is unchanged in that case.

The target set is not known yet (it is found per annotation at draw time), so
the pre-draw rule is wider than the overlay's own: any wheel turn or click with
the pointer inside the captured monitor, and any non-modifier key while the
foreground window is the one recorded at capture.

### Decision at `on_result`

`on_overlay_input` today returns when `self.overlay` is `None`. It gains a
branch: when the watch is armed for the current run it decodes the input (the
existing `wire::to_input`) and calls a pure `predraw::on_input`, which sets the
run's `stale` flag. It never hides anything, there is nothing to hide.

`on_result` then, for a detector run: drains the watch (disarm), reads the flag,
and compares foreground window and rectangle. If stale, it skips `show_overlay`
and shows the `words::fallback` card plus one line, "The screen changed while
this was running, so the marks were not drawn." If not stale, it continues into
`show_overlay`, which arms the post-draw watch as today. The disarm of the
pre-draw watch and the arm of the post-draw one happen on the same main-thread
turn, so there is no gap and no second hook install (the shared `HOOK` slot is
already installed and stays).

### State and lifetime

| State | Set when | Cleared when | Hooks installed |
|---|---|---|---|
| `Idle` | no detector run in flight | n/a | none |
| `Watching { run, hwnd, rect, stale: false }` | capture succeeded for a marks run, both hooks armed | any exit below | mouse (shared slot), key reporting |
| `Watching { stale: true }` | a wheel, click or key arrives | same | same |
| `Drawn` | `on_result`, not stale, `show_overlay` took over | overlay hide (existing single hide function) | the overlay's own |
| `Idle` again | `on_result` stale; run failed; Pause pressed; a new run starts; run abandoned by the stuck timer | n/a | none |

Every row that returns to `Idle` goes through one `disarm_predraw()` that calls
`disarm_overlay` and `disarm_overlay_keys` and is safe to call when not armed.
It is called from: `on_result` (all branches, success and error), the
`set_busy(true)` path that already calls `drop_overlay` for a new run, `pause_for`,
`busy_now`'s abandoned-run branch, and app exit. The run generation guards a late
input: an input carries no run number, so the watch stores the run it was armed
for and `on_overlay_input` drops it when that differs from `run_generation`.

### Failure behaviour (the check itself fails)

- **Hook cannot be installed** (`arm_overlay` or `arm_overlay_keys` returns
  `false`): the run is marked "no marks" from the start. It still runs and still
  produces its card text, and `on_result` shows the fallback card. This is the
  same outcome as `show_overlay` returning `None` today.
- **Foreground or rectangle lookup fails** at capture or at `on_result`: treat
  as stale.
- **The worker finishes before the watch was armed** is not possible: the watch
  is armed before the worker is spawned.
- **A late hook post after disarm:** harmless, `on_overlay_input` drops it
  (watch not armed, or run differs).

## Tests

Pure, in `overlay/predraw.rs` (new, small): every input kind against the state
(wheel, click, key, modifier-only key, input for another run) gives the expected
`stale`; disarm from every exit leaves no hook recorded in a fake `Steps`; the
foreground and rectangle comparison for equal, moved, resized and unreadable.
Scanning tests (CRLF-normalised, non-empty candidate set): `on_result` calls
`disarm_predraw` on every branch; each path in the table above calls it; the
pre-draw arm is before the worker spawn. The existing `hotkey` tests gain one:
report-only arming never returns `OverlayKey::Esc`.

Manual check (the named observable): start a marks run on a long page, scroll
during OCR. Expected: the card appears with the marks listed as text and no
overlay window. With the watch wired to nothing, the marks would draw over the
scrolled page.

## Open questions

1. **Should Esc during OCR cancel the run?** Recommended: no, not here. It is
   reported as a key, so it makes the run stale, and cancelling is a separate
   change.
2. **Mouse moves and hover?** Recommended: ignore. Only wheel and button-down,
   as the overlay design already decides.
3. **Report-only as a new argument or a second function?** Recommended: an
   enum argument (`Claim::Esc(expiry)` or `Claim::ReportOnly`), so there is one
   place that decides what a keydown does.
4. **Use the window-move hook instead of the rectangle compare?** Recommended:
   the compare. It is one call at the end, and a hook would need the target set
   that does not exist before the draw.
