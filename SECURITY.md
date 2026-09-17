# Security policy

Wingman is a tray app that captures your screen and sends it, on your
explicit key press, to a third-party model provider. Please report anything
that weakens that contract before you file a public issue about it.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting for this repository:
**Security ▸ Report a vulnerability** on
[Raaif-Yousuf/Wingman](https://github.com/Raaif-Yousuf/Wingman/security/advisories/new).
That opens a draft security advisory visible only to the maintainer and
GitHub, not a public issue.

Do not open a public issue for a vulnerability until a fix is available,
except for a finding that is already public (a vulnerable dependency with a
published CVE, for example), which can go straight into a normal issue.

Expect an acknowledgement within a few days; this is a one-person project run
outside working hours, not a company with an SLA. A fix timeline depends on
severity: a key- or data-exposure bug gets priority over anything else in the
backlog.

## Scope

In scope:

- **API keys.** How they are read, stored and sent. Today: `OPENAI_API_KEY` /
  `ANTHROPIC_API_KEY` env vars (take precedence when set), otherwise Windows
  Credential Manager generic credentials named `Wingman/<provider>` (issue
  #2). `config.toml`'s own `api_key` field is always blank; a build older
  than #2 that still has a live key there gets it imported into Credential
  Manager and blanked on first load. A bug that leaks a key into a log, a
  crash dump, a window title, Settings (which must show only the last four
  characters), or a network request to the wrong host is in scope.
- **Screen capture.** What gets screenshotted, when, and where it goes. A
  capture triggered without a key press, a capture sent to a provider other
  than the one configured, or a capture that includes more than the active
  monitor, is in scope.
- **The local config and any local store.** `config.toml` today; the planned
  SQLite awareness store, profile store and memory store once they exist.
  Path traversal, unintended world-readable ACLs, or writing outside
  `%APPDATA%\copilot-ask` are in scope.
- **Executors**, once they exist (none are built yet: see the [expansion
  plan](docs/superpowers/specs/2026-09-16-expansion-plan-design.md) §6). Any
  executor that runs without a confirmed user action, or that does something
  other than what the confirmed preview showed, will be treated as a security
  bug, not a regular one, because it breaks the "never the final button"
  guarantee the whole design leans on.
- **The offline/mode switch**, once it exists: a leak of a single byte to a
  non-loopback host while a mode claims to be offline.

Out of scope: the vision model's own judgement (a wrong verdict is a bug
report, not a security report), denial-of-service against a third-party
provider's API, and social engineering.

## Supported versions

Pre-1.0: only the latest commit on `main` is supported. There is no LTS
branch yet.
