# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it has a
tagged release.

## [Unreleased]

### Added

- `LICENSE` (MIT, Raaif Yousuf, 2026).
- `README.md` rewritten for the Wingman name and positioning: separates
  what is built (one action, cloud-only) from what is planned (the
  Look/Propose/Confirm/Do loop, local models, executors).
- `SECURITY.md`, `PRIVACY.md`, `CODE_OF_CONDUCT.md`.
- `CHANGELOG.md` (this file), `THIRD_PARTY_NOTICES.md`.

### Changed

- Product name: the app is now referred to as Wingman in documentation; the
  crate, binary and installed paths still say `copilot-ask` until the Phase 0
  rename (issue #1) lands.

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
