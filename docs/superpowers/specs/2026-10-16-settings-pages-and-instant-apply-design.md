# Settings: five pages in a left rail, changes apply instantly (#829)

Status: **accepted, 2026-10-16**

Scope: `src/ui/settings.rs` (layout, navigation, commit rules), a config
writer module (`src/config_writer.rs`), `src/ui/theme.rs` (layout tokens),
`src/app.rs` (how Settings is hosted and told about a change), the gallery
states. Closes the layout half of #829 and folds in #821 (Mode and Pause),
#823, #826 (setup leaves the tray), #781, #782 (items 1 and 2), #785, #828 and
the open parts of #427 and #585 that touch rows. Out of scope: what any
setting means, dark and high-contrast paint (its own accepted spec), the
Actions page's real content (#48, #51), any WebView2 window.

## Why a design doc (rule 12)

A modal window that returns a whole `Config` on Save becomes a window that
writes single fields as they change. That alters who owns the config file, how
`App` hosts the window, and the keyboard order of dozens of controls.

## Decisions

- **Decision: Settings stays native Win32.** No WebView2 for Settings; #44 and
  #51 do not apply to it. The later main window (#51) is a separate surface.
- **Decision: a left navigation rail with five pages**: General, Models,
  Privacy, Actions, Advanced, one page at a time, like the Windows 11 Settings
  app.
- **Decision: changes apply instantly, with no Save button.** A typed API key
  is checked before it is written.
- **Decision: Settings never overwrites a hand edit of `config.toml`.** It
  applies only the fields the user touched onto the file as it is on disk now.

## Where it is today (master, 2026-10-16)

- One 620 dp page of eight group boxes; about 14 provider controls come before
  anything about what Wingman does. `show_modal` runs its own `GetMessageW`
  loop and returns `Some(Config)` on Save; `App::open_settings` then replaces
  `self.config` and calls `Config::save`.
- While that loop runs `App` holds `&mut self`, so main-window messages wait in
  `PENDING_MESSAGES` (#213). That is why Quit waits for Settings (#673) and
  why no change can reach the tray before the window closes.
- `Config::save_to` re-serialises the typed `Config` with `toml`: comments,
  unknown keys and key order are lost on every save. #711 added a one-generation
  backup and a refusal when the file changed since it was read (#851 applies
  it to Save). That prevents a clobber; it does not make a partial write.
- Mode, the pause key, Pause and Import/Export of actions live in the tray only
  (#821, #826). Learn mode knows two targets (`HK_PRIMARY`, `HK_SECONDARY`).
- `build_ui_recorded` creates every child once with a design rect
  (`ChildLayout`), re-lays out on `WM_DPICHANGED`, and a `WS_VSCROLL` body
  scrolls (#481). Kept.
- A typed key already triggers a model-list fetch on `EN_KILLFOCUS`
  (`fetch_for_typed_key`, #585): worker thread, generation counter,
  `key_fits_vendor` shape check. Validation reuses it.
- Manifest v6 (#839) is in; plain labels (#851) and one field column (#850)
  are landing, and this spec is written against them.

## The model

1. **Page model (pure).** A `Page` enum and `page_rows(page) -> Vec<Row>`; a
   `Row` has a title, an optional description, a control kind and the `Field`
   it edits. A test asserts every `Field` is on exactly one page.
2. **Layout (pure).** `page_layout(page, width_dp, dpi) -> Vec<RowRect>` stacks
   rows: title and description left, control right, a 1 px separator, no group
   boxes. Every size is a `theme.rs` token, which also retires the module's own
   `MARGIN`, `ROW_H` and `GROUP_GAP` (#828, last item).
3. **Commit (impure, small).** `commit(field, value)` sends a `FieldChange` to
   the writer and updates the row state.

All children of all pages are created once at open (stable ids, the existing
`ChildLayout` list), tagged with their `Page`. Switching pages hides the old
children (`SW_HIDE`) and shows and lays out the new ones from the top of the
body. Hidden controls are not tab stops and are not in the UIA tree, so Tab and
Narrator only see the current page.

## Page contents

Control ids are today's.

**General** (how I use it)
- Mode: Cloud, On this PC, Automatic, Offline (new, #821), first row. Labels come
  from `Mode::label` so the tray cannot drift; the description says what leaves
  the PC in that mode.
- Pause: a "Pause..." button with the three choices (one hour, until tomorrow,
  until I resume); while paused, "Paused until 3:40 PM" and Resume. Pause is not
  in `config.toml` (`pause.rs`); the row sends the tray's own command, so there
  is one pause path.
- Wingman key `ID_CHANGE_PRIMARY`, Quick Ask shortcut `ID_CHANGE_SECONDARY`,
  Pause shortcut (new `HK_PAUSE` target; the tray's "Set pause key" moves here,
  #826), with the hotkey status line.
- Start with Windows `ID_AUTOSTART`; card corner `ID_CARD_CORNER`; card monitor
  `ID_CARD_MONITOR`; hide the card after `ID_CARD_SECONDS` (a number box, so a
  hand-edited value is never rewritten); text size `ID_TEXT_SCALE_TRACK`.

**Models** (what answers)
- Active provider `ID_ACTIVE_PROVIDER`; under it only that provider's key (eye
  toggle, #427), model and thinking time. "Add another provider" shows the
  other two in place; a vendor that has a key is always shown.
- Ollama card: status, model `ID_OLLAMA_MODEL`, Pull (`ID_PULL_NAME`,
  `ID_PULL_BUTTON`, `ID_PULL_STATUS`), the link or fix button.
- "Missing a model? Add it in the config file." with Open config file (#851).

**Privacy** (what leaves this PC)
- Hide card numbers, keys, emails and passwords `ID_REDACT`; screenshot detail
  `ID_MAX_EDGE`; which screen is captured `ID_MONITOR`; Review before filling
  `ID_REQUIRE_TICK_FOR`; a "Copy egress log" link (the tray's command).

**Actions**: a short explanation, Open actions file, Import action, Export
action (moved from the tray's Help group, #826). It is the slot #48 and #51
fill later, so it is never an empty page.

**Advanced**: Check my work prompt (toggle, scrolling multi-line box, Restore
default, one line on what it is for), Open config file, the diagnostics entries
the tray holds today. No "reset everything" (Open question 3).

## Navigation

- **The rail** is a single-select `LISTBOX` (`LBS_NOTIFY | LBS_OWNERDRAWFIXED`),
  named "Settings pages", 40 dp items with a Segoe Fluent Icons glyph, the name
  and a 3 dp accent pill on the selected one. One Tab stop. Up and Down change
  the page at once (selection follows focus, as Windows 11 does), Home and End
  jump, Enter or Tab goes into the page. The listbox gives the list and
  selection UIA patterns and type-ahead for free.
- **Anywhere:** Ctrl+1 to Ctrl+5 pick a page, Ctrl+PageDown and Ctrl+PageUp
  cycle, F6 moves between rail and page, Esc closes (see Commit).
- **Tab order:** rail, the page row by row (title before its control), Close.
  Z-order matches, so Narrator reads a row's title before its control (#781).
  A test walks the page model and asserts the order and that no two controls on
  a page share an accessible name.
- **Alt mnemonics** belong to controls in a page, never to the rail, so the two
  never compete: each label carries an `&`, unique within its page (tested; a
  letter may repeat across pages because hidden pages are unreachable).
  Repeated labels carry the vendor ("OpenAI model", "Show OpenAI key").
- **Narrator:** the page heading is a static, first in tab and z-order, named
  for the page. Changing page moves no focus; Narrator reads the selected rail
  item. The status line is a plain, non-disabled static that raises
  `EVENT_OBJECT_LIVEREGIONCHANGED`, so "Saved: Card corner" and every error are
  announced (#781). The hotkey status line becomes non-disabled too (#824).
- **Role of the owner-drawn rail is not assumed.** The dark spec measured that
  owner-draw keeps a combo's role and drops a checkbox's; a listbox is
  unmeasured. THEORY (unverified): it keeps ListBox and ListItem. Slice 2
  measures it with UIA first; if the role is lost, the stock listbox is the
  fallback in every theme.

## Instant apply

### What commits, and when

| Control | Commits on |
|---|---|
| Checkbox, combo | `BN_CLICKED`, `CBN_SELCHANGE` |
| Trackbar | release (`TB_ENDTRACK`); the value label follows the drag |
| Text, number and key edits | `EN_KILLFOCUS` and Enter, never per keystroke |
| Hotkey Change | the captured chord, after the existing `judge_learned` check |
| Pause, Pull, Open config file | actions, not fields |

**No idle timer.** A debounce timer on a window left open all afternoon is the
polling rule 5 forbids; focus loss and Enter fire on their own. Two edges:
closing commits the focused edit first (`WM_CLOSE` calls `commit_focused()`,
because a destroyed window does not reliably send `EN_KILLFOCUS`), and a rail
click commits it because the click takes focus. **Esc in an edit with pending
text** restores the last applied value and does not close; with nothing pending
it closes. A number that does not parse is not committed: the row shows its
error and the text stays for fixing, and it is never written.

### The writer

A change is a value, not a `Config`: `enum FieldChange { Mode(Mode),
CardSeconds(u32), Redact(bool), ProviderKey(Vendor, String), .. }` with
`apply(&self, doc: &mut Document)`. One thread, `settings-writer`, blocks on a
channel (no polling) and for each change:

1. **Reads the file fresh.** Missing: defaults. Does not parse: refuses (the
   #711 rule) and the row says so.
2. **Patches one key** with `toml_edit`, leaving every other byte alone:
   comments, unknown keys and order survive, and so does anything the user
   hand-edited since the window opened (Open question 1 covers the dependency).
3. **Writes atomically** with the existing temp file, owner-only ACL, backup and
   rename, factored out of `Config::save_to` so both share one path, plus the
   #711 mtime check against the read in step 1. If the file changed between
   read and rename it re-reads and retries once, then fails.
4. **Keeps keys apart.** `ProviderKey` goes to Credential Manager
   (`push_secrets_to_store`), never the file; a store failure leaves the file
   untouched and the row says the key was not saved. Key text is never logged,
   put in a report or kept for undo.
5. **Reports back:** a `WM_APP` message to Settings (field, ok or a plain
   reason) and, on success, one to the main window so `App` reloads from disk
   and runs `apply_config` quietly (no "Settings reloaded" card). Mode, tray
   check marks, tooltip, hotkeys and the chain already hang off `apply_config`,
   so no per-field apply code is added.

Changes to one field coalesce (latest wins); different fields keep order. All
file and `icacls` work is on this thread: nothing in Settings touches disk or
network on the UI thread, which also hosts the keyboard hook.

### Row state, Undo, errors

A row is `Idle`, `Checking` (keys), `Saved` or `Failed(reason)`. `Saved` shows
in the status line as "Saved: Card corner" with **Undo**, which restores the
previous value through the same writer. Undo covers the latest change only and
never a key. Fields that must change together (active provider and its order
entry; a pulled model and the Ollama box) are one `FieldChange`, so a half pair
is never written.

**Validation** is inline under the control, in the error token and prefixed
"Error" (never colour alone), e.g. "Enter a number from 0 to 3600"; it also sets
the status line so Narrator announces it. Nothing is written while a row is
invalid.

**A write failure** (disk full, file locked, ACL step failed, file does not
parse, changed twice): the control snaps back to the value on disk, the row says
"Not saved: <plain reason>. Wingman is still using the old value.", the failure
goes to diagnostics, and the window stays usable. Rule 7 wants a card, but one
cannot sit on the window being typed in (the app hides cards when it opens). So:
inline while open, and if a row is still failed at close, one card "Couldn't
save N settings" through the existing error-card path.

### A typed key is checked before it is written

On commit of a key field:

1. **Shape**, pure, on the UI thread: `key_fits_vendor`. A wrong shape fails at
   once ("That is not an OpenAI key. OpenAI keys start with sk-") and is not
   sent anywhere, so a key pasted into the wrong vendor's box never leaves the
   PC (#678).
2. **A live check on a worker thread**, row `Checking` ("Checking key...", the
   rest of the window usable). It is the model-list request made when Settings
   opens, `model_list::fetch(vendor, key)`: read-only, and its answer fills the
   Models page anyway. A per-vendor generation counter (as `CloudSync`) drops a
   result for a key since replaced. It goes through the guarded transport, so
   the Offline guard and the egress log see it.
3. **Outcome.** `2xx`: write the key and fill the model box. `401` or `403`: do
   not write; "OpenAI did not accept that key. Check it and try again." A request
   that cannot complete (no network, timeout, `5xx`, rate limit): write it and
   say "Saved. Could not check it right now." (Open question 2). Offline mode
   makes no request: shape-checked, saved, "Saved. Not checked in Offline mode."
4. After a successful write the box shows the mask (`mask_key`) as today; a
   rejected key stays typed so it can be corrected.

### Staying in step with the file

Settings re-reads on `WM_ACTIVATE` (event-driven; one `metadata` call, a parse
only if the mtime moved) and refreshes rows that are neither focused nor being
edited, so a hand edit shows up. Writes never depend on this: they always patch
the current file, so a stale display cannot write a stale value.

## Hosting: modeless

`show_modal` goes away. `settings::open(instance, main_hwnd)` creates a modeless
window; `App` enters no nested loop and holds no `&mut self` across it. The main
pump gives it `IsDialogMessage`, the single-window guard (`OPEN_HWND`) stays,
and `WM_APP` replies are ordinary messages to the main proc.

- A change reaches `App` while the window is open, which instant apply needs.
  The #213 deferral keeps one job: a card that would land on the window waits
  until Settings is left. A press of the Wingman key made meanwhile is refused
  with a short card, not kept (Open question 4).
  Quit, session end, tray and config apply are handled on arrival, closing #673.
  Quit closes the window; what was typed but not saved is dropped with it, as
  it was when the window was modal, and a Save already pressed is written
  before the process exits.
- `open_settings`' save branch (`self.config = edited; save`) and the "Settings
  saved" card are deleted. The tray keeps its Mode submenu and calls the same
  `App::set_mode`.

## Composing with the dark and high-contrast spec

That spec changes how controls are *painted*; this one changes where they *are*.

- **No new colour.** Rows, separators and the rail use `Tokens` and `SysColors`;
  row controls use the dark spec's `control_fill` and `control_border`. The
  rail's pill is `accent`; high contrast uses `COLOR_HIGHLIGHT`.
- **The rail is custom-drawn in light mode too**, which that spec forbids for
  existing controls. Its rule protects a stock baseline from regressing; a new
  control has none, and a stock listbox as the navigation of a Windows 11 style
  window would look wrong in every theme. High contrast draws it from
  `GetSysColor` only.
- **Group boxes disappear** (hairline-separated rows, #785, #828), so the dark
  spec's group-box paint subclass is not needed and its slice 2 shrinks to
  buttons and checkboxes.
- **Edit frames, combos, trackbar, scrollbar** are unchanged: every page uses the
  same classes, so the dark slices cover every page, and per-page gallery
  pictures are the proof. Live re-theme repaints the rail and status line from
  the one resolve in the top-level proc.
- **Order.** Slices 0 and 1 are independent of the dark slices. From slice 2 the
  dark work lands on the new layout; the dark spec's plumbing slice is
  layout-independent and can ship first. `SETTINGS_DARK_ENABLED` stays false
  until that spec's last slice.

## DPI and size

Layout is dp through the existing `to_px`; the rail (188 dp), row heights (56
with a description, 40 without) and the control column are new `theme.rs`
tokens. The window is 840 dp wide and as tall as the fullest page, clamped to
the work area as today. **No page scrolls at 100 % and 150 % on a 1080p screen**
(#829's done condition); from 200 % the body scrolls and the rail does not (five
items need 200 dp). `WM_DPICHANGED` re-lays out hidden children too, so a page
shown later is right.

## Gallery states

Replace `SettingsTop`, `SettingsBottom`, `SettingsAdvancedTop` and
`SettingsAdvancedBottom` with `SettingsGeneral`, `SettingsPaused`,
`SettingsModels`, `SettingsModelsMoreProviders`, `SettingsModelsOllamaPulling`,
`SettingsPrivacy`, `SettingsActions`, `SettingsAdvanced`,
`SettingsAdvancedPromptOpen` and `SettingsRowStates` (one row each of Checking,
Saved with Undo, invalid number, rejected key, failed write). They follow
`--dpi` and, once the dark spec ships, `--theme`; until then `follows_dpi_only`
lists them. A test asserts every `Page` has a state. A slice is not done until
its pictures at 100 % and 150 % have been looked at.

## Migration from the single window

No config field is removed; each keeps its control, now on a page. A user sees a
larger window with a rail, no Save or Cancel, and no hotkey learners in the tray.
Someone who expected Cancel to discard finds Undo; the "Settings saved" card
becomes the inline "Saved: <field>" line. `CHANGELOG.md` says so in the release
that ships slice 5.

## Testing

- **Pure:** the page model (every `Field` once, mnemonics unique, tab order),
  `page_layout` at five DPIs (no overlap, no overflow at 100 and 150 on 1080p),
  the commit table, `FieldChange::apply` over a document with comments, unknown
  keys and a hand-edited unrelated field (all survive byte for byte), and the
  key decision as a pure function `(mode, shape_ok, response) -> (write?,
  message)`.
- **Writer, temp path (rule 9):** file parses, is missing, does not parse (no
  write), changes mid-write (one retry, then failure), ACL step fails (aborted),
  two rapid changes (order, coalescing).
- **Source scans** (CRLF-normalised, non-empty candidate set): no Save button id
  remains; no `SetTimer` in `settings.rs` beyond the learn timeout and existing
  one-shots; no `Config::save` or file I/O on the UI thread there; no literal
  pixel constant in layout code. **Dispatch guards:** arms for rail selection,
  writer replies, `WM_ACTIVATE` and Ctrl+digit.
- **Manual, named in each PR:** the slice's observable, keyboard only, Narrator
  on General and Models, real high contrast.

## Slices (one PR each; each names its observable, rule 8)

0. **Config writer.** `FieldChange`, the writer thread, the shared atomic write;
   the tray's Mode change uses it. *Observable:* put a comment and an unknown key
   in `config.toml`, switch Mode from the tray: both remain and only `mode`
   differs. With the writer unplugged the comment is gone.
1. **Modeless host.** `settings::open`, no nested loop; Save sends only the
   touched fields through the writer. *Observable:* with Settings open, tray Quit
   exits (#673); hand-edit an unrelated key, change card seconds, Save: the hand
   edit survives.
2. **Rail and five pages.** Rail, page model and layout, existing controls on
   their pages with today's labels, Ctrl+1 to 5, UIA role measured. *Observable:*
   the redaction checkbox exists only on Privacy and Ctrl+3 shows it; each page's
   gallery picture needs no scrolling at 100 and 150 %.
3. **Mode, Pause, pause key** on General (#821, #826 part). *Observable:* pick
   Offline in Settings and the tray's Mode check mark moves with Settings still
   open; Pause greys the tray icon and Resume restores it.
4. **Models page.** Active provider rows, "Add another provider", Ollama card,
   eye toggle, model-list hint (#829, #427, #585 hint). *Observable:* with one key
   the page shows three rows; "Add another provider" reveals the other two.
5. **Instant apply.** Remove Save and Cancel; commit table, row states, Undo,
   `WM_ACTIVATE` refresh, close-time card, Esc rules. *Observable:* change Card
   corner and read `config.toml` before closing: the value is there and the next
   card appears in that corner. Without the commit arm the file does not change.
6. **Typed key validation.** Shape, live check, outcomes, Offline. *Observable:* a
   key with a wrong last character is rejected and Credential Manager is
   unchanged; a good key is stored and the model box fills from the same answer.
7. **Rows.** Title and description rows, no group boxes, a seconds choice list
   that keeps a custom value, names and mnemonics, live-region status, Actions
   content (#823, #781, #785, #826, #828). *Observable:* Narrator reads "OpenAI
   model" then the combo; no `BS_GROUPBOX` remains in `settings.rs`.
8. **Verification and docs.** Narrator on every page in dark and high contrast,
   real high contrast at 100 and 200 %, release size recorded, the `settings.rs`
   module doc and `docs/README.md` rewritten, `CHANGELOG.md`.

**Size.** No new `windows` feature is expected (`NotifyWinEvent` is
`Win32_UI_Accessibility`, to confirm in slice 7). `toml_edit` (MIT or Apache-2.0)
is the one possible dependency. MEASURED 2026-10-16 (slice 0, #870, same
machine, release profile): 4,055,040 bytes on master, 4,143,104 with
`toml_edit` and the writer, a delta of 88,064 bytes (about 86 KB), less than the
100 to 150 KB guessed. `toml_edit` shares `toml_parser`, `toml_datetime` and
`winnow` with `toml`, so it adds no second parser. The gate (4,500,000) is not
raised: master measured 3,982,336 on CI, so the writer lands near 4,070,000. That
leaves about 430 KB for the options card, overlay and knowledge work.
`settings.rs` should shrink (modal loop, group boxes go).

## Adversarial review: the five strongest objections

1. **"Modeless Settings reopens reentrancy bugs the modal loop hid."** Real:
   `App` methods now run while the window exists. Resolved: the window never
   borrows `App` and talks only by posted messages, which the main proc handles
   like tray messages; the Ask deferral stays so no card lands on it. It is its
   own slice (1) with Save unchanged, so a regression is isolated, and its
   observable (Quit works) is one the modal loop could not give.
2. **"Preserving comments adds a dependency to a 3.5 MB exe."** Real, and the
   rule is "never overwrite hand edits". Resolved: patch one key with
   `toml_edit`, a test proves comments, unknown keys and order survive, and slice
   0 measures the size and raises the gate if needed. If refused, a typed round
   trip keeps every known field but not comments, with the existing backup
   (Open question 1).
3. **"Checking a key against the vendor contradicts 'validated before written'
   when the network is down."** Resolved: only a definite rejection (401 or 403)
   blocks a write. A transport failure writes and says it was not checked,
   because refusing a correct key over a flaky connection is worse and the key is
   used only by a later request. Offline mode makes no request. The rule is a
   pure function with a test per case (Open question 2).
4. **"No Save means a slip is permanent, and the window, tray and a hand edit
   will race."** Resolved: each change is one field, shown as "Saved: X" with
   Undo for the latest (never keys, whose old value is not kept). The writer
   reads fresh, patches one key, checks the mtime and retries once, so a race
   costs at most one field and never the file; activation refreshes untouched
   rows so the window does not sit on stale values.
5. **"A custom-drawn rail breaks screen readers and contradicts the dark spec's
   'stock in light'; five pages hide controls from Narrator."** Resolved: the rail
   is a real listbox (selection patterns, type-ahead and arrow keys are the
   system's); slice 2 measures its UIA role before more is built on it and falls
   back to a stock listbox if it is lost; the dark spec's reason (a baseline to
   regress) does not apply to a new control; high contrast uses system colours
   only. Hidden pages are `SW_HIDE`n, so they leave the Tab order and UIA tree.

## Open questions

1. **Add `toml_edit` for comment-preserving writes?** Recommendation: yes. It is
   the only way to keep comments and unknown keys, and slice 0's measured size
   decides the gate. If too costly, ship the typed round trip and document that
   comments are not kept.
2. **A typed key that cannot be checked (no network, timeout, 5xx): write or
   hold?** Recommendation: write it and say "Saved. Could not check it right
   now." Only a definite rejection stops a write.
3. **A "Restore defaults" button per page?** Recommendation: not in this spec.
   Undo covers a slip, and a reset needs its own account of what it keeps (keys,
   hotkeys, actions). File it if people ask once the pages ship.
4. **Should the Wingman key still ask while Settings is the foreground window?**
   Decision: no. On the modal window the key was inert (the press was dropped),
   so there was nothing to keep. A press made while Settings is in front, or
   while a Settings save is still being applied, is refused with a short card
   ("Settings is open" or "Saving your settings"). It is not queued: run later it
   would capture another window, bill a request, and could use settings from
   before the save (an Offline switch ignored once). Answers that finish while
   Settings is in front are held and shown when it is left (#213).
