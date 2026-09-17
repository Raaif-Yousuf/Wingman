# Privacy

This document describes what Wingman actually does today, checked against
the code in `src/`, not what is planned. Planned behaviour is labelled
**Planned** and does not run yet. Wording here will be finalized alongside
the first-run consent screen (tracked as issue #58); until then this is the
accurate, current description.

## What leaves the machine, and when

Wingman makes exactly two kinds of network request, both in
`src/provider/openai.rs` and `src/provider/anthropic.rs`, and both happen
**only** in direct response to you pressing the hotkey (the Copilot key,
`Ctrl+Shift+/`, or **Ask now** in the tray menu):

1. A screenshot of your active monitor, downscaled to `capture.max_edge`
   pixels on the long edge (1568 by default) and PNG-encoded, sent as part of
   one request to `https://api.openai.com/v1/responses` or
   `https://api.anthropic.com/v1/messages`, whichever provider is configured
   and first in `providers.order`.
2. The system prompt from `ui.prompt` in your config, sent alongside the
   image in the same request.

That request goes to whichever provider you configured with your own API
key. Wingman has no server of its own; there is no Wingman-operated backend
anywhere in the path. The response (the verdict text and, if enabled, the
difficulty rating) is shown in the card and is not written anywhere except
the in-memory "last answer" the tray's **Copy last answer** reads from.

**Nothing else is captured, and nothing is sent while idle.** There is no
telemetry, no crash reporting, no update check, no analytics, no background
network activity of any kind. A `grep` of `src/` for network calls today
turns up exactly the two endpoints above.

## What is stored locally

- `%APPDATA%\copilot-ask\config.toml`: your API key(s) (unless supplied via
  the `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` environment variables instead,
  in which case the file has none), the system prompt, capture and UI
  settings, and model choices. Owner-only ACL. Never read by any Wingman
  code path except the settings window and the request-building code; never
  uploaded anywhere as a file, only the individual fields the request needs.
- Nothing else. No screenshot is written to disk. No history, log or cache of
  past answers persists across a restart.

## What Wingman never does today

- Never captures your screen without a key press.
- Never sends a screenshot to more than one provider per request.
- Never runs on a timer, and never polls anything while idle.
- Never phones home to a Wingman-operated server, because there isn't one.

## Planned: awareness (not built)

The [expansion plan](docs/superpowers/specs/2026-09-16-expansion-plan-design.md)
§9 describes an optional awareness feature: a local, event-driven record of
your foreground window title, optionally clipboard text, and on-screen text
via OCR, used to answer questions like "what was that error five minutes
ago" and to populate context chips you can attach to a request. None of this
exists in code yet. When it ships:

- It will default **on**, and turning it on for the first time will require
  passing a **first-run consent screen** that states what is recorded, where
  it lives, that it never leaves the machine on its own, and how to turn it
  off, before anything is recorded.
- Storage will be local-only, SQLite encrypted at rest with DPAPI (Windows'
  own user-scoped encryption). Nothing awareness records leaves the machine
  unless you explicitly attach a context chip to a specific request, and that
  attachment will show in an egress log.
- Default retention will be 24 hours, with a "clear now" button.
- It will refuse to run on a timer (event-driven only), refuse to capture
  while an excluded window is foreground (password managers, anything with
  "InPrivate", "Incognito" or "Private Browsing" in its title, and anything
  you add), and stop entirely while Wingman is Paused.
- A tray indicator will show while awareness is on.

## Planned: local models and an offline mode (not built)

Today every request goes to a cloud provider; there is no local-model option.
The plan adds Ollama support and a Mode switch (Cloud / Local / Auto /
Offline). In Offline mode, the plan is for a socket-layer guard to refuse any
connection to a non-loopback host in code, not just hide the setting, so an
"offline" claim can be verified rather than trusted. That guard does not
exist yet either; until it does, "offline" is not a real mode of this app.

## Planned: connectors, profile and memory (not built)

Future actions like calendar or email drafting will use OAuth-based
connectors (Google, Microsoft) with tokens in Windows Credential Manager, and
a local "about me" profile store, explicitly excluding payment data by
design. None of this exists yet; when it does, this document will be updated
in the same change, per issue #58.

## How to verify any of this yourself

Wingman is open source under the MIT license. `src/provider/openai.rs` and
`src/provider/anthropic.rs` are the only two files that open a network
connection; everything above can be checked by reading them, or by running
the app under a packet capture during a key press.

## How to wipe your data

```powershell
Remove-Item "$env:APPDATA\copilot-ask" -Recurse -Force
```

Deletes the config file, including any saved API key. This does not rotate
the key itself; do that with your provider if the key may have been exposed.
There is currently nothing else on disk to remove: no cache, no history, no
awareness store, because none of those exist yet.
