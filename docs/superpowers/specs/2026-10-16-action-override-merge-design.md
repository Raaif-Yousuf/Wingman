# Overriding a built-in action field by field

Status: **approved, 2026-10-16** (AGENTS.md rule 12). Issue #559.
Supplements the [action model spec](2026-09-17-action-model-design.md)
("Merge rule", "Origin tracking").

## The problem

An `[[actions]]` entry whose `id` matches a built-in replaces the whole
built-in, prompt included, and marks it as a user action, so `config.ui.prompt`
stops applying to it. To turn on `rate_difficulty` for "Check my work" a user
has to copy the whole prompt into `actions.toml`.

## Decisions

- **Decision: field-level merge is the default for a built-in id.** Only the
  fields present in the user's entry override; absent fields keep the
  built-in's value. "Present" means present in the TOML, never "differs from
  the default", so `enabled = true` or `confirm = false` written explicitly
  still counts as a choice. The user file is parsed into a struct where every
  field is optional (`deny_unknown_fields` stays), so a typo is still a named
  parse error.
- **Decision: `replace = true` restores wholesale replacement.** An entry with
  `replace = true` takes every field from the user's block, as before, so it
  must carry all required fields. It is also how to clear `group` or `hotkey`,
  since TOML has no null and an absent key now means "keep". `replace` is a
  loader key only: it is not a field of `Action` and is never serialized.
- **Decision: a new (non built-in) id still needs every required field.**
  `name`, `inputs`, `proposal`, `executor`, `confirm` and `prompt`. A missing
  one is a named error for the file, as before. `replace = true` on an id that
  is not a built-in is accepted and means the same as leaving it out.
- **Decision: `Origin` stays `Builtin` when the merged entry has no `prompt`.**
  `Origin` only decides where the prompt comes from (`config.ui.prompt` for a
  built-in, the action's own text for a user action), so an override that does
  not set `prompt` must not switch the Settings prompt off. An entry with its
  own `prompt`, a `replace = true` entry and a new id are `Origin::User`.
- **Decision: validation runs on the merged result.** An invalid merge (for
  example a bad `executor`) is skipped, the built-in is kept, and the existing
  "Your version was skipped and the built-in was kept" notice is shown.
  Hidden actions (`enabled = false` or a disabled group) are still not checked.

## Compatibility

An existing override carries every required field, so its prompt and core
fields do not change. What changes is the optional fields it left out: an
omitted `group`, `hotkey`, `prefer`, `rate_difficulty` or `enabled` now keeps
the built-in's value instead of falling back to the default. A user who relied
on that adds `replace = true`.

## Where

`src/actions/mod.rs`: a loader-only `ActionOverride` struct,
`apply_override` (pure), and `merge_actions` generalised to carry an origin
per entry. `docs/actions.md` "Merge and visibility rules" and the schema table
are updated. `actions.example.toml` is generated from `builtin_actions()` and
has no overrides in it, so it does not change.
