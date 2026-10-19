# Restore markers only in the fields that are free text (#1131)

Status: **proposed, 2026-10-17**

Scope: `redact.rs` (`Redactions::restore_in_json`), the two call sites in
`provider/common.rs` (the transport success path and the retry loop's success
return), `actions/schema.rs` (a new property flag), and the preview card in
`ui/preview.rs`. Related: #505 (reversible redaction of more text sources),
#1124 and #1146 (label masking). No new dependency.

## Problem

Before a request leaves, `Redactions` swaps each sensitive span for a numbered
marker such as `[email 1]` or `[secret 2]`, and keeps the pairs. When the
response comes back, `restore_in_json` puts the originals back. It does that by
text replacement over the **whole raw provider body**: the envelope, the keys,
every string, whatever the model wrote.

That is wider than it needs to be, in four ways:

- A marker in a JSON **key** or an enum value is replaced. The model can name a
  property `[secret 1]` and the body now holds a real secret as a key.
- A marker in a field the user never sees as text (an id, a kind, a field name
  inside a form proposal) is replaced, so a value leaves the "redacted" world
  into structure that other code reads.
- The replacement is on raw text, so a marker the model wrote with a JSON
  escape (`[email 1]`) is missed, and one split by an escape cannot match.
  The escaping of the *original* is handled today, the model's is not.
- The `secret` markers match in any case ("[Secret 1]"), by lowercasing the
  whole body to search it. That is a copy of the entire response per marker.

The goal of redaction is that a value leaves the machine as a marker. The same
care belongs on the way back: a value should return only where a person will
read it as text.

## Goals

- Restore only inside JSON string **values**, never keys.
- Restore only in fields the action's proposal schema declares as free text.
- Any other marker stays visible as a marker, so nothing is silently dropped.
- The confirm card shows the restored values, so the user sees exactly what
  will be used before any executor acts.
- A response that is not valid JSON never has anything restored (fail safe).

## Non-goals

- Changing what is redacted on the way out, or the marker format.
- Restoring inside embedded PNG data (already skipped on the way out).
- Re-redacting restored text for the activity log. The log keeps the redacted
  chain, as today.

## Design

### Declare the fields

`actions/schema.rs` already has one non-standard keyword, `"editable": true`,
read only by the preview card. Add a sibling, `"restorable": true`, on the
string properties whose text the user reads or edits: `detail`, `headline`, an
event title and description, a form field's value, a rewritten text, the items
of `action_items`. It is default-off, so a new proposal kind restores nothing
until its author says which fields are text. A test walks `PROPOSAL_KINDS` and
fails when a kind has no restorable string property and is not on a short
allow-list of kinds that carry no text (this keeps the flag from being forgotten).
Like `editable`, it lives on the schema the provider is sent, so "which fields
exist" and "which fields restore" cannot drift. Providers ignore unknown schema
keywords, as they do `editable` (confirm per provider in the first slice).

### Restore after parsing, by schema path

Today the restore runs inside the transport, on the envelope, before anything
knows the proposal kind. Move it to where the proposal `Value` is parsed out of
the envelope, then walk it with the schema:

1. Parse the proposal. If it does not parse, return it unchanged, with nothing
   restored.
2. Walk `Value` and schema together. At a string value whose schema property is
   `restorable`, replace markers in the **parsed string**, so JSON escaping is
   no longer a concern (a marker the model escaped is already plain text, and
   the original needs no escaping because it is a `String`, not JSON text).
3. Keys are never visited. Arrays of strings take the schema of their `items`.
4. Case-insensitive `secret` matching is done on the one string being walked,
   not on the whole body.
5. Markers left anywhere else are reported to the diagnostics counters
   (counts only, as `egress` does) so a model that scatters markers is visible.

`Redactions::restore_in_json` stays as a thin function for the one caller shape
that has no schema (none today; the test that pins "both call sites restore"
changes to pin the new function instead) and is removed when nothing uses it,
not kept as a second, wider path.

### The confirm card

The preview (`PreviewModel::from_schema`) is built from the restored value, so
an editable field shows the real text and the user's edit starts from it. A
marker that stayed (a non-restorable field) is shown as it is, which is the
signal that something sensitive was in that spot. Nothing restored is written
to the card's saved history beyond what the store already keeps.

### Interaction with the neighbours

- **#505, reversible redaction of UIA, selection and clipboard text.** Those
  sources add markers to the same `Redactions` pairs, so restore needs no
  change: it works on the pairs, and the pairs now grow. What matters is the
  allow-list above: more text sources means more markers in more answers, which
  is the reason to scope restore before #505 lands, not after.
- **#1124 and #1146, label masking** (the value after `password:` or `PIN`).
  These produce `secret` markers whose original is the masked value. A
  restorable `detail` that quotes the value would put a password back on the
  card. That is the existing behaviour and it is local, but it is a decision
  rather than an accident now: **Decision (proposed):** `secret` markers are
  restored only in fields that are both `restorable` and `editable`, never in a
  read-only `detail`. A password therefore comes back only where the user can see
  it and change it before an executor uses it.
- **Fail safe.** If the schema for the kind cannot be found, nothing is
  restored and the markers stay visible.

## Tests

Pure, in `redact.rs` and `actions/schema.rs`: a marker in a key is not
restored; in a non-restorable string is not restored; in a restorable string is;
an escaped marker (`[`) in a restorable string is restored; a made-up
marker is left; a `[Secret 1]` in odd case is restored once and only in an
editable field; a body that does not parse is returned unchanged; a restorable
array item; and the restore does not copy the whole body (no lowercase of the
envelope). The scanning test over `provider/common.rs` (CRLF-normalised,
non-empty candidate set) asserts the transport no longer calls the wide
function. A preview test shows restored values in the editable control.

Manual check: run a form-fill action on a page with an email and a password
field, confirm the card shows both values, and that a marker the model put in a
property name is still a marker in the diagnostics chain.

## Open questions

1. **Flag name and shape.** `"restorable": true` beside `"editable"`, or a
   list at the top of the schema? Recommended: the per-property flag, so it
   cannot drift from the property.
2. **Should a read-only `detail` ever show a restored secret?** Recommended:
   no. Restore `secret` markers only in fields that are also editable.
3. **Restore on a card that quotes a non-secret (an email) in `detail`?**
   Recommended: yes for non-secret kinds, since the user needs to read it, and
   it is local.
4. **Keep `restore_in_json` for providers with no schema?** Recommended: no,
   every request has a proposal kind today. Remove the wide function.
