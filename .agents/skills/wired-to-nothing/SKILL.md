---
name: wired-to-nothing
description: Use before reporting ANY feature, fix, or wiring change as done in this repo. Code that compiles, runs, passes tests and does nothing at runtime is the most expensive recurring bug in the sibling CLAIR repo (eighteen recorded instances, none caught by a test), and a Win32 tray app has more ways to do it than a web app. Also use when a fix "should work" but the symptom persists, or when reviewing a change that adds a message handler, hook, menu item, config field, provider, or installer step.
---

# Wired to nothing

**The bug class:** code that compiles, runs, passes its tests, is reviewed as
correct, and does nothing at runtime. The sibling CLAIR repo recorded eighteen
instances and **not one was caught by a test**, because a unit test mounts the
thing and calls it, which is precisely the step missing in production. The test
supplies the caller the real system does not have.

## The one rule

Before you say a change is done, **name the single observable that would differ
between "this works" and "this is wired to nothing", and go check that
observable.** Not the test suite. Not the diff. The observable.

If you cannot name one, you do not yet know whether the work is done. Say so.

## Check by shape

Match what you changed to a row. Do the check. Read the output.

| You added or changed | The check |
|---|---|
| A `WM_APP_*` message | Grep for the constant in both the poster and the window procedure. A message posted to a window that is not yet created, or handled in a `match` arm nobody reaches, is silent. Run the app and trigger it; watch the card or a `--verbose` log line |
| A low-level hook (`WH_KEYBOARD_LL`, `WH_MOUSE_LL`) | It needs a message loop on the installing thread. Confirm the install happens on the main thread after the loop exists, and that `UnhookWindowsHookEx` runs on quit. Press the key; watch the observable |
| A tray menu item | Trace the menu id to its `WM_COMMAND` arm. A visible item with no arm is #319's shape in the sibling repo: visible, clickable, reading nothing |
| A config field | Grep for the field name outside `config.rs`. A field with a sensible default and no reader looks like a deliberate choice and is dead. Same for a field in `Default` that `save_to` never writes |
| A provider or a request field | Read the recorded fixture and the live response, not the request builder's return value. A field the API silently ignores (an `effort` the model does not support, a `keep_alive` nested inside `options`) leaves no trace in a green test |
| A registry or autostart value | Read it back with `reg query` or `Get-ItemProperty`, then launch from the path it names. A value that points at a build-output path is broken on the next `cargo clean` |
| Anything in `install.ps1` or the manifest | Run the install, then query what Windows believes: `Get-AppxPackage`, the Copilot-key provider catalog (`AppExtensionCatalog.Open("com.microsoft.windows.copilotkeyprovider")`), the Start Menu entry. The manifest compiling is not the check |
| Threading a parameter through | Confirm the value actually differs at the destination. A parameter that reads the same source as the default it replaces changes nothing |
| A hard-coded list (model names, provider order, difficulty anchors) | Grep for a sibling copy. Two lists that "compute the same thing" drift by exactly one entry |

## Smells that mean "check harder"

- The fix is one line and the symptom was dramatic.
- It passed on the first run with no surprises.
- You are about to write "should now work" instead of "does work".
- The change is in a file whose tests all construct the thing directly.
- The change is in Win32 code and nothing in your transcript shows the app
  running.

## When the fix "should work" but the symptom persists

Stop adding fixes. You are likely looking at a second independent cause. Re-run
the same action and see whether the observable moved at all. If it did not move,
the fix is not partially working; it is not running.

Record it per Hard Rule 10: `MEASURED <date>:` with the observation, or
`THEORY (unverified):`.

## What honest reporting looks like

Say which observable you checked and what it showed. If you could not check one
(no live key, no Ollama running, cannot press the Copilot key from a script)
**say that plainly**, comment the manual check on the issue with what
passing looks like, and add the `needs-manual-check` label. "Tests pass" is not evidence that a feature is reachable.
