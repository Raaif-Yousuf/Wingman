# Local store: SQLite schema, migrations and what moves in

Status: **draft.** Issue #46. Supplements the
[expansion plan](2026-09-16-expansion-plan-design.md) (`store.rs` row of the
module table, section 18 Knowledge and memory) and the
[running app spec](2026-09-14-copilot-ask-design.md).

## Why this is a spec and not a PR

The store is the one place every later feature writes to: usage, the egress
log, the profile, memory, and the whole knowledge track (#81 to #88). It picks a
dependency that adds C code to the build, decides what is encrypted and what is
searchable, and fixes the shape of data users will have on disk for years.
Migrations are forward-only, so a wrong first schema is paid for forever. That is
architecture (AGENTS.md rule 12), so this stops at the design. Open questions are
at the end, each with a recommendation.

No code is part of this change.

## What exists today

Three ad-hoc files, each with its own read, write, lock and cap logic:

| data | file | shape | notes |
|---|---|---|---|
| spend | `%LOCALAPPDATA%\Wingman\spend.json` | one JSON object keyed by local date: `usd`, `requests`, `unpriced`; newest 60 days kept (`usage.rs`) | read-modify-write behind a process mutex |
| egress log | `%LOCALAPPDATA%\Wingman\egress.log` | one JSON object per line (`EgressEntry`); capped at 2 MB by dropping the oldest lines (`egress.rs`) | owner-only ACL via `icacls` once per process (#277); `snippets` always empty, `cost` always `None` |
| profile | `%APPDATA%\Wingman\profile.bin` | the whole `Profile` as JSON inside one DPAPI envelope (`dpapi.rs`, magic `WMDP`), plus a one-shot import of a hand-edited `profile.toml` | written atomically; every write passes the payment denylist |

`egress.rs` already says "when #46 lands, this file's rows are exactly the
columns that table needs", and `usage.rs` and `profile/mod.rs` name #46 the same
way. What none of them can do: query by date or provider, join a request to the
knowledge snippets it carried, delete one source's data in one step, or migrate
their own format.

The issue text lists `conversations` and `messages` tables. Wingman has no chat
(AGENTS.md: one press, one action, one card), so this spec leaves them out. The
issue text should be updated to match.

## Goals and non-goals

Goals: one database file; versioned forward-only migrations with a test per
step; the three existing files move in without losing data; the knowledge and
memory tables have an agreed home and shape; every failure ends in a card; nothing
runs while idle.

Non-goals: the Memory page, the Profile page, the ingest pipeline, embeddings and
retrieval ranking (their own issues; this spec only reserves their tables and
rules); the awareness `context` table (Phase 4, #61; it will arrive as a later
migration under the same rules); sync between machines.

## Crate choice and licenses

| piece | license | role |
|---|---|---|
| `rusqlite` | MIT | the binding |
| `libsqlite3-sys` (with rusqlite's `bundled` feature) | MIT | builds the SQLite amalgamation into the exe |
| SQLite itself | public domain | the engine |
| `cc` (build-time only, already a transitive dependency of other crates on this graph) | MIT OR Apache-2.0 | compiles the C |

All of these fit AGENTS.md rule 2. `rusqlite` and SQLite were checked against
the expansion plan's 2026-09-16 table (rusqlite MIT). Not re-verified for this
draft because crates.io was not consulted; the adoption slice runs
`cargo deny check` and records the result in `THIRD_PARTY_NOTICES.md`, naming
SQLite's public-domain status explicitly since it is not in `deny.toml`'s
allowlist and `cargo deny` will not see it (it is C source inside
`libsqlite3-sys`).

**Bundled, not system.** Windows ships `winsqlite3.dll`, but its version and
compile options are not ours to choose, and FTS5 is not guaranteed there. A
bundled build pins both. Cost: a C compile in the build (MSVC, already required
for the `windows` crate) and a larger exe.

**FTS5.** THEORY (unverified): the `bundled` feature compiles SQLite with FTS5
enabled. The first slice settles it with a test that runs
`CREATE VIRTUAL TABLE t USING fts5(x)` and a `MATCH` query. If it fails, the
fix is a `LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_FTS5` build setting, and that goes in
the slice.

**Binary size.** Not measured. THEORY (unverified): SQLite with FTS5 adds on the
order of 1 MB at `opt-level = "z"` with LTO, against the roughly 2 MB release exe
AGENTS.md records, so the exe could plausibly grow by half. The first slice
records the exact before and after in its PR (release build of master versus the
branch) and AGENTS.md's number is updated from the result. Options if the number
is unacceptable: `SQLITE_OMIT_*` flags for features Wingman never uses (loadable
extensions, JSON1, deprecated APIs, the shell), set through `LIBSQLITE3_FLAGS`.
The exe is not the only budget: the plan's section 19 mentions performance
gates (exe size, startup, idle RSS); this change must fit whatever those gates are
when they land.

**Async.** `rusqlite` is blocking and synchronous. No runtime is added.

**Vector search.** The plan and #85 name `sqlite-vec`. The options:

| option | license | how it works | cost |
|---|---|---|---|
| `sqlite-vec` | MIT OR Apache-2.0 (per the plan's table and issue #85; re-verify at adoption, the plan itself records two projects that relicensed) | C extension registered at startup; `vec0` virtual table | another C build, an `unsafe` registration call, a pre-1.0 version (THEORY: from memory, check) and, to my knowledge, still an exact scan underneath rather than an index, so it is not faster than the next option, only more convenient in SQL |
| BLOB column plus brute-force cosine in Rust | none (our code) | `embedding BLOB` (little-endian `f32`) on the chunk row; a query loads vectors and scores them | works with no extension; the scan is linear in chunks |

Scale check, from arithmetic and not a measurement: nomic-embed-text is
published as 768 dimensions, so 10 000 chunks is about 7.7 million
multiply-adds and 30 MB of vectors. THEORY (unverified): that scores in tens of
milliseconds on the dev laptop, small next to a model call. The slice that adds
vectors measures it on real data before the choice is final. The design isolates
the choice behind one trait (`VectorIndex`: `upsert`, `remove_doc`, `nearest`),
so changing it later touches one file and, if the vectors move, one migration.
Recommendation in the open questions.

## File location, ACL, pragmas

`%LOCALAPPDATA%\Wingman\store\wingman.db`. Local, not roaming: it can be large
and its encrypted values are bound to one machine's user key anyway. The sparse
package keeps `%LOCALAPPDATA%` writes real (AGENTS.md rule 4; a full MSIX would
virtualize them).

The plan says `%LOCALAPPDATA%\Wingman\wingman.db`; a subdirectory is proposed
because WAL mode creates `wingman.db-wal` and `wingman.db-shm` on demand, and
`egress.rs`'s way of restricting a file with `icacls /inheritance:r` does not
cover files SQLite creates later. Restricting the directory with inheritance
(`icacls <dir> /inheritance:r /grant:r "<user>:(OI)(CI)F"`) covers all three and
anything added (snapshots, a `.corrupt` copy). The ACL step is applied when the
directory is created and once per process after that, the same shape as #277's
once-per-process flag. The argument list is built by a pure function and unit
tested; the existing `Config::restrict_acl` is file-only, so this is a sibling
helper, not a change to it. If it fails the store does not open: a card says so
(rule 7), because an unrestricted store is exactly what the rest of this spec
promises against.

Pragmas, set on every connection when it opens:

| pragma | value | why |
|---|---|---|
| `journal_mode` | `WAL` | readers never block the writer; persistent in the file |
| `synchronous` | `NORMAL` | safe against app crashes in WAL; the last commit may be lost on power loss, which is fine for logs and recoverable for the rest |
| `foreign_keys` | `ON` | chunk and fact cleanup by cascade |
| `busy_timeout` | `5000` | a second connection waits, then fails with a card, never hangs the UI |
| `trusted_schema` | `OFF` | defence in depth, nothing here needs it |
| `secure_delete` | `ON` | "forget this document" has to overwrite the pages, not just unlink them; small cost on rows that are rarely deleted |
| `cache_size` | small (about 2 MB) | keeps idle RSS inside the 10 MB budget; grows only while a query runs |
| `mmap_size` | `0` | no mapped file pages counted against RSS |

After any user-initiated delete ("forget", "delete all data"), run
`PRAGMA wal_checkpoint(TRUNCATE)`: a deleted row can otherwise live on in the
`-wal` file until the next automatic checkpoint.

## Threading

The binary has no async runtime and a strict "main thread never panics, worker
catches everything" rule (AGENTS.md rule 7). SQLite in WAL mode allows one writer
and any number of readers at once. Shape:

- One process-wide `Store` handle holding the **write connection** behind a
  `Mutex`. Every write is one short transaction. Callers get the `Result`
  directly, so the failure reaches whichever code can show a card; a dedicated
  writer thread with a channel would turn every write failure into an
  asynchronous message to route back.
- **Read connections** are opened on demand, read-only, by whoever needs a
  consistent read that must not queue behind a writer: the settings window's
  pages and retrieval. They are closed when done.
- A long job (ingesting a large folder) writes per file in its own transaction
  and releases the mutex between files, so the usage write for a user's request
  is never stuck behind an import for more than one file's insert.
- Mutex poisoning is recovered, not propagated (the same choice `usage.rs`
  already makes), because a panic elsewhere must not take history with it.

Idle cost (AGENTS.md rule 5): no thread, no timer, no watcher belongs to the
store. The connection opens lazily on first use. Nothing checkpoints on a timer:
SQLite's automatic checkpoint runs inside a commit. Retention pruning runs inside
the insert that would exceed the limit, or at startup, never on a schedule.
Opening costs one file open at the first use after launch; the tray tooltip
needs today's spend, so that open happens on first tooltip draw. THEORY
(unverified): a warm open and the `user_version` read is a few milliseconds.
Slice 1 measures idle RSS with the store open and idle, and idle power as the
project already measures it.

## Schema

`PRAGMA user_version` holds the schema version (one integer, written inside the
migration's own transaction). A one-row `meta` table keeps everything else about
the database, so a version number never has to carry meaning.

```
meta        (key TEXT PRIMARY KEY, value TEXT)       -- created_at, last_app_version,
                                                     -- imported_spend, imported_egress,
                                                     -- imported_profile, last_snapshot

usage_day   (date TEXT, provider TEXT, model TEXT,   -- local date, like spend.json
             requests INTEGER, unpriced INTEGER,
             input_tokens INTEGER, output_tokens INTEGER,
             cache_write_tokens INTEGER, cache_read_tokens INTEGER,
             usd REAL,
             PRIMARY KEY (date, provider, model))

egress      (id INTEGER PRIMARY KEY, time_unix INTEGER,
             provider TEXT, model TEXT, retried INTEGER,
             attachments TEXT,        -- JSON: kind, bytes, width, height; never content
             snippets TEXT,           -- JSON: source labels, see "Privacy"
             bytes_sent INTEGER, bytes_received INTEGER,
             outcome TEXT, error TEXT, cost REAL)
             -- index on time_unix

profile_field (key TEXT PRIMARY KEY,                 -- the FieldKind name
               value BLOB,                            -- DPAPI envelope, per value
               updated_unix INTEGER)

-- Reserved by slice, shape agreed here, created in their own migrations:

memory_item (id INTEGER PRIMARY KEY, kind TEXT,       -- fact|preference|correction|
             text BLOB, structured BLOB,              --   profile_field|style
             source_kind TEXT, source_ref TEXT,       -- doc|action|user, and the id
             source_span TEXT, confidence REAL,
             created_unix INTEGER, last_used_unix INTEGER,
             uses INTEGER, pinned INTEGER)

document    (id INTEGER PRIMARY KEY, path TEXT UNIQUE, content_hash TEXT,
             size INTEGER, mtime_unix INTEGER, parser TEXT, ingested_unix INTEGER)
chunk       (id INTEGER PRIMARY KEY,
             doc_id INTEGER REFERENCES document ON DELETE CASCADE,
             ordinal INTEGER, text TEXT, span TEXT,
             embed_model TEXT, embed_dim INTEGER, embedding BLOB)
chunk_fts   -- FTS5, external content over chunk.text, kept in sync by triggers
```

The existing types map one to one: `DayTotal` to a `usage_day` sum, `EgressEntry`
to an `egress` row (`attachments` and `snippets` stay JSON because they are
variable-length lists that are read with the row and never queried on their own).

`usage_day` is keyed by provider and model, not only by date, and carries the
token counts, because the plan's `usage.rs` row says "tokens per provider and
model" and a changed price table cannot recompute dollars without them. The
tooltip is `SELECT sum(usd), sum(requests), sum(unpriced) FROM usage_day WHERE
date = ?`. Dollars stay stored (the price a request was billed at is a fact about
that day, not about today's table).

`profile_field` starts as its own small table because `Profile` is typed by
`FieldKind` and a migration that already knows the memory model would be
guessing. When #86 lands, the plan says the profile "becomes a view over
`profile_field` items"; that migration copies each row into `memory_item` with
`kind = 'profile_field'` and drops the old table. One extra copy step is
cheaper than designing the memory columns ahead of the memory work.

Vectors live in `chunk.embedding` with the model name and dimension beside them.
A different embedding model makes old vectors meaningless, so retrieval filters
on `embed_model` and a model change is "re-embed", never a silent mix.

### Derived, authored and logged data

This split drives backup, corruption and delete behaviour:

| class | tables | if lost |
|---|---|---|
| **logged** | `usage_day`, `egress` | gone; acceptable, regenerates as the app is used |
| **derived** | `document`, `chunk`, `chunk_fts` | re-ingested from the Knowledge folder |
| **authored** | `profile_field`, `memory_item` | not recoverable from anything else; backed up and exportable |

## Migrations

- A `const MIGRATIONS: &[Migration]` list, `Migration { version, sql }`, with
  each step's SQL in its own `store/schema/vNNN.sql` pulled in by `include_str!`.
- On open: read `user_version`; apply every step above it in order, each in its
  own transaction that also bumps `user_version`. A step that fails rolls back
  and leaves the database at the previous version, still usable by this build,
  and a card names the step.
- **Forward-only.** A shipped migration is never edited. A mistake is fixed by
  the next migration.
- **Newer than this build** (`user_version` above the highest known): do not
  touch the file, do not downgrade. A card says the data was made by a newer
  Wingman and offers to open the folder. This is the only safe answer to
  "someone downgraded".
- **Snapshot before migrating**: if the file is non-empty and a step is pending,
  take `VACUUM INTO 'wingman.db.pre-vN'` first and keep the newest one. This is
  what makes a bad migration recoverable by hand.
- Version numbers are assigned in merge order. Two slices racing for the same
  number show up as a file-name conflict, and a test asserts the versions are
  contiguous from 1.
- Data-only migrations (the imports below) are not schema steps. They run after
  schema migration, guarded by a `meta` flag that is set in the same transaction
  as the import, so a crash mid-import cannot import twice.

## What moves in

All three imports follow one rule: **one source of truth after the move.** The
old file is read once, its rows are written in one transaction together with the
`imported_*` flag, then the old file is renamed to `<name>.migrated` and is no
longer read. Writes never go to both. The `.migrated` file stays for one release
as a hand rollback, then a later slice deletes it.

- **`spend.json`**: each date becomes one `usage_day` row with `provider = ''`
  and `model = ''` (the old file never recorded them) and the token columns
  `NULL`. Unparseable file: skipped with the data left in place and the flag not
  set, so a later fixed build can retry; the card says so once.
- **`egress.log`**: each JSON line becomes an `egress` row. A line that does not
  parse is counted and skipped, not fatal; the count goes in the same card.
  Retention is applied after the import.
- **`profile.bin`**: loaded with the existing `Profile::load_from` (so the
  payment denylist runs over it like any other read), each field written to
  `profile_field` as its own DPAPI value. The `profile.toml` hand-edit import
  stays in `fill_form.rs` and writes through the store instead of the file. The
  whole-blob `profile.bin` is renamed, not deleted.

Replaced code: the file I/O in `usage.rs` (price table and `apply` stay; the
`FILE_LOCK`, path, read and write functions go), `egress.rs`'s append, cap and
ACL-once logic (the entry building, redaction of opaque tokens and the human
formatter stay), and the `load` and `save` of `profile/mod.rs` (the denylist and
label matching stay). Pure logic is not rewritten, only its persistence.

## Privacy

- **No payment data.** The store's write functions for `profile_field` and
  `memory_item` call the existing denylist (`payment_denylist.rs`,
  `profile::denylist`) before the insert and return a typed refusal that the
  caller turns into a card saying why (#86's done-when). The check lives in the
  store, not in each caller, so no future write path can skip it. A
  source-scanning test (CRLF-normalised, with a non-empty candidate set) asserts
  no module outside `store/` names the store's tables.
- **Egress rows hold no content.** Attachments are kind and size; snippets are a
  source label and chunk id, never the snippet text (see Open questions for the
  trade-off). The `error` text keeps going through `redact_opaque_tokens` before
  it is stored, exactly as today.
- **Encryption at rest.** `libsqlite3` has no encryption, DPAPI cannot be applied
  under a full-text index, and #85 wants both. The options and a recommendation
  are in the open questions. What is fixed here: whatever is encrypted uses the
  existing `dpapi.rs` envelope with a new entropy constant (not
  `PROFILE_ENTROPY`), one value at a time, and the store directory is owner-only.
- **Retention.** `usage_day`: 60 days, as today. `egress`: proposed 90 days and
  at most 10 000 rows, whichever is reached first, pruned inside the insert; the
  current 2 MB cap is a byte limit that does not mean anything for rows, and
  2 MB at the current row size is on the order of thousands of rows. Knowledge:
  until the file is removed or the user forgets it. Memory: never deleted
  automatically; decay only suggests (plan section 18).
- **Export and delete.** Authored data exports to JSON through one function in
  the store and imports back; "forget by source" and "delete all local data"
  are store functions that finish with the WAL checkpoint above. Their callers
  are the Memory and Profile pages (#88, #50), so they ship with those pages and
  not before (rule 8). Until then the documented path is the PowerShell wipe in
  `PRIVACY.md`.
- **`PRIVACY.md`** currently tells the user to remove `%APPDATA%\Wingman` and says
  "there is currently nothing else on disk", but `spend.json` and `egress.log`
  already live under `%LOCALAPPDATA%\Wingman`. The first slice corrects the wipe
  section to name both folders and the store directory.

## Backup, corruption and every failure ends in a card

| situation | behaviour |
|---|---|
| first run, no file | create, set ACL, migrate, import; no card |
| `user_version` newer than this build | untouched; card as above |
| migration step fails | rolled back, still at the previous version; card names the step; the app keeps working on features that do not need the new tables |
| `SQLITE_CORRUPT` or `SQLITE_NOTADB` at open or at runtime | close, rename the file and its `-wal`/`-shm` to `*.corrupt-<unix>`, create a fresh store, card: what happened, where the damaged copy is, that knowledge will be rebuilt from the folder and that profile and memory can be restored from the latest snapshot |
| `SQLITE_FULL`, read-only directory, ACL step failed | card with the plain cause; no partial writes |
| `SQLITE_BUSY` after the 5 s timeout | the write is reported failed; card |
| a write on the request path fails (usage, egress) | the user's request is **not** failed for it; one card per run says "Wingman could not save its history" (rate-limited so it cannot spam), and the full error goes to the diagnostics bundle |

Snapshots of authored data: at startup, if `meta.last_snapshot` is older than
seven days, `VACUUM INTO 'wingman.db.bak'` (the previous one rotates to
`.bak.1`). It is a startup check, not a timer. `quick_check` is not run on every
open (it reads the whole file); it runs when an open or a statement reports a
corrupt-class error. DPAPI values in a snapshot are byte-identical, so a snapshot
is only readable by the same Windows user, which is the intended property.

## Test strategy

- A store takes its path as a parameter (`Store::open_at(path)`); the real
  `store_path()` is `cfg(test)`-switched to a per-process temp path, the pattern
  `usage.rs` already uses, and every test makes its own directory under
  `%TEMP%` (rule 9). No test opens `%LOCALAPPDATA%\Wingman`. Pure SQL tests use
  `:memory:`.
- **One test per migration** (issue #46's done-when): build a database at N
  through the real migrations 1..N, insert representative rows, migrate to N+1,
  assert every row is intact. Plus a schema snapshot per version, so editing a
  shipped migration fails a test. Plus "versions are contiguous".
- Imports: a fixture `spend.json`, `egress.log` (with one corrupt line) and a
  DPAPI profile are imported; row counts, the flag and the rename are asserted;
  a second open imports nothing; a simulated crash (error injected before commit)
  leaves no flag and no rows.
- Corruption and refusal paths: a garbage file, a truncated file, a file whose
  `user_version` is higher than known, a read-only directory. Each must return a
  typed error that maps to a card, and never panic.
- FTS5 availability test (the THEORY above) and, when vectors land, the scoring
  test against known vectors.
- Concurrency: two threads recording usage concurrently both land (the property
  `FILE_LOCK` gave `usage.rs` today).
- The denylist at the boundary: a 16-digit number is refused through every
  public write function; a source-scanning test (CRLF-normalised, non-empty
  candidate set) asserts nothing outside `store/` names `rusqlite`.
- ACL: the argument builder is pure and unit tested; the real `icacls` result is
  checked by hand (below).

## Slice plan

Each slice names its caller and the observable that would differ if it were
wired to nothing (AGENTS.md rule 8).

| # | slice | caller | observable |
|---|---|---|---|
| 1 | `rusqlite` dependency; `store/` with open, pragmas, ACL, migration runner, corruption handling, v1 = `meta` and `usage_day`; `spend.json` import; `usage.rs` records and reads through it; `PRIVACY.md` wipe fix; exe size and idle RSS recorded | `usage::record` after each request, and the tray tooltip | After one real request, `wingman.db` exists in the restricted directory with a `usage_day` row for today; `spend.json` is renamed `.migrated`; the tooltip still shows today's spend; `icacls` on the directory lists only the current user; release exe size before and after in the PR |
| 2 | v2 = `egress`; `egress.log` import; `egress::try_record` and the diagnostics human formatter read and write the store; the file cap and ACL-once code are deleted | the provider request path, `diagnostics.rs` | The diagnostics bundle after a request lists that request from the database; `egress.log` is `.migrated` and no longer grows |
| 3 | v3 = `profile_field`; `profile.bin` import; `Profile::load`/`save` go through the store | `fill_form` and the `app.rs` fill path; the Profile page (#50) when it lands | Fill a form after upgrading: the fields come from the database; `profile.bin` is `.migrated` |
| 4 | v4 = `memory_item`, the boundary denylist, export and import functions (with #86) | the Remember button, the review card (#87), the Memory page (#88) | A 16-digit number typed into Remember is refused with a card; export then import round-trips |
| 5 | v5 = `document`, `chunk`, `chunk_fts`, the `VectorIndex` trait and chosen implementation (with #83 and #85) | the ingest pipeline and retrieval | A phrase from a dropped resume comes back as the first hit; deleting the file leaves no chunk rows |
| 6 | delete-after-release: remove the `.migrated` files; context table with #61 | the same startup path | the files are gone |

Slices 1 to 3 do not depend on the knowledge track and can land in order as soon
as this design is accepted. Slices 5 and 6 are listed so their tables have a
reserved home; their own specs and issues own the detail.

Manual check for slice 1: run the installed app after the upgrade from a build
that had `spend.json`, press the key once, and look at the three observables
above. The state of a fresh install and of an upgrade are both worth one run.

## Open questions

Each has a recommendation; none blocks slice 1 except the first.

1. **How is the data encrypted at rest?** Options: (A) owner-only ACL and
   plaintext values: simplest, FTS5 and vectors work, but an administrator
   account can read the file. (B) per-value DPAPI for the small authored tables
   (`profile_field`, `memory_item`) and ACL-only plaintext for knowledge chunks
   and their FTS index: the sensitive identity data is protected from other
   accounts including administrators; the retrieval index is not. Memory search
   then happens in Rust over decrypted values, which is fine at memory's size.
   (C) SQLCipher (BSD-style license) with a random key wrapped by DPAPI: the
   whole file is encrypted and FTS5 still works, but the build needs OpenSSL or
   an equivalent, which is a much larger C build and exe (THEORY, unverified).
   **Recommendation: B for slices 1 to 4 and say so honestly in `PRIVACY.md`;
   keep the store API unchanged so C is a later swap if wanted.** #85's wording
   "read from another account yields no plaintext" is met for ordinary accounts
   by the directory ACL and should be reworded to say that.
2. **Vectors: `sqlite-vec` or a BLOB with brute-force cosine in Rust?**
   **Recommendation: BLOB plus brute force behind `VectorIndex`**, measure on
   real data in slice 5, and adopt `sqlite-vec` only if the measurement or the
   corpus size says so. It avoids a second C dependency and an `unsafe`
   extension registration for something that is not faster.
3. **Directory: `store\wingman.db` or the plan's flat `wingman.db`?**
   **Recommendation: the subdirectory**, for the inheritable ACL over the WAL
   files. Update the plan's module table to match.
4. **Writer: a `Mutex<Connection>` or a dedicated writer thread?**
   **Recommendation: the mutex**, because write errors must reach the caller and
   there is no backlog worth queuing. Revisit only if ingest contention shows up
   in measurement.
5. **What do egress rows store about knowledge snippets: labels or text?** The
   done-when of #106 only needs "the same snippets the chip showed". Storing the
   text makes the egress log a second copy of the user's documents.
   **Recommendation: source label, chunk id and a short hash, never the text.**
6. **Egress retention: 90 days and 10 000 rows, and is 60 days of usage still
   right?** **Recommendation: yes to both**, user-adjustable later on the Usage
   page; they replace a byte cap that does not mean what it did.
7. **Old files after import: rename and keep one release, or delete at once?**
   **Recommendation: rename and keep one release**, since forward-only
   migrations give no way back and a bad import would otherwise lose history.
8. **Snapshot cadence: weekly at startup?** **Recommendation: yes**, checked at
   startup so it needs no timer, two kept, authored tables only restored from it.
9. **Update the wording of #46** (drop `conversations` and `messages`, say
   `egress` and `memory_item`)? **Recommendation: yes**, once this spec is
   accepted.
