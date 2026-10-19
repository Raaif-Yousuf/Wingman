# Portable mode: an opt-in keys file next to the exe

Status: **proposed, 2026-10-17** (AGENTS.md rule 12). Issue #687. Builds on
portable mode (#130, `src/portable.rs`) and the Credential Manager lifecycle in
`src/config.rs` and `src/secrets.rs` (#2, #175, #899).

## The problem

Portable mode moves every file Wingman keeps next to the exe, but API keys stay
in Windows Credential Manager, so a copy on another PC starts with no keys. That
is the safe default, and it stays the default. Some users will want the keys to
travel with the drive. Hand-copying keys into `config.toml` is the wrong way to
get that: the load path blanks them and moves them to Credential Manager, and a
key left in a plain file with the drive's default permissions is readable by
anyone.

**Done when** (from the issue): there is an explicit setting, off by default; with
it on, keys live in an owner-only file on NTFS; on FAT or exFAT the user gets a
clear warning and the file is not written unless they override it knowingly.

## The decision, in one paragraph

**Decision (proposed):** a setting `portable.keys_in_file` (bool, default
`false`), honoured only when portable mode is on. When on, API keys are read from
and written to `wingman.keys` in the portable root (the exe folder), through a
new `FileStore` that implements the existing `SecretStore` trait, so the import,
hydrate, blank and env-override rules in `config.rs` do not change. The file is
restricted to the current user on a volume that supports permissions. On a volume
that does not (FAT, exFAT), it is refused unless the user turns on a second,
separately named override. Credential Manager is neither read nor written while
the setting is on.

## What this is not

- Not on by default, and not turned on by the portable marker. A user who copies
  the folder to a stick gets today's behaviour until they switch it on.
- Not encryption. DPAPI (used by `src/dpapi.rs` for the store) is bound to the
  Windows account and machine, so a DPAPI-encrypted file would not open on
  another PC, which defeats the point. The file is protected by permissions only.
  Passphrase encryption is an alternative, listed below.
- Not a replacement for Credential Manager. Credential Manager is stronger, and
  the Settings text says so next to the toggle.

## Threat model, in plain words

The file holds real API keys that spend money. Who can read it:

- **Another account on the same PC:** cannot, on NTFS, because the file is
  restricted to the account that wrote it. This is the one thing the permission
  protects against.
- **Anyone who has the drive and an administrator account on a PC:** can read it,
  by taking ownership. The permission does not stop this. Treat a lost drive as
  lost keys: revoke them at the provider.
- **Malware running as the same user:** can read it, as it can read the
  Credential Manager entry too.
- **A FAT or exFAT drive:** anyone who can plug it in can read the file. There is
  nothing the app can do about it, which is why it is refused by default.
- **A cloud-synced folder** (OneDrive, Dropbox) puts the file on someone else's
  servers. The Settings warning names it. See the open question on refusing it.
- **The repo:** a development exe runs from `target\` inside the checkout. A keys
  file there could be committed. See "Never under the repo".

What it does not change: keys still never appear in `config.toml`, the egress log,
a card, a diagnostics export or an error string (hard rule 1).

## The file

- Name: `wingman.keys`, next to the exe, in the same folder as the marker.
- Format: one TOML table, `[keys]`, one line per credential target
  (`openai = "..."`, `anthropic = "..."`, `compat.<name> = "..."`), using the same
  target names as `secrets::target_name` so a key moves between stores unchanged.
  The first line is a comment saying what the file is and not to share it.
- Never read from `config.toml`. The setting lives there; the keys do not.
- `.gitignore` gains `*.keys`, as a belt for the case below.

### Writing

`FileStore::set` and `delete` rewrite the whole file atomically in the same
folder: create a temp file, restrict it to the owner **before any key is written
into it**, write, flush, then rename over the old file. A crash leaves either the
old file or the new one, never a half-written file or an unrestricted one with
keys in it. A failed restriction aborts the write (an `Err`, never swallowed, as
`owner_acl::restrict_to_owner` already requires), so the caller shows a card and
the key is not saved anywhere else. There is no fallback to `config.toml`.

### Reading

`FileStore::get` keeps the tri-state from #175: file or key absent is `Ok(None)`,
a file that exists but cannot be read or parsed is `Err`, so `hydrate_secrets`
puts `UNREADABLE_KEY_MARKER` in the field and a later save never deletes what it
could not read. A parse error names the line number, never the text.

### Never under the repo

At startup, with the setting on, the portable root and its parents are checked
for a `.git` entry. If one is found the setting is refused with a card ("The keys
file would sit inside a code repository, so it was not used"). The check is a
file-exists walk, pure apart from that, and runs once. If the check itself cannot
run (the path cannot be read), the safe outcome is refusal, not acceptance.

## Permissions and the volume check

- **Supports permissions?** One call to `GetVolumeInformationW` for the exe's
  volume, reading `FILE_PERSISTENT_ACLS` in the flags. The filesystem name is not
  consulted, because a ReFS or network volume answers the flag correctly and a
  name list would drift. The flags to answer mapping is a pure function.
- **Yes:** `owner_acl::restrict_to_owner` on the temp file, as the config file
  and the store folder already do.
- **No (FAT, exFAT, or the call fails):** refused by default. The setting stays
  off, the Settings toggle shows why in plain words, and a card appears: "This
  drive cannot protect a keys file (FAT and exFAT have no file permissions), so
  your keys were not saved to it." The text has no em dash (rule 11).
- **Override:** `portable.allow_unprotected_keys_file` (bool, default `false`),
  a second checkbox shown only on such a drive, with the warning beside it.
  Turning it on shows a card that states the threat in one sentence first. While
  it is on, Settings keeps a standing warning line, and a card shows each time a
  key is saved to the file. It does not show on a bare launch.
- **If the volume check cannot run**, the answer is "no permissions". The safe
  outcome of a failed check is refusal.

## Interaction with what exists

| Piece | With `keys_in_file` off (today) | With it on |
|---|---|---|
| Key store | Credential Manager | `wingman.keys` only |
| Plaintext key typed into `config.toml` | imported to Credential Manager, blanked | imported to the file, blanked (same lifecycle) |
| Existing Credential Manager entries | used | left alone: not read, not written, not deleted |
| Env vars (`OPENAI_API_KEY` and the rest) | win over the store | win over the file, same rule |
| Env value on `save()` | never written to the store | never written to the file |
| Clear a key in Settings, save | credential deleted | line removed from the file |
| Diagnostics | names the store, never a value | names "keys file", never a value |

The seam is one place: where `Config::load` and `Config::save` pick the store
(today a fixed `CredManagerStore`). They ask `secrets::active_store()`, which
returns the file store when portable mode and the setting say so and the checks
above pass, and Credential Manager otherwise. Nothing else in the crate learns
about the file.

### Switching on and off

- **Switching on** does not copy anything by itself. Settings offers a button,
  "Copy my keys into the file", which reads Credential Manager once and writes the
  file, and says so. Without it the keys are blank until typed. It never deletes
  from Credential Manager.
- **Switching off** keeps the file. Settings then offers "Delete the keys file".
  Keys come from Credential Manager again, which may hold older values than the
  file did.
- **No silent fallback.** If the setting is on but the file cannot be used (refused
  volume, repo check, unreadable file), keys are unavailable and every request
  that needs one ends in a card that says why. It does not quietly go back to
  Credential Manager: that would hide which store holds the live key. Env vars
  still work.

### Another PC

This is the sharp edge. The restriction grants the account that wrote the file.
On another PC the same person has a different account (different SID), so the file
is unreadable there, and the keys do not travel without help. That is the price of
a file only its owner can read.

**Decision (proposed):** an access-denied read gives a specific card: "The keys
file was made by another account. Take it over to use it here." The card's action,
"Take over keys file", runs `takeown` and `icacls` on that one file and needs
administrator rights, with one UAC prompt, the same way install does. The user
asked for it explicitly, and the prompt is the confirmation. If they decline, keys
are unavailable, as above, and they can type new ones, which replaces the file.
THEORY (unverified): replacing the file works without ownership because deleting a
file needs delete rights on the folder, which a drive's folder usually grants.
It is checked in the implementation PR.

## Idle cost, failures and panics

- Idle cost is zero: the file is read at config load and written at save, both
  already event driven. No watcher, no timer (rule 5). The volume check and the
  repo walk run once at load.
- Every failure ends in a card (rule 7). The worker still catches everything.
- Nothing indexes or slices on file content. A key containing a quote, newline or
  other TOML-special character is written escaped and read back identical (test).

## Test plan

Rule 9: tests never touch the production file or setting. The file name `wingman.keys`
is a constant used only by production code; every test builds a `FileStore` on its
own path, `wingman-test-keys-<name>-<pid>.keys` in a scratch folder (the pattern
`portable.rs` tests use), and forces portable mode with `portable::ForcedRoot`.
No test calls `CredManagerStore` (existing rule), and the in-memory stub still
covers the shared lifecycle.

Pure:

- Flags to "supports permissions" for NTFS, FAT, exFAT and a failed call (refuse).
- The repo walk: `.git` in the root, in a parent, absent, and unreadable (refuse).
- File parse and write round trip, including special characters, empty file,
  duplicate keys (last wins, reported), and a garbage line (error names the line).
- Store selection: setting off, on without portable mode, on with the override, on
  on a refused volume. Only one case chooses the file.
- Precedence: env set means the file value is not used and the env value is never
  written to it, using the same injected `env_is_set` the config tests use.
- Lifecycle with the file store through the `SecretStore` trait: import and blank,
  hydrate, clear-and-save deletes the line, `Err` read gives the marker and a save
  leaves the file as it was.

On the real filesystem, no Win32 window:

- Atomic write on an NTFS scratch folder: after `set`, the file exists, the temp
  file is gone, and the permissions read back with `icacls` list the current user's
  SID and nothing else (the observable that differs if the restriction were wired
  to nothing). A second test arranges for the restriction to fail and asserts the
  file does not contain the key.
- Source-scanning test (CRLF normalised, non-empty candidate set): no code outside
  `secrets.rs` and `config.rs` names the keys file.

Manual check, named (rule 8): format a small virtual disk as exFAT, put the exe and
the marker on it, switch the setting on. The card appears, no `wingman.keys` is
created, and `dir` shows none. Then format it NTFS and see the file appear with
only the current account in its permissions. Run it once from a second Windows
account and see the access-denied card.

## Alternatives

- **DPAPI-encrypted file.** Rejected: it opens only for the same account on the
  same machine, so it does not travel.
- **Passphrase-encrypted file** (a key derived from a passphrase typed at launch).
  Travels and does not need ACLs, so it would work on FAT. Rejected for this slice:
  it needs a crypto dependency (a permissive one exists), a passphrase prompt, and a
  bare launch must not ask. It is the better design if travel matters more than
  simplicity; see the open question.
- **Keys in `config.toml`.** Rejected: that file is read by the migration, the
  backup and the blanking code, and it is the file people paste into bug reports.
- **A second marker file instead of a setting.** Rejected: the issue asks for an
  explicit setting, and a stray file should never switch where keys go.
- **Credential Manager only (today).** Stays the default.

## Open questions

- Open question: is an owner-only file acceptable when it cannot be read on another
  PC without taking it over, or is a passphrase-encrypted file the better answer to
  "keys travel with the drive"? The proposal takes the first, as the issue asks.
- Open question: should a keys file in a known sync folder (OneDrive, Dropbox) be
  refused, or only warned about?
- Open question: should the per-save card on an unprotected drive be a one-time
  card per launch instead?
- Open question: is a UAC prompt for "Take over keys file" acceptable, or should
  the user be told to do it from the file's Properties page?
- Open question: should "Copy my keys into the file" also be offered the other way,
  file to Credential Manager, when switching off?

## Slicing

1. `FileStore`, the format, the volume and repo checks, as pure code with the tests
   above. Not shipped as a feature alone (rule 8).
2. `active_store`, the setting, the card text, the Settings rows and the
   copy button. Turns it on.
3. Take-over action, after the unreadable-on-another-PC case is checked by hand.
