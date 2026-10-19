# Export and import settings as one TOML file (#117)

Status: **proposed, 2026-10-17**

Scope: a new module for the settings file format and its checks (`src/settings_share.rs`),
two tray items under Help and diagnostics (next to the action import and export),
the preview card, and the write through `src/config_writer.rs`. Part of #117.
The other half of #117, sharing one action as a file, shipped in #704. Out of
scope: a zip archive, exporting `actions.toml` as a whole, the knowledge store,
palette history, and anything that holds an API key.

## Why a design doc (rule 12)

This adds a new file format that other people will send each other, and a new
way to change `config.toml` that does not go through the Settings window. Both
need rules written down before code.

## Problem

Someone who sets Wingman up on a second machine, or after a reinstall, has to
click through every Settings page again. Someone who wants to help another
person has no way to say "use my settings".

## Goals

- Export the settings that make sense on another machine into one `.toml` file.
- Never put a secret in that file. Keys stay in Credential Manager.
- Import shows what would change before anything is written, and writes only
  those keys, on top of the config as it is on disk now.
- Every failure ends in a card (rule 7).
- No new dependency.

## Non-goals

- A zip with everything inside. A single readable file can be checked by eye
  before it is imported, which a zip cannot.
- Moving the knowledge store, palette history, or the contents of `actions.toml`.
  Actions already have their own one-file share (#704).
- Merging two settings files, or syncing between machines.

## Design

### The file

One TOML file, `wingman-settings.toml` by default:

```toml
format = "wingman-settings"
version = 1
[settings.ui]
...
```

- `format` and `version` are required. A file with another `format`, or a
  `version` newer than the app knows, is refused with a card that says so.
- `settings` holds a subset of the keys of `config.toml`, with the same names
  and types, so the existing `Config` checks can validate it.
- The file is capped at 64 KiB, and each string at 1,000 characters.
- Unknown keys are not imported. The preview lists them as "ignored".

### What is exported

Exported: mode, `ui` (theme, text scale, card corner, and the like), `palette`,
`options`, `forms`, `egress_preview`, `budget`, `capture` options that are about
behaviour, the provider order, and per-provider model names.

Left out, on purpose:

- Anything secret: every `api_key`, and anything `Config::hydrate_secrets` fills
  from Credential Manager. The exporter works from the typed `Config` after the
  secrets are cleared, and a test fails if any `api_key` field is non-empty in
  the output.
- Machine-specific values: hotkeys and learned chords (they depend on the
  laptop's firmware), the chosen monitor, the Ollama and compatible-server
  addresses, and the knowledge folders (paths).
- Counters and flags that record what this install has done, such as the
  welcome card having been shown.

The list lives in one table in the module, so adding a setting later means
deciding on purpose whether it travels.

### Import

1. The user picks a file. It is read with the size cap and parsed.
2. It is checked: format, version, every value through the same validation
   `Config` loading uses. Values that fail are listed and skipped, not guessed.
3. A card shows the changes: for each key, the current value and the new one,
   grouped by page of Settings. Keys whose value would not change are not
   shown. A line says how many keys were ignored or skipped and why.
4. "Apply" writes the shown keys through `config_writer::write_changes`, which
   reads the file fresh, patches only those keys, keeps the user's comments and
   other keys, and refuses if the file changed while the card was open. Esc or a
   click away writes nothing.
5. The running app picks the change up the way a Settings change does.

If nothing would change, the card says so and offers no Apply button.

### Why this is safe

- It cannot overwrite a hand edit silently: the preview shows every change, and
  the write is a patch of those keys on the file as it is on disk.
- It cannot carry a key in or out: export clears secrets and the writer already
  refuses a non-empty `api_key`.
- It cannot run anything: the file holds settings values only, and import adds
  no action and no hotkey.
- Fail-open check: if the validation or the write cannot run, nothing is
  written and a card names the problem. A config file that does not parse is
  never written, as today.

## Slices

1. **Format and export.** The module, the exclusion table, the tray item, the
   file dialog. Unit tests on the exclusion rules and the round trip.
2. **Import preview.** Parse, check, diff against the current config, the card.
   No write yet.
3. **Apply.** The write through the config writer, the live reload, the cards
   for every failure.
4. Later, only if asked for: a zip that also carries `actions.toml`.

## Open questions

- Open question: should the Settings window also get Export and Import buttons?
  Recommended: tray only at first, like the action share. The Settings pages
  spec keeps that window to what a setting means.
- Open question: should a hotkey ever travel? Recommended: no. Chords depend on
  the keyboard, and a wrong one on import could take over a key.
- Open question: should the provider order travel when the other machine has no
  key for the first provider? Recommended: yes, since the setup card already
  says what is missing.
- Open question: file extension and default name. Recommended:
  `wingman-settings.toml`, so it opens in any editor.

## Test plan

- Exported text never contains a non-empty `api_key`, a hotkey, or a path;
  a source-scanning test over the exclusion table asserts its set is non-empty.
- Round trip: export, import into a default config, and the typed `Config`
  equals the original on every exported key.
- A bad `format`, a newer `version`, an oversized file, an unknown key, a value
  of the wrong type, and a file that is not TOML each end in a card and write
  nothing.
- Apply on a config with a hand-edited comment and an unrelated key keeps both
  byte for byte, and refuses when the file changed after the preview.
- Manual check of the card at 100% and 150% in light, dark and high contrast.
