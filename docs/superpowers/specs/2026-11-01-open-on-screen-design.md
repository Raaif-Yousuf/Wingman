# Open what's on screen: an address, a tracking number, a flight number

Status: **proposed, 2026-11-01** (AGENTS.md rule 12: written before any code,
waits for the owner). Covers #1277. Builds on the
[executor spec](2026-09-17-executor-design.md) (`Confirmed<P>`, the registry),
the [options card spec](2026-10-16-options-card-design.md) (rows, `key_pickable`,
the static card) and #331 (triggers). Where this spec and those disagree about
what an executor or the card does, they win.

## The problem

A parcel number, a street address or a flight number on screen is usually
followed by the same chore: copy it, open a browser, find the right page, paste
it. The loop is Look, Propose, Confirm, Do, and every step here can be done
without a model: a pure detector finds the thing, the user confirms a URL they
can read, and one executor hands it to the shell.

## Where it is today

- Model-free built-ins exist and share one path: Copy text from screen, Read
  QR, Copy region, Relative dates. Each has a `DispatchTarget` arm in
  `ui/palette_model.rs`, and `App::run_selection_first_action` (#709) reads the
  selection first and falls back to OCR of the screen on a worker.
- `src/calc/` is the template for a local detector: pure functions, bounded
  input (`MAX_INPUT_LEN`), no model, unit tests with near misses.
- No executor opens a URL. `ShellExecuteW` is used in `app.rs` for the config
  file and the knowledge folder, and in `connectors/ics.rs` behind an injectable
  `Opener` trait. Read QR shows a link on its card and never opens it.
- `Effect` is `ReadOnly` or `Writes`; `auto_confirm_read_only` refuses
  anything that is not `ReadOnly`.

## Decisions

- **One new executor, `open_url`, with `Effect::Writes`.** It changes nothing
  on disk, but it makes the user's browser send a request that carries the
  number or address to a third party. `Writes` means `auto_confirm_read_only`
  refuses it, so a `Confirmed` can only come from the user's Enter. (Rule: it
  can never be read-only, because loading a tracking page is a lookup the user
  has not asked for until they confirm.)
- **One catalogue action, `open-on-screen`** ("Open what's on screen"),
  model-free, `executor = "open_url"`, options-card `effect` = `preview` (it
  ends in a confirm card, so the key may pick it, per the options spec's
  `key_pickable`). A new `DispatchTarget::OpenOnScreen` arm, like
  `ResolveDates`. The existing test that every built-in id has an arm covers it.
- **Detection is local and pure.** A new module `src/open_on_screen/`
  (`mod.rs`, `tracking.rs`, `flight.rs`, `address.rs`, `url.rs`), hand-written
  scanners in the style of `calc/`, no new crate, no regex backtracking.
- **No network until Confirm, and none from Wingman at all.** Detection
  reads the selection or OCR text on this machine. The egress log stays empty
  because Wingman sends nothing; on Confirm the browser makes the request.
- **The confirm card shows the URL exactly as it will be opened.** Not a
  prettified host, not "Open UPS tracking" alone: the headline names the kind,
  the detail is the full URL, character for character.
- **Wingman never fills or submits anything on the page.** The URL carries the
  value in a GET query string or path, so loading it is the whole action. No UIA
  on the browser, no keystrokes, no clipboard write.

## Detectors

```rust
pub enum Kind { Tracking(Carrier), Flight, Address }
pub struct Candidate { pub kind: Kind, pub text: String, pub span: Range<usize> }
pub fn detect(text: &str) -> Vec<Candidate>;            // pure, bounded, ordered by position
```

`text` is clamped to 20,000 characters on a char boundary before scanning (OCR
of a full screen is well under that). The scan is linear in the input. At most
8 candidates are returned, deduplicated on normalised text.

### Tracking numbers

Carrier forms and their check digits. Where an algorithm is stated here from
memory it is `THEORY (unverified)` until the unit test uses a real published
sample and passes; a form whose algorithm cannot be confirmed is dropped from
v1, not guessed.

| Carrier | Shape | Check | Needs a context word? |
|---|---|---|---|
| UPS | `1Z` + 16 alphanumeric | mod 10 over the 16 (letters map to digits; odd positions once, even doubled). Sample `1Z999AA10123456784` | no: the `1Z` prefix and length are distinctive |
| USPS | 22 digits starting `92`, `93`, `94` or `95` (also 26) | GS1 mod 10 (weights 3 and 1 from the right) | no |
| USPS | 20 digits starting `9` | same | yes |
| FedEx | 12 digits | weights 3,1,7 repeating over the first 11, mod 11, then mod 10 | yes |
| FedEx | 15 digits | not yet confirmed | dropped from v1 unless a source is found |
| DHL Express | 10 digits | first 9 digits mod 7 equals the last | yes |

A context word is one of the carrier's name, `tracking`, `track`, `waybill`,
`shipment`, `parcel`, `package` on the same line or the line before (OCR lines
are in reading order). The reason is arithmetic, not measurement: a random
12-digit string passes the FedEx check about 1 time in 10, and a random 10-digit
string passes mod 7 about 1 time in 7, so a check digit alone cannot carry an
unlabelled number.

### Flight numbers

Shape: an airline designator (two letters, or a letter and a digit, or a digit
and a letter) then optional space then 1 to 4 digits, no leading zero, not
touching other letters or digits. **The designator must be in an embedded list
of IATA airline codes** (about 150 entries, about 1 KB; THEORY, unverified, the
size is an estimate). Without the list `PO 1234`, `ID 5555` and `AB 12` would
all match. A match also needs a flight cue (`flight`, `departs`, `arrives`,
`gate`, `terminal`, `boarding`, `airline`) anywhere in the same text, unless
the whole selection is the flight number (a deliberate selection is its own
cue).

### Addresses

v1 is US and English only. A candidate is: 1 to 6 digits, then 1 to 4 words of
which at least one starts with a capital, then a street suffix from a fixed list
(`St Street Ave Avenue Rd Road Blvd Dr Drive Ln Lane Ct Court Pl Place Way Pkwy
Hwy`), then optionally a unit (`Apt 4B`, `Suite 200`), then optionally a city,
a two-letter US state and a 5 or 5+4 ZIP on the same line or the next. A line
break inside an address is joined with a space. Rejected: a leading `$`, a number
followed by a unit or percent (`5 miles`), a suffix with no capitalised name
word before it (`2 way street`).

## False-positive rules

Applied to every candidate, in this order, each a named test:

1. **Phone numbers.** A digit run is dropped if it touches `+`, `(`, `)`, or
   is grouped by separators as 3-3-4 or 3-4. Separators in a tracking number
   are not allowed except one space or hyphen every 4 digits for the USPS 22-digit
   form.
2. **Order numbers.** A digit run is dropped if the word just before it is
   `order`, `invoice`, `account`, `acct`, `ref`, `reference`, `po`, `confirmation`,
   `id`, `phone`, `tel` or `fax`, or if it follows `#`, unless a carrier name or
   `tracking` sits between that word and the number.
3. **Check digit.** Required where the table says so. A bad check digit is a
   near miss, never offered.
4. **Redaction overlap.** A candidate whose span overlaps
   `redact::sensitive_spans` (card number, IBAN, key-shaped token, email) is
   dropped when `capture.redact` is on. Its text and URL would otherwise appear
   on a card and go to a third party.
5. **Length and charset.** Over-long or non-ASCII-looking runs are dropped
   before any check is computed.

## URLs and the allow-list

`url.rs` holds one pure function per kind that builds the URL from a
**normalised value** (uppercase, spaces and hyphens removed, percent-encoded),
and one pure `validate(url) -> Result<(), UrlError>` that every URL passes
before it reaches a card or the shell.

| Kind | URL (templates are THEORY until opened by hand, Manual check 4) |
|---|---|
| UPS | `https://www.ups.com/track?tracknum=<n>` |
| FedEx | `https://www.fedex.com/fedextrack/?trknbr=<n>` |
| USPS | `https://tools.usps.com/go/TrackConfirmAction?tLabels=<n>` |
| DHL | `https://www.dhl.com/global-en/home/tracking/tracking-express.html?submit=1&tracking-id=<n>` |
| Flight | `https://www.bing.com/search?q=<designator>+<number>+flight+status` |
| Address | `https://www.bing.com/maps?q=<encoded address>` |

`validate` accepts a URL only if all of these hold, and each has a negative test:

- It starts with `https://`. No `http`, `bingmaps:`, `file:`, `javascript:`, `ms-*`,
  `data:`, UNC paths, or anything else. (`bingmaps:` is an open question, not v1.)
- The host is exactly one of the five below (`www.ups.com`, `www.fedex.com`, `tools.usps.com`, `www.dhl.com`, `www.bing.com`) and nothing else: no
  userinfo (`@`), no port, no subdomain trick, no non-ASCII or punycode host.
- The path is exactly the template's path for that host.
- The query holds only the template's keys, values are `[A-Za-z0-9._~%+-]`, and
  there is no fragment, no backslash, no whitespace or control character.
- Total length is at most 512 bytes.

**The executor never trusts the proposal's `url` field.** The proposal carries
`{kind, value}`; `open_url` rebuilds the URL with the same pure function, checks
it equals the URL the card showed, validates it, then calls the shell. A mismatch
or a failed validation is an error card and nothing opens (preview equals
execution). The shell call is `ShellExecuteW` with verb `open`, the URL as the
file, no parameters, no working directory, never `runas`. It runs on the
`spawn_executor` worker (a slow shell extension must not block the message loop),
behind an injectable `Opener` trait like `connectors/ics.rs`, so tests never
open a browser.

## How it runs

1. **Entry.** Palette row, tray item "Open what's on screen", the options card,
   or a pinned `primary_action`. Gates first, before anything is read: Paused
   (`actions::extract_text::gate`), then `refuse_if_password_manager`.
2. **Where detection runs: selection first, then OCR** (`run_selection_first_action`).
   The worker reads the selection through `inputs::selection`; if it holds a
   candidate, no screenshot is taken. Otherwise the main thread captures after
   hiding the card, and the worker OCRs (`ocr::serialize_lines`) and runs
   `detect`. Both off the UI thread; the hook thread is never involved.
3. **Zero candidates:** an info card, "Nothing to open found." (rule 7: never a
   silent no-op). **One:** the confirm card. **Several:** a static options card
   of candidates (no router, no network), then the confirm card for the pick.
4. **Confirm card.** Headline "Open UPS tracking?" / "Open this address in
   Maps?" / "Look up flight AA 1234?". Detail is the full URL. Buttons: Open page,
   and Esc or the key to dismiss. No Edit: the user cannot change a URL Wingman
   vouches for. The detail also says where the value goes ("The number is sent
   to ups.com in your browser").
5. **Do.** Enter runs `open_url`. The outcome card says "Opened in your browser"
   with the URL; there is no undo.

## How it enters the options card

The options card's rows are action ids dispatched through
`dispatch_palette_action(action_id, line, trigger)`. That call already takes a
`line`, so a detected row is the action id `open-on-screen` plus the candidate's
raw text as `line`. Running it re-runs `detect(line)`, which finds exactly that
one candidate, so a stale click needs no cached state beyond the card's existing
options generation.

- A row's label names the thing, sanitised and clamped to 40 characters like every
  router label: "Track UPS 1Z999AA10123456784", "Map 12 Main Street".
- **Detected rows take at most 3 of the 4 slots**, best position first, so the
  router (or the floor) keeps at least one. With several kinds present, order is
  tracking, flight, address.
- Without a model the rows are the same: detection is model-free, so they join
  the static card.
- Detection on a key press needs OCR text before the router answers. That is
  exactly #331's job (match locally, rank instantly), so this spec does not run
  OCR on the press itself.

## Fit with triggers (#331)

#331's triggers are `regex:` and `words:` strings. A check digit cannot be a
regex, so this spec adds a third form, `detector:<name>`, that calls a built-in
pure function in the same hard time budget and returns spans. `open-on-screen`
would declare `triggers = ["detector:open_on_screen"]`. The pure `detect` has
the signature #331 needs, so nothing here has to be reworked when it lands.

**v1 does not depend on #331.** It ships as a catalogue action reachable from the
palette, tray and a pinned key. The router can offer it by name. Triggers make
it appear first on the card without a model, in a later slice.

## Config

```toml
[open_on_screen]
tracking = true
flight = true
address = true      # each kind can be switched off
```

All optional with defaults, `#[serde(default)]`. A kind switched off is not
detected and its URL builder is not reachable from `open_url`.

## Failure cards

| Case | Result |
|---|---|
| Paused, or a password manager in front | The existing gate cards, before anything is read |
| Nothing found | Info card "Nothing to open found." |
| OCR missing language pack or timeout | The existing OCR messages |
| URL fails `validate` or differs from the card | Error card "Wingman would not open that link." Nothing opens |
| `ShellExecuteW` returns 32 or less | Error card with the shell's reason ("No browser is set up") |
| User presses Esc, the key, or clicks away | Dismissed, nothing opens |

## Testing

Pure logic is unit-tested (rule 8); tests come first.

- Detectors: each table row as a positive fixture (UPS sample, valid and
  invalid check digit, a USPS 22-digit, a FedEx 12 with and without a context
  word, a DHL 10-digit mod 7), plus near misses: a US phone number in four
  formats, `Order #123456789012`, an invoice number, a 16-digit card number
  (overlaps redaction), `PO 1234`, `5 miles per hour`, a price. Multi-byte text
  never panics and spans are on char boundaries. Input over the cap is clamped.
- `url.rs`: golden URL for each kind; `validate` rejects `http://`, `javascript:`,
  `file:`, `bingmaps:`, a userinfo host, a port, a look-alike host, an IDN host,
  a CRLF in a value, a backslash, a fragment, an extra query key, over 512 bytes.
- Executor: with a fake `Opener`, a confirmed proposal opens exactly the rebuilt
  URL; a tampered `url` field is refused and the opener is never called;
  `resolve("open_url")` has `Effect::Writes`; `auto_confirm_read_only` refuses it.
- A source-scanning guard (CRLF-normalised, candidate file set asserted
  non-empty) that `open_on_screen/` imports no `provider`, `egress` or
  `ureq`, and that `ShellExecuteW` in `executors/open_url.rs` takes only a value
  that came from `url::build`.
- Dispatch: the arm exists for `open-on-screen`; the options row id plus `line`
  re-detects the same candidate.
- Tests use no real browser and no named kernel objects (rule 9).

### Manual checks (each names the observable that differs if wired to nothing)

1. **UPS.** Put `1Z999AA10123456784` in Notepad, press, pick, Enter. Observable:
   the default browser opens a tab whose address equals the card's URL.
2. **Address.** `1600 Pennsylvania Ave NW, Washington, DC 20500` opens the Maps page.
3. **Near miss.** `Order #123456789012` and a phone number show "Nothing to open
   found." Observable: no confirm card.
4. **Templates.** Open each carrier and flight URL by hand with a real number and
   record that the page loads and the number is pre-filled. Until then the templates
   are THEORY.
5. **No network from Wingman.** Egress log has no new row before or after Confirm.
6. **Pause and password manager.** Each shows its gate card, nothing is read.

## Slices

1. **Pure core.** `open_on_screen/` detectors, `url.rs`, tests. No wiring.
2. **Executor and action.** `open_url`, registry entry, `open-on-screen`
   catalogue entry, `DispatchTarget::OpenOnScreen`, confirm card text, tray item.
   Done when Manual checks 1, 3, 5 and 6 pass.
3. **Candidate rows** on the options card (needs the options card, slice 2 there).
4. **Trigger** `detector:` form, when #331 lands.

Size: no new crate or `windows` feature (`ShellExecuteW` is already used).
THEORY (unverified): about 12 KB, mostly the airline list and scanners.

## Open questions for the owner

1. Flight page: a Bing search (no airline-code conversion, works for any airline), FlightAware (needs IATA to ICAO mapping), or Google?
2. Address to Bing Maps puts the address in front of Microsoft. Acceptable, or use OpenStreetMap?
3. Addresses US and English only in v1: agreed?
4. Carriers in v1 (UPS, FedEx, USPS, DHL): add Royal Mail, Canada Post or S10 international (`AA123456785US`)?
5. Allow `bingmaps:` (opens the Maps app) in a later slice, or keep https only?
6. Detected rows may take 3 of the 4 slots: right balance against router rows?
7. Selection first, then screen: or always read the screen?
