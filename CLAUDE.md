# copilot-ask — Quick-Reference Card

> Loaded into every session, so it stays small on purpose. Rules in their
> shortest enforceable form, the stack, the pitfalls that have already cost a
> session, and where to look. Depth lives in [`docs/`](docs/README.md).

**Name:** **Wingman** (decided 2026-09-16; the crate, bin, window classes,
mutex and config directory were renamed 2026-09-16, issue #1. The repo
folder is still `copilot-ask` until it is physically moved; the package
identity `RaaifYousuf.CopilotAsk` and icons are issue #10, not yet done).
GitHub: `Raaif-Yousuf/Wingman`.

**App:** a native Windows 11 tray assistant behind the Copilot key. Today: one
press screenshots the active monitor, a vision model checks the physics problem
on screen, a GDI card shows the verdict. Target: the key you press when you
want something on your screen dealt with. The loop is always **Look, Propose,
Confirm, Do**: the model fills a typed proposal, the user confirms, a
deterministic executor acts. Wingman never presses Send, Submit, Buy or Pay.
Cloud or local (Ollama), a purely offline mode, one switch that turns it off.
**No chat**: one press, one action, one card; no conversations, no
follow-ups (owner decision 2026-09-16). Meant to be a community project: the
action framework is the product and actions are the contribution surface.

**Repo:** `C:\Users\raaif\Wingman` | single Rust crate, ~8k LOC, Win32 via
`windows` 0.62, no async runtime | open source, MIT.

---

## Hard Rules

1. **No secret ever enters the repo.** Keys come from env vars or Windows
   Credential Manager (planned), never from a tracked file. `config.toml` lives
   in `%APPDATA%` and is gitignored. Never read
   `%APPDATA%\Wingman\config.toml` or the pre-rename
   `%APPDATA%\copilot-ask\config.toml` (still present; the rename migration
   copies it forward and leaves it in place): the owner asked for the keys to
   stay unread, and no task so far has needed them. Keys were rotated
   2026-09-15; do not re-raise that.
2. **Permissive licenses only.** Dependencies: MIT, Apache-2.0, BSD, ISC, Zlib,
   Unlicense. Code copied from another project: MIT only, attributed in
   `THIRD_PARTY_NOTICES.md`. Re-verify a license at adoption; two projects on
   the 2026-09-16 shortlist (Screenpipe, Piper's successor) had relicensed to
   proprietary or GPL since they were last looked at.
3. **`serde_json` keeps `preserve_order`.** JSON-schema property order is
   load-bearing: with `headline` before `detail` the model commits to a verdict
   before doing the arithmetic. The comment in `Cargo.toml` is the rule's home.
4. **The package stays sparse** (`AllowExternalContent`). A full MSIX moves the
   exe into a per-version `WindowsApps` path, breaking the `HKCU\…\Run`
   autostart on every upgrade and virtualizing the `%APPDATA%` writes.
5. **Nothing runs while idle.** No polling timers in the tray, never
   `NtSetTimerResolution`/`timeBeginPeriod`, every watcher is event-driven. The
   owner audits this machine's battery drain to the tenth of a watt.
6. **Ollama is `127.0.0.1:11434`, never `localhost`** (IPv6-first resolution
   stalls ~2 s per connection on Windows, MEASURED in the sibling CLAIR repo).
   `think` is always set explicitly (unset on a thinking model was MEASURED 28x
   slower). `keep_alive` is a top-level request field, not inside `options`.
7. **Every failure ends in a card.** Never a dialog box, never a silent no-op.
   The worker thread catches everything and posts a `Result`; the main thread
   never panics.
8. **Pure logic is unit-tested; Win32 is checked by hand and the check is
   named.** Before reporting a change done, state the one observable that would
   differ if it were wired to nothing, and go look at it (`wired-to-nothing`
   skill). A tray app has many ways to compile, pass tests and do nothing: a
   hook installed on a thread with no message loop, a `PostMessage` to a window
   that was never created, a menu item with no `WM_COMMAND` arm.
9. **Tests never touch production names.** The single-instance test uses its
   own mutex and window-class names (commit `011f11a`); any new named kernel
   object, registry value or file path gets a test-only variant too.
10. **Mark unverified theories as theories.** A causal claim carries its
    evidence: `MEASURED <date>:` plus the observation, or `THEORY (unverified):`.
    Disproving a theory replaces it in place. Two live examples: Claude 4.5
    models return 400 on `output_config.effort` (MEASURED 2026-09-15), and
    Anthropic's classifier refuses prompts that read as reasoning extraction
    (`stop_reason: "refusal"`, MEASURED 2026-09-15).
11. **No em dashes in user-facing strings**: card text, tray menu, settings
    labels, error strings, README. Use a full stop, a colon, or the word the
    dash was hiding. Comments and specs may use them.
12. **Design before code for anything architectural.** Specs live in
    `docs/superpowers/specs/YYYY-MM-DD-<topic>-design.md` and are written and
    approved before the implementation plan. The two existing specs are
    authoritative for what is built.
13. **Issues live in GitHub Issues** once the public repo exists; until then
    the roadmap is the expansion plan's § 13. Never recreate a markdown
    backlog. `NEXT_SESSION.md` is a handoff snapshot, `OWNER_TODO.md` is the
    human-only queue; neither is a backlog.
14. **Subagents run on Sonnet 5** (`model: "sonnet"` on every Agent call).
    Owner rule 2026-09-16.
15. **Shell discipline.** The Bash tool here is Git Bash; PowerShell is a
    separate tool with its own syntax. Never mix them in one command. Prefer
    `rtk` wrappers where the hook rewrites them. A compound command's exit code
    is the last command's, so check the one that matters.

---

## Stack

| Layer | Technology and the gotcha |
|---|---|
| Windows | `windows` 0.62 (Win32 + WinRT). Per-monitor-v2 DPI. `Windows.Media.Ocr` (`src/ocr.rs`, issue #30) does not need package identity for a direct-path launch -- MEASURED 2026-09-17, see the pitfall below |
| HTTP | `ureq` 3, blocking, on purpose. No tokio, no reqwest, no streaming: every response is a whole structured result |
| Capture | `xcap` 0.9 (Apache-2.0, permissive but not MIT) + `image` PNG-only |
| Config | `toml` in `%APPDATA%\Wingman\config.toml`, owner-only ACL, env overrides |
| Packaging | sparse MSIX, self-signed cert, `install.ps1` self-elevates once; identity `RaaifYousuf.CopilotAsk` |
| Local models | Ollama 0.34 on `127.0.0.1:11434`; Intel Arc 140T iGPU needs `OLLAMA_IGPU_ENABLE=1` or Vulkan drops it and runs CPU-only; the only oracle for GPU use is `size_vram > 0` on `/api/ps` |
| Release profile | `opt-level = "z"`, LTO, `panic = "abort"`, stripped; ~2 MB exe, under 10 MB idle |

---

## Critical Pitfalls (read before editing)

**The Copilot key is `Win+Shift+F23`, and swallowing it leaves Win logically
down.** `hotkey.rs` taps `VK_CONTROL` to cancel the pending Start menu. Known
fragile; if Start flickers, the fallback is swallowing the following `LWin`
keyup. Learn mode exists because Dell firmware may emit something else.

**A bare launch must not ask.** The Copilot-key picker and the Start Menu both
activate the exe with no arguments; the login autostart is the same bare launch.
`single_instance::poke_existing` decides what a duplicate launch means. Changing
that default means a billed API call every boot.

**The Settings picker step cannot be scripted.** `HKCU\…\Shell\BrandedKey` is
write-protected by the shell even for the user. The low-level hook works
regardless, so "the key responds" does not mean the picker route is live.

**Stock Ollama's tray app steals port 11434** about one second after being
killed, CPU-only. A health check that only asks "did something answer" reports
false success. Check the listener's owning process path, then `size_vram`.

**Windows OCR under a sparse package.** MEASURED 2026-09-17 (issue #30,
`src/ocr.rs`'s `ocr_live_recognizes_gdi_rendered_text`, `#[ignore]`d, run
manually with `cargo test ocr_live -- --ignored --nocapture`): a plain
`cargo test` binary has no package identity
(`GetCurrentPackageFullName` returns `APPMODEL_ERROR_NO_PACKAGE`, confirmed
by `ocr::has_package_identity()`), and `OcrEngine::RecognizeAsync` over a
synthetic GDI-rendered image still succeeded from it: cold call 36 ms, warm
call 22 ms, recognizer language `en-US`, `MaxImageDimension` 10000. WinRT
OCR does **not** require package identity for a direct-path-launched
process on this machine -- the earlier THEORY below is disproven.
`THEORY (unverified)`: whether an exe launched from the `Run` key while the
sparse package is installed elsewhere on the machine differs from this
measurement; no mechanism is known by which installing an unrelated package
would change this process's own identity, so this is not expected to be
revisited without a concrete reason to doubt it.

---

## Where to look

| Topic | Doc |
|---|---|
| **Where the last session stopped** | [NEXT_SESSION.md](NEXT_SESSION.md) |
| **Things only the owner can do** (the picker click, the repo name) | [OWNER_TODO.md](OWNER_TODO.md) |
| The running app: threading, modules, request shapes, card, hotkeys | [2026-09-14 design spec](docs/superpowers/specs/2026-09-14-copilot-ask-design.md) |
| Install, sparse package, signing, Copilot-key registration | [2026-09-15 packaging spec](docs/superpowers/specs/2026-09-15-packaging-and-install-design.md) |
| **What comes next**: name, providers, Ollama, modes, palette, awareness, roadmap and issue map | [2026-09-16 expansion plan](docs/superpowers/specs/2026-09-16-expansion-plan-design.md) |
| Which skill to reach for | [docs/README.md § Skills](docs/README.md#skills-claudeskills) |
| User-facing install and usage | [README.md](README.md) |
| Docs index and who owns which fact | [docs/README.md](docs/README.md) |

*Last updated: 2026-09-16 (card created; conventions ported from the sibling CLAIR repo and cut to what applies here).*
