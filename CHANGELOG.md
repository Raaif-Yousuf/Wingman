# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it has a
tagged release.

## [Unreleased]

### Changed

- Settings has no Save or Cancel button any more. A change applies when you
  make it: a list or a checkbox when you pick it, a number or text box when you
  press Enter or leave the box, the text size when you let go of the slider. The
  line under the page says "Saved: Card corner" with an Undo button, or says why
  the change was not saved and puts the old value back. Close the window and the
  change is already in `config.toml`; coming back to the window shows anything you
  edited by hand meanwhile. Wingman changes only the one setting you touched, so
  comments and other edits in `config.toml` stay.

- One press of the Wingman key now shows the options card (a few choices for
  what is on screen) instead of running Check my work. This costs one small
  model call for the card, then one more for the choice you pick. To keep the
  old one-press flow, set `primary_action = "check-my-work"` under `[hotkeys]`
  in `config.toml`. Pressing the key once while the card is up runs the first
  choice, and twice quickly runs the second (never one that writes the
  clipboard: the key closes the card instead). A press while an answer or
  error card is showing closes it. `key_mode` and `double_press_ms` under
  `[hotkeys]` change this (#393).
- Starting Wingman a second time (the Start menu, the Copilot-key picker) shows
  the same card with only the choices that need no model, and sends nothing.

### Added

- A preview with a "Do it" button no longer closes when you switch to another
  window, so a half-edited entry is not lost. Esc or a press of the Wingman key
  closes it. The preview that asks before sending a screenshot still closes when
  you leave it. New `click_away_dismiss` under `[options]` (default `true`):
  set it to `false` and a click outside an answer or options card leaves the
  card up (#324).
- An error card has a "Report a problem" link. It opens a new GitHub issue in your browser with the version, mode, provider kinds and error class filled in. Nothing is sent by Wingman, and no error text, screen text, keys or paths are in the link (#373).

### Accessibility

- A screen reader now finds the links at the bottom of an expanded card (Copy details, Report a problem, Read aloud, Copy). Each has a name and a role, says when it has focus after Tab, and can be activated from the screen reader (#654).

## [0.2.0] - 2026-10-16

Changes since 0.1.0, grouped by what you notice. The release page has the download and its checksums.

### New actions

Wingman now ships more than two dozen built-in actions, all in the Quick Ask
palette and the tray's Actions menu. New since 0.1.0: Read QR code from screen
(no model, works offline), Copy text from screen, Add event from screen,
Review this email, Fill this form (works with no model too), Action items to a
checklist, Summarize this page, Summarize a link, Explain this error, Define
this word, Translate selection, Describe image (alt text), Explain what's on
screen, Table to CSV, Chart to data table, Receipt to expense row, Convert
times to my time zone, Relative dates to real dates, Is this a scam?, Contact to address book, Jargon to
plain English, Explain, fix and write tests for code, and three Rewrite for
tone actions.

You can write your own in `actions.toml`: they appear in the palette and run
like built-in ones, and an entry with the same id as a built-in changes only
the fields you write. Share an action as a file from the tray (Help and
diagnostics) and import one after a preview.

### Look and feel (Windows 11)

- Settings uses the modern Windows 11 controls, one aligned column of fields,
  visible edit outlines and plain-word labels, plus an "Open config file"
  button.
- The card follows the Windows 11 type sizes, the main button uses your accent
  colour, preview fields look editable, long labels and values show whole, and
  the difficulty badge has an accessible name.
- The Quick Ask palette follows light, dark and high contrast, has rounded
  corners and a clearer selected row, and sends nothing when it opens: a
  request starts only after you pick or type.
- The region overlay keeps your selection bright with a clear outline and a
  size label.

### First run

- A one-time card on the very first start says Wingman is running, which key
  to press and where the tray icon is.
- Setup cards ("No AI model set up yet", "Ollama is not running") show what to
  do without expanding, close when you click away, and are read in full by
  Narrator. A config.toml that does not parse is named on the card, with the
  line and column.

### Privacy and safety

- Card numbers, IBANs, key-like tokens, emails and password fields are
  painted over in the screenshot before a cloud request, and hidden in any text
  sent too, then restored in the answer. One checkbox in Settings.
- Wingman does not read password managers: it shows a card and sends nothing.
- Optional preview of exactly what will be sent before any cloud request.
- A daily cost budget with a warning and a stop, and the cost shown on each
  cloud answer. Local models stay free and are never blocked.
- A screenshot is treated as content, never as instructions to the model.
- A write is refused if the field changed after you confirmed it.
- Calendar files are deleted sooner, and the log and profile files are limited
  to your account.

### Local models and modes

- First run: with nothing set up, a press looks for a local Ollama with a
  vision model and uses it.
- Settings can pull an Ollama model with live progress, pick it from the tray,
  and fix a CPU-only Ollama on an Intel GPU PC. Tray: "Check this PC for local
  models".
- Local mode no longer fetches cloud model lists or starts connector sign-in.
- Settings has live model lists for ChatGPT, Claude and Gemini, and Gemini
  key, model and thinking fields.

### The card and Settings

- The card follows light, dark and high contrast live, shows what Wingman is
  doing while it works, scrolls from the keyboard, and has Tab between its
  footer links. Copy the answer or the card as an image (Ctrl+Shift+C), read an
  answer aloud, and choose which corner and display it appears on.
- Settings is laid out in plain words, scrolls with Save always in view, and
  Save with no edits changes nothing. The tray menu is regrouped, and its
  first line shows the running version.
- The tray icon shows when a run is working or the last one failed.

### Reliability

- A crash leaves a note that is shown on the next start. Copy diagnostics
  hides your account name and the last error's private details.
- A config.toml that fails to parse is never saved over.
- Cards show human errors instead of raw messages, with a next step.
- Fixes for QR codes that crashed the reader, held keys auto-repeating, and
  the card opening at 0x0.
- Confirmed actions no longer run on the keyboard thread.
- Quit and sign-out work while Settings is open, and Quit waits for a form
  fill or the Ollama fix to finish.
- A request that timed out is not sent again, so one press is billed once.
- When a vendor declines a request, Wingman stops and says why instead of
  sending the screenshot to the next vendor.
- In Local mode, an Ollama on another computer gets a one-time warning that
  the screenshot is sent there.
- Spend that could not be saved still counts toward the daily budget, and a
  refused Settings save leaves everything as it was.

### Install and release

- Portable mode: a `wingman.portable` file next to the exe keeps every file
  Wingman writes in that folder.
- CI now checks the exe size, startup time, idle memory and a start, hand-off
  and clean-exit smoke test. Release builds are reproducible and ship a
  CycloneDX SBOM and SHA256SUMS.
- The release is not code-signed. Windows SmartScreen will warn on first run;
  see the README.

## [0.1.0] - 2026-09-20

### Added

- Share an action as a file: tray, Help and diagnostics, "Export action to file"
  saves one of your actions as a `.toml`; "Import action from file" checks a
  file, shows what it does, and adds it to your `actions.toml` only if you say
  yes. Part of issue #117 (export and import of all settings is still open).
- "Convert times to my time zone" action: reads each time and zone on
  screen and lists it in your local time, day included. Ambiguous zones
  (CST, IST, BST) show every reading (issue #315).
- Ollama provider: local models over `127.0.0.1:11434`, with a health check
  that tells apart a real server from the stock Ollama tray app's CPU-only
  respawn, model discovery (`/api/tags`, `/api/show`) with a live or
  static-allowlist vision badge, and a GPU/CPU status line from `/api/ps`
  (issues #13, #14, #15).
- Gemini provider (issue #17).
- Mode switch: Cloud / Local / Auto (default) / Offline, from the tray. Auto
  tries Ollama first when it is up with the configured model loaded, then
  falls back to cloud providers. Offline refuses any non-loopback network
  call in code, enforced at the one place every provider's HTTP send goes
  through, before a socket ever opens (issue #19; see `docs/offline.md`).
- Pause: 1 hour / until tomorrow / until resumed, from the tray. While
  paused the hotkey passes every chord through untouched, no capture runs,
  and no request can start (issue #20).
- Automatic retry: a transport error or an HTTP 5xx retries with jittered
  exponential backoff; a 429 retries once against the server's own
  `retry-after` delay, or shows a card naming it if the delay is too long
  to wait on (issue #98).
- `docs/providers.md` and `docs/offline.md` (issue #22); `PROMISES.md`
  (issue #134); a "Why not Copilot or Recall?" comparison table in the
  README (issue #136).
- Wingman icon set and brand assets: tray, app and package icons replaced
  with the wing-and-spark mark (issue #10).
- `LICENSE` (MIT, Raaif Yousuf, 2026), `SECURITY.md`, `PRIVACY.md`,
  `CODE_OF_CONDUCT.md`, `THIRD_PARTY_NOTICES.md`.
- `.github/workflows/release.yml`: on a `v*` tag, builds the release exe,
  packages the sparse MSIX headlessly via `packaging/Build-Msix.ps1`, signs
  both only if the signing secrets are set, computes `SHA256SUMS`, and
  creates or updates the GitHub release with all three (issue #8).
- `.github/ISSUE_TEMPLATE/` issue forms and a pull request template asking
  for the spec link, red-then-green tests, docs, `cargo deny check`, and
  the wired-to-nothing observable (issue #11).

### Changed

- The global "Show difficulty rating" setting is gone (issue #197). The
  difficulty rating now belongs only to the Check my work action: override it
  in `actions.toml` with `rate_difficulty = true` to get the badge (an
  override replaces the whole entry, prompt included). An old `config.toml`
  that still has `show_difficulty` loads fine and the key is dropped on the
  next save.
- Product name: the app is now called Wingman. The crate, binary, window
  classes, config directory and package identity were renamed from
  `copilot-ask` on 2026-09-16 (issue #1); the first run copies the old config
  directory forward and leaves it in place.
- Provider API keys now live in Windows Credential Manager
  (`Wingman/<provider>`), never in `config.toml` (issue #2).
- `install.ps1` now restarts the previous `wingman.exe` if a phase-2
  upgrade fails, instead of leaving neither version running (issue #173),
  and cleans up the exported `wingman.cer` signing certificate on every
  exit path (issue #184).
- Settings save no longer silently drops `providers.order` entries beyond
  `openai`/`anthropic` (issue #194); `App::ask`'s readiness gate is now
  mode-aware, so it checks the providers the active Mode would actually
  select rather than the whole configured order (issue #192).
- The pending card now shows before the screenshot is PNG-encoded, instead
  of the encode step (at best compression) freezing the UI for up to a
  second first (issue #177).
- `ui.show_difficulty` now defaults to `false` (issue #197 part 1, owner
  decision 2026-09-17): a fresh config no longer requests or pays for the
  difficulty rubric unless the user turns it back on in Settings. An
  existing `config.toml` with `show_difficulty = true` already written to
  disk is unaffected, since `#[serde(default)]` only applies to an absent
  key. The difficulty rating is being moved into the future "Check my work"
  action (#37) as an opt-in option; README's "Difficulty rating" section is
  removed, and `docs/providers.md` now notes the rubric is opt-in where it
  documents the schema property order.

### Fixed

- A credential `Config::hydrate_secrets` could not read is no longer
  deleted by the next `Config::save`; it is now reported and left alone
  (issue #175).
- `edit_settings` no longer swallows a `Config::save()` error silently
  (issue #174).
- The provider fallback chain now re-validates a schema-invalid 200
  response against every provider in the chain, not only the first, so a
  malformed answer from provider 1 correctly falls through to provider 2
  (issue #176).

## Before the first tag

The state of the code under its working name, `copilot-ask`, before 0.1.0 was
tagged.

### Added

- Global hotkey capture: the Copilot key (`Win+Shift+F23`) and a secondary
  binding (`Ctrl+Shift+/`), both rebindable from the tray menu by pressing
  the key to learn it.
- Screenshot of the active monitor (or the primary, per config), downscaled
  before upload.
- A single built-in action: send the screenshot and a configurable prompt to
  OpenAI or Anthropic, and show the verdict on a native GDI card in the
  corner of the screen.
- A 1-10 (or U for "a professor would struggle") difficulty rating on each
  answer, toggleable; off means it is never requested.
- Click-anywhere-to-dismiss on the card; clicks are ignored while the
  request is in flight.
- A GUI settings window: provider selection, model picker per provider,
  API key entry, capture size, card timeout, text scale, prompt editing,
  start-with-Windows toggle.
- Provider fallback: with both OpenAI and Anthropic keys set, Anthropic is
  tried automatically if OpenAI fails; an empty key is skipped, not treated
  as an error.
- Single-instance enforcement: a second launch hands off to the already
  running process (opens its Settings window) instead of starting a second
  copy, which would double the keyboard hook and the billed API call per
  key press.
- Start-with-Windows via a per-user `HKCU...\Run` registry entry, self-
  repairing to the current exe path on launch, with no admin prompt.
- Sparse-package install (`install.ps1`) and uninstall (`uninstall.ps1`):
  self-signed certificate, one UAC prompt on first install, registers the
  app so it appears in the Copilot key's Settings picker; config is never
  touched by install, upgrade or uninstall.
- Environment variable overrides (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) so
  keys never need to live in `config.toml`.

### Known limitations

- Cloud providers only: no local model support yet (Ollama is planned).
- One action only: no actions framework, no executors, no confirm-before-act
  loop (all planned; see the expansion plan).
- No offline mode: every request leaves the machine to the configured
  provider.
