# Privacy

This document describes what Wingman actually does today, checked against
the code in `src/`, not what is planned. Planned behaviour is labelled
**Planned** and does not run yet. Wording here will be finalized alongside
the first-run consent screen (tracked as issue #58); until then this is the
accurate, current description.

## What leaves the machine, and when

Wingman makes a request **only** in direct response to you pressing the
hotkey (the Copilot key, `Ctrl+Shift+/`, or **Ask now** in the tray menu),
and only while it is not Paused (see "Pause" below). There is no telemetry,
no crash reporting, no update check, no analytics and no background network
activity of any kind: nothing is sent while idle, and nothing runs on a
timer (CLAUDE.md rule 5).

Five files under `src/provider/` can open a network connection --
`openai.rs`, `anthropic.rs`, `gemini.rs`, `ollama.rs` and `openai_compat.rs`
-- one per provider.
A press sends a request to exactly one provider, chosen from
`providers.order` and filtered by the active **Mode** (see "Modes and the
Offline guard" below); `Chain` tries the next provider in that filtered list
only if the first one fails, never more than one at a time. Whichever
provider answers, the request carries:

1. A screenshot of your active monitor, downscaled to `capture.max_edge`
   pixels on the long edge (1568 by default) and PNG-encoded.
2. The system prompt from `ui.prompt` in your config, sent alongside the
   image in the same request.

Where each provider's request goes:

| provider | endpoint | key source |
|---|---|---|
| OpenAI | `https://api.openai.com/v1/responses` | your `OPENAI_API_KEY` or the stored key |
| Anthropic | `https://api.anthropic.com/v1/messages` | your `ANTHROPIC_API_KEY` or the stored key |
| Gemini | `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent` | your `GEMINI_API_KEY` or the stored key |
| Ollama | `{providers.ollama.base_url}/api/chat`, default `http://127.0.0.1:11434/api/chat` | none; local server, no key |
| OpenAI-compatible (`compat:<name>`) | `{base_url}/chat/completions`, where `base_url` is whatever you configured per endpoint (OpenRouter, Groq, LM Studio, a local server, ...) | your saved `Wingman/compat:<name>` credential, a caller-named header, or none, depending on that endpoint's `auth` setting |

The three cloud requests go to whichever provider you configured with your
own API key. Ollama's request never leaves your machine: it is a loopback
HTTP call to a local Ollama server, only ever `127.0.0.1` (never
`localhost`; see [`docs/providers.md`](docs/providers.md#ollama)). Wingman
has no server of its own; there is no Wingman-operated backend anywhere in
the path. Exact per-provider request shapes are in
[`docs/providers.md`](docs/providers.md). The response (the verdict text
and, if the opt-in `ui.show_difficulty` setting is on, the difficulty
rating) is shown in the card and is not written anywhere except the
in-memory "last answer" the tray's **Copy last answer** reads from.

In Auto mode (the default), before a cloud request is sent Wingman may also
make one bounded (400ms), loopback-only `GET /api/tags` reachability probe
against your configured Ollama server, to decide whether to try Local first.
This probe is skipped entirely unless you have already added `"ollama"` to
`providers.order` yourself (`mode::should_probe_ollama`); a user who never
configured Ollama sees no extra network activity from this at all.

## Modes and the Offline guard

The tray's **Mode** submenu is Cloud / Local / Auto (default) / Offline.
Cloud and Local simply select which providers are tried. **Offline** is
enforced in code, not just a setting that hides a button: every network
send in `src/provider/common.rs` (`post_json_with`,
`post_json_with_connect_timeout`, `get_text_with_timeout`,
`post_json_read_body`) calls a guard, `offline_guard`, before opening any
socket or resolving any DNS. While Offline is active, the guard refuses any
URL whose host is not a literal loopback address (`127.0.0.0/8` or `[::1]`);
`localhost` is refused too, with a message pointing at `127.0.0.1`, since
CLAUDE.md rule 6 already establishes Wingman never uses that name itself. A
test in `provider/common.rs` fails the build if any file under
`src/provider/` calls the HTTP library directly outside this guarded path,
so the enforcement is structural, not a convention someone could forget in
a new provider.

Full detail, including what the guard cannot guarantee (it is
application-level, not a firewall rule) and how to verify it yourself with a
packet capture, is in [`docs/offline.md`](docs/offline.md).

## What is stored locally

- **Windows Credential Manager**, generic credentials named
  `Wingman/<provider>` (e.g. `Wingman/openai`, `Wingman/anthropic`,
  `Wingman/gemini`; see `src/secrets.rs`): your API key(s), unless supplied
  via the `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY`
  environment variables instead, in which case nothing is stored on disk at
  all for that provider. On first run after upgrading from a build that kept
  the key in `config.toml`, that key is moved into Credential Manager and the
  file is rewritten with an empty `api_key`. Settings shows only the last
  four characters of a stored key. Ollama has no key at all: it is a local
  server, nothing to authenticate against.
- `%APPDATA%\Wingman\config.toml`: the system prompt, capture and UI
  settings, and model choices; `api_key` is always empty here now. Owner-only
  ACL. Never read by any Wingman code path except the settings window and the
  request-building code; never uploaded anywhere as a file, only the
  individual fields the request needs.
- Nothing else. No screenshot is written to disk. No history, log or cache of
  past answers persists across a restart.

## Pause

The tray's **Pause** submenu (1 hour / until tomorrow / until resumed) stops
Wingman from making any request at all: while paused, the hotkey passes
through untouched, no screenshot is captured, and no provider is ever
reached, regardless of Mode.

## What Wingman never does today

- Never captures your screen without a key press.
- Never sends a screenshot to more than one provider per request.
- Never runs on a timer, and never polls anything while idle.
- Never makes any request at all while Paused.
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

## Planned: connectors, profile and memory (not built)

Future actions like calendar or email drafting will use OAuth-based
connectors (Google, Microsoft) with tokens in Windows Credential Manager, and
a local "about me" profile store, explicitly excluding payment data by
design. None of this exists yet; when it does, this document will be updated
in the same change, per issue #58.

## How to verify any of this yourself

Wingman is open source under the MIT license. `src/provider/openai.rs`,
`src/provider/anthropic.rs`, `src/provider/gemini.rs`,
`src/provider/ollama.rs` and `src/provider/openai_compat.rs` are the only
files that open a network connection;
everything above can be checked by reading them and `src/provider/common.rs`
(the shared, guarded send path and the Offline guard), or by running the app
under a packet capture during a key press -- see
[`docs/offline.md`](docs/offline.md#how-to-verify-this-yourself) for the
exact steps.

## How to wipe your data

```powershell
Remove-Item "$env:APPDATA\Wingman" -Recurse -Force
```

Deletes `config.toml`. If a key is still sitting in the file (rather than
Credential Manager) it goes with it, but rotate any exposed key with your
provider regardless; deleting the file does not invalidate it. A saved
credential in Windows Credential Manager (`Wingman/<provider>`) is not
touched by this and must be removed separately (Credential Manager app,
"Windows Credentials", or `cmdkey /delete:Wingman/<provider>`) if you want it
gone too.

A pre-rename install may also still have
`%APPDATA%\copilot-ask\config.toml` (`Config::old_path`, `src/config.rs`):
the rename migration only ever copies it forward, never deletes it, so
remove that folder too if it exists.

There is currently nothing else on disk to remove: no cache, no history, no
awareness store, because none of those exist yet.
