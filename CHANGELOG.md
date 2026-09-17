# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it has a
tagged release.

## [Unreleased]

### Added

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

- Product name: the app is now referred to as Wingman in documentation; the
  crate, binary and installed paths still say `copilot-ask` until the Phase 0
  rename (issue #1) lands.
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

## [0.1.0] - unreleased

The version currently in `Cargo.toml`. Not yet tagged; listed here because
this is what exists in the codebase today, per the working name
`copilot-ask`.

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
