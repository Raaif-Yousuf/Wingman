# Promises

Short, plain statements about what Wingman does and does not do, and how
each one is enforced or checkable today. Written against the Screenpipe and
Rewind precedents (see [`docs/positioning.md`](docs/positioning.md)): a
toggle nobody can verify is not a promise. Where a promise depends on
something not built yet, that is stated plainly, not implied to already
work.

Rule 10 (AGENTS.md) applies here too: every enforcement claim below is
either checked against the code as it exists today, or marked as a promise
for a planned feature.

## Never presses Send, Submit, Buy or Pay

**The promise:** Wingman fills, drafts and proposes; it never presses the
button that commits an action, permanently.

**Today:** there is nothing yet that *could* press such a button. The one
action Wingman has (checking a physics/statistics screenshot) is read-only:
it shows a card and does nothing else. This is not yet an enforced
guarantee about a confirm-and-execute loop, because that loop
(`LOOK -> PROPOSE -> CONFIRM -> DO`, described in the
[expansion plan](docs/superpowers/specs/2026-09-16-expansion-plan-design.md))
does not exist in code. **Planned:** when executors ship, every write goes
through a `CONFIRM` step the user's own Enter or click authorizes, and the
four named actions (Send, Submit, Buy, Pay) are permanently out of scope for
any executor, not a version-1 limitation to be relaxed later.

## Nothing runs while idle

**The promise:** zero CPU, zero network, zero polling timers when nobody
has pressed the key.

**Today, enforced:** the keyboard hook (`WH_KEYBOARD_LL`) and tray icon are
the only things resident, both purely event-driven; there is no polling
timer anywhere in the crate, no `NtSetTimerResolution`/`timeBeginPeriod`
call (AGENTS.md rule 5). A network request only ever originates from
`App::ask`, itself only reachable from a hotkey press, **Ask now** in the
tray menu, or the Auto-mode Ollama reachability probe (which itself is only
attempted when Ollama is actually configured -- see
[`docs/offline.md`](docs/offline.md)'s "Auto" section for the exact
condition). Grep `src/` for `Sleep(` or a timer-creating Win32 call outside
test code to check this yourself.

## No telemetry

**The promise:** nothing about your usage, your screen, your prompts or
your machine is sent anywhere except the one request you just triggered, to
the provider you configured.

**Today, enforced:** there is no telemetry code, no crash reporter, no
analytics SDK and no update checker anywhere in this crate. The only two
kinds of network request the app ever makes are documented in
[`PRIVACY.md`](PRIVACY.md): the provider completion request, and (Auto mode
only) the Ollama reachability probe to your own configured `base_url`.
`grep -rn "ureq::" src/provider/` names every place a socket can open;
outside `src/provider/`, nothing opens one at all.

## Keys never on disk

**The promise:** an API key you paste into Settings does not sit in a
plaintext file anywhere on the machine.

**Today, enforced:** `config.toml`'s `api_key` fields are always blank on
disk. A saved key goes to Windows Credential Manager as a generic
credential named `Wingman/<provider>` (`src/secrets.rs`), written by
`Config::save` before the file is serialized (`Config::push_secrets_to_store`
in `src/config.rs`). An environment variable override (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`) is never written to the store or the
file either. See [`docs/providers.md`](docs/providers.md#where-a-key-lives)
for the exact mapping. Ollama has no key at all, by design.

## Offline means loopback only

**The promise:** with Offline mode on, nothing leaves the machine except to
127.0.0.1 (or `[::1]`).

**Today, enforced:** every function in `src/provider/common.rs` that opens
a socket calls a guard (`offline_guard`) first, which refuses any URL whose
host does not classify as loopback while Offline mode is active, before any
transport call or DNS resolution. A structural test
(`no_provider_file_calls_ureq_directly_outside_common_rs`) fails the build
if any other provider file bypasses this by calling `ureq::` directly. Full
detail, including what this guarantee does **not** cover yet (no
connectors or update checker exist to need disabling, and the guard is
application-level, not an OS firewall rule), is in
[`docs/offline.md`](docs/offline.md).

## Open source, MIT

**The promise:** Wingman is MIT-licensed, permanently, with no relicensing
and no paid tier for the app itself.

**Today, enforced:** [`LICENSE`](LICENSE) is MIT. `deny.toml` restricts
every dependency to a permissive-license allowlist (MIT, Apache-2.0, BSD,
ISC, Zlib, Unlicense, Unicode-3.0) and CI runs `cargo deny check` on every
push. Code copied from elsewhere must itself be MIT and is attributed in
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) (AGENTS.md rule 2). There
is no payment code, no license-check code, and no feature gate anywhere in
the crate -- there is nothing to gate. **This is a policy commitment about
the project's future, not something a compiler can enforce**: nothing stops
a future release from changing the license text, the same way nothing
stopped Screenpipe. The check available to you is that the license lives in
the same public git history as the code, so a change would be a visible,
dated commit, not a silent swap.

## No auto-update without opt-in

**The promise:** Wingman does not update itself in the background.

**Today, enforced by absence:** there is no update-check code in the crate
at all. `install.ps1` is a script you run by hand; nothing calls it
automatically. **Planned:** the expansion plan describes an opt-in update
checker (Phase 5) that, like connectors, is explicitly disabled outright
under Offline mode when it exists. Until it ships, "no auto-update" holds
trivially, because there is no update mechanism to opt into or out of.

## Settings survive updates

**The promise:** running `install.ps1` again to upgrade, or moving/rebuilding
the exe, does not reset your configured keys, prompt, model choices or
hotkeys.

**Today, enforced:** `install.ps1` and `uninstall.ps1` never read, write or
delete `%APPDATA%\Wingman\config.toml` (or the pre-rename
`%APPDATA%\copilot-ask\config.toml`, which is migrated forward, not
discarded) at any phase -- see the "config.toml" comments at the top of
both scripts and their closing summary output, which states the path is
left untouched. The start-with-Windows registry entry self-repairs to the
current exe path on next launch rather than being reset
(`autostart.rs`). **Not yet covered by an automated test**: there is no CI
job today that performs a real upgrade-over-a-configured-install and
asserts every setting and hotkey survived byte-for-byte; the guarantee
above rests on the scripts never touching the file, which you can verify
by reading them, or by diffing `config.toml`'s timestamp and contents
before and after running `install.ps1` yourself.

## Never the final button (see above)

Folded into the first promise above; listed separately here only because
it is also the project's most load-bearing single sentence. It is a
permanent design constraint (AGENTS.md, the app's own top-level
description), not a feature flag: an executor that pressed Send, Submit,
Buy or Pay would be rejected in review regardless of how it is written,
per the `filing-findings` skill's "Enhancements must fit the product" rule.
