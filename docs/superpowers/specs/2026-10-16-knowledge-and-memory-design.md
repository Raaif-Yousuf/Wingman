# Knowledge and memory: the folder, the index, the memory model

Status: **proposed, 2026-10-16.** Issues #81 to #97 (milestone Phase 3b).
Builds on the [expansion plan](2026-09-16-expansion-plan-design.md) section 18
and the accepted [local store spec](2026-10-15-local-store-design.md); where
this spec refines a table the store spec only reserved, the change is listed
in "Changes to the store spec's reserved tables". There is no second store.

## Why this is a spec and not a PR

This track decides what Wingman reads from the user's disk, what it keeps
about them, and what of that may leave the machine. It adds parsers that read
untrusted files, a watcher thread, a new network call type (embeddings), and
the first data the user cannot regenerate (memory). A wrong shape here is paid
for in data users hold for years and in a privacy promise that is hard to take
back. That is architecture (AGENTS.md rule 12), so this stops at the design.

No code is part of this change.

## Goals and non-goals

Goals:

- A folder the user drops files into becomes something Wingman can answer from,
  with citations, offline if the user wants.
- Facts about the user (from documents, from corrections, from "remember this")
  live in one inspectable, editable, exportable place.
- Nothing from either leaves the machine except inside a request the user
  started, and every such request lists exactly what was attached.
- Nothing runs while idle (rule 5). The watcher is event-driven, indexing is
  batched and throttled, and a user who never turns knowledge on pays nothing.
- Each slice ships alone and is useful alone.

Non-goals: chat or follow-up questions (one question, one card); sync between
machines; a general desktop search; indexing the whole disk; a vector index
structure (approximate nearest neighbour) before measurement says it is needed;
the awareness context table (#61, a later migration under the same rules).

## Problems each slice solves for the user

| user problem today | what fixes it | slice |
|---|---|---|
| "What was my start date at X?" means opening files and searching by hand | Ask my documents: one question, one card, with the file and page cited | K1 |
| Resume is a PDF or .docx, not text | out-of-process parsers | K2 |
| Searching by wording misses "employer" vs "company" | embeddings and hybrid ranking | K3 |
| Fill this form only knows the 10 or so profile fields | a memory of facts, editable, with a page to see and fix it | K4 |
| Typing the resume's contents into the memory by hand | facts extracted from a dropped file, reviewed once | K5 |
| Every application form and cover letter starts from nothing | retrieval inside Fill, Reply and a cover-letter action | K6 |
| Wingman repeats the same mistake in a draft | corrections, "remember this", style, hygiene | K7 |
| Notes already live in Obsidian | a vault as a source, and a note written back | K8 |

## What exists today (checked against master)

- `src/store/` (slices 1 and 2 of the store spec): `rusqlite` 0.40 with the
  bundled build, FTS5 compiled in and tested, forward-only migrations from
  `store/schema/vNNN.sql`, an owner-only directory ACL, one write connection
  behind a mutex, read connections on demand, corruption recovery, a pending
  notice channel (`store::report`, `take_notice`) that rides the next card.
- `profile/` and `payment_denylist.rs`: a DPAPI-wrapped profile and the shared
  card (Luhn), IBAN, CVV-label and bank-label checks. No national-id check
  exists yet.
- `redact.rs`: `Redactions` swaps emails, card numbers, IBANs and key-like
  tokens for numbered markers on the way out and restores them in the answer.
- `provider/`: one `Provider::complete` path, every send goes through
  `provider/common.rs` (the Offline guard, the send gate, the egress record).
  There is no embedding call. `ollama_admin.rs` already has model listing and
  pull with progress.
- `egress.rs`: `snippets` is always empty; the column is waiting for this track.
- `ui/preview.rs` already has a "Knowledge snippets" row that is always 0.
- `inputs/html_text.rs` turns HTML into text without a crate; `ocr.rs` is
  Windows OCR, which needs no package identity (AGENTS.md, MEASURED 2026-09-17).
- `InputKind` (actions) lists `context` as "accepted but gathered by nothing".
  This track adds `knowledge` beside it.
- The release exe is 3,800,576 bytes against a 3,900,000 gate
  (`perf-budgets.json`, 2026-10-16). About 100 KB of headroom. This is the
  constraint that shapes the parser decision below.

- **`windows` features this track needs and master does not enable.**
  `Cargo.toml` enables `Win32_Foundation`, `Win32_Security*`,
  `Win32_Security_Cryptography` and `Win32_System_Threading`, but not
  `Win32_Storage_FileSystem` (`CreateFileW`, `ReadDirectoryChangesW`, the file
  attribute constants), `Win32_System_IO` (`OVERLAPPED`), `Win32_System_JobObjects`
  (the parser's limits) or `Win32_System_Power` (the AC power notification).
  "No new crate" is true; "no new dependency surface" is not, and features cost
  size. Each slice lists the ones it adds.
- **There is no application manifest.** `assets/app.rc` holds the icon and
  nothing else, and `build.rs` embeds only that. So Settings and any list view
  run classic (v5) common controls today. The Memory window's list view and the
  Settings dark-mode spec depend on the same missing manifest; see "Memory page".
- **`InputKind::Text` is declared and gathered by nothing.** The palette run
  message (`WM_APP_PALETTE_RUN`) carries only the action id, so a typed line
  never reaches an action. Ask my documents needs both changed, and K1 owns it.
- `provider/common.rs` has `post_json` style helpers and a source-scanning test
  that confines `ureq::post(` to it. Whether an embedding request fits those
  helpers or needs one more generic helper is checked in K3, and any helper is
  added inside `common.rs` so the guard stays true.
- The store spec orders its migrations profile (v3), memory (v4), knowledge
  (v5). This track puts knowledge first. Numbers are assigned at merge; this
  K1's PR adds a one-line order note to the store spec's slice table.

## Architecture in one picture

```
 knowledge folder(s)            Wingman process                         out of process
 ------------------      ----------------------------------      ----------------------
 file added/changed ---> watcher thread (ReadDirectoryChangesW)
                          wakes only on an event, debounces
                                   |
                                   v
                          ingest worker (exists only while the queue is non-empty)
                          hash -> skip if unchanged
                          parse  --- md txt html csv json: in process
                                 --- png jpg: Windows OCR, in process
                                 --- pdf docx: ----------------------------> wingman-parse.exe
                          chunk -> payment-shape masking -> store (document, chunk, FTS)
                          embed (batched, only if an embedder is allowed)
                          learn facts (only on an explicit press, see "What leaves the machine")
                                   |
                                   v
                          SQLite store: document, chunk, chunk_fts, chunk_embedding,
                                        memory_item, memory_source

 a key press ----> action with inputs ["screen","knowledge"] -> retrieve (FTS + vectors + memory)
                   -> redact -> request -> card (citations built by Wingman, not by the model)
```

## Crates and licenses (checked 2026-10-16)

Every row was read from crates.io for the latest non-yanked version on
2026-10-16, and its normal dependencies were listed and checked the same way.
None has relicensed to something outside AGENTS.md rule 2. The adoption slice
still runs `cargo deny check` and records the result in `THIRD_PARTY_NOTICES.md`.

| crate | latest | license | verdict |
|---|---|---|---|
| `pdf-extract` | 0.12.1 | MIT | candidate for PDF text. Pulls `lopdf`, `adobe-cmap-parser` (MIT), `cff-parser` (MIT OR Apache-2.0), `postscript` (Apache-2.0/MIT), `type1-encoding-parser` (MIT), `euclid` (MIT OR Apache-2.0), `encoding_rs` ((Apache-2.0 OR MIT) AND BSD-3-Clause), `unicode-normalization`. All permitted |
| `lopdf` | 0.45.0 | MIT | the PDF object parser under `pdf-extract`. Its dependencies (`aes`, `cbc`, `ecb`, `md-5`, `sha2`, `flate2`, `weezl`, `brotli-decompressor` BSD-3-Clause/MIT, `nom` MIT, `rangemap`, `stringprep`) are all permitted |
| `docx-rs` | 0.4.22 | MIT | reads and also writes .docx. Heavier than needed for reading |
| `zip` | 9.0.4 | MIT | read a .docx, .odt, .pptx container. Use with default features off and only deflate |
| `quick-xml` | 0.42.0 | MIT | pull text runs and heading styles out of `word/document.xml` |
| `html2text` | 0.17.3 | MIT | not needed: `inputs/html_text.rs` exists. It would pull `html5ever`, `tendril` (MIT OR Apache-2.0), more size |
| `sqlite-vec` | 0.1.10-alpha.4 | MIT/Apache-2.0 | still alpha on crates.io. Not adopted (see "Vector search") |
| `usearch` | 2.26.4 | Apache-2.0 | an ANN index with a C++ core. Not needed at this scale |
| `hnsw_rs` | 0.3.5 | MIT/Apache-2.0 | pure Rust ANN. Not needed at this scale, named as the fallback if a measurement demands it |
| `sha2` | 0.11.1 | MIT OR Apache-2.0 | fallback for file hashing if the CNG wrapper below is rejected |
| `blake3` | 1.8.7 | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception | usable through its Apache-2.0 option, not needed |
| `notify` | 9.0.0-rc.5 | **CC0-1.0** | **not adopted.** CC0 is not on the allowed list, and a watcher over `ReadDirectoryChangesW` is about 150 lines of the `windows` crate we already have |
| `oxidize-pdf` | 5.4.2 | MIT | MIT today. Not adopted: 7.6 MB crate, a generator more than an extractor. Its license history was not checked, so treat it as unverified if it ever comes up |
| `pdf_oxide` | 0.3.78 | MIT OR Apache-2.0 | 6.8 MB crate, pre-1.0. Not adopted |
| `pdfium-render` | 0.9.4 | MIT OR Apache-2.0 | needs a separate pdfium binary (BSD-3-Clause and others), which has to be shipped and re-licensed per release. Not adopted |

Models are data, not crates: `nomic-embed-text` is published under Apache-2.0
(verify on the Ollama library page when K3 is written; it is pulled by the user
through Ollama, never bundled).

Findings: nothing in the shortlist relicensed to a disallowed license. One
popular candidate, `notify`, is CC0 and is rejected for that reason, not for
quality. `sqlite-vec` is permissively licensed but has not left alpha.

### Decision: parsers run in a second binary, not in `wingman.exe`

Two reasons, either enough:

1. **Panics.** The release profile is `panic = "abort"`. PDF and docx parsers
   are written against hostile input and still panic on malformed files; in
   this process that would kill the tray over one bad file, the exact thing
   rule 7 forbids. A child process dies alone and the ingest worker turns its
   exit code into a per-file failure.
2. **Size.** The exe has about 100 KB of headroom. THEORY (unverified): the PDF
   stack is several hundred KB in `opt-level = "z"`; K2 measures it. Out of
   process, the gate on `wingman.exe` does not move.

Shape: a second `[[bin]]`, `wingman-parse`, in the same crate (still one crate,
`required-features = ["parsers"]` so a plain `cargo build` of the tray does not
compile the parser stack). Packaging copies it next to `wingman.exe` and signs
it with the same certificate. It is started per batch, never resident:
`wingman-parse <path> --max-bytes N`, writes one JSON document to stdout (pages
or sections, each with text) and exits. The tray starts it suspended inside a
Job object with a memory cap (512 MB), a wall-clock limit (60 s) and
`BELOW_NORMAL` priority, then kills it on breach. It contains no networking
code and links none. Text-like formats (md, txt, html, csv, json) and OCR of
images stay in process: they are small, have no panic-prone parser, and the
OCR engine is the OS's.

A .docx is read as zip plus XML (`zip` and `quick-xml`, both MIT): headings from
paragraph styles, text from runs, tables flattened row by row. Caps: 200 MB of
uncompressed output refused (zip bomb), 2 MB of extracted text per file. If
that proves too thin, `docx-rs` (MIT) is the substitute behind the same trait.

A scanned PDF has no text layer. `pdf-extract` returns nothing for it. A later
option, not in K2: render pages with `Windows.Data.Pdf` (built into Windows) and
send them through Windows OCR. Until then the file is marked `skipped` with the
reason "no text layer", visible on the Knowledge page.

**Packaging check for the second binary (K2).** It runs without package identity
(no WinRT, no network, no OCR), launched by path from the directory of
`wingman.exe`, so the sparse package (AGENTS.md rule 4) needs no manifest entry
for it. THEORY (unverified): that an unregistered executable in the external
location starts normally from the identity process; K2's packaging test installs
the package and runs a real PDF through it. `Build-Msix.ps1` signs both files
with the one certificate, the installer asserts both are present, and the
missing-file failure row below covers a half install. It also needs the
`Win32_System_JobObjects` feature in `wingman.exe`, which is part of K2's size
line.

## Data model

Versions are assigned at merge (store spec); the roles below are what matter.
Everything is created by a forward-only `store/schema/vNNN.sql` with a
migration test per step, exactly as the store spec requires.

### Knowledge migration (first role: K1)

```
knowledge_root (id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,         -- absolute, canonical
                kind TEXT NOT NULL,                -- default | extra | vault
                enabled INTEGER NOT NULL DEFAULT 1,
                note_target INTEGER NOT NULL DEFAULT 0,   -- K8: Save as note may write here
                added_unix INTEGER NOT NULL)

document (id INTEGER PRIMARY KEY,
          root_id INTEGER NOT NULL REFERENCES knowledge_root ON DELETE CASCADE,
          rel_path TEXT NOT NULL,                  -- relative to the root
          title TEXT,
          content_hash TEXT,                       -- SHA-256 hex of the file bytes
          size INTEGER, mtime_unix INTEGER,
          parser TEXT, parser_ver INTEGER,         -- a parser upgrade re-parses
          status TEXT NOT NULL,                    -- pending | indexed | failed | skipped
          error TEXT,                              -- plain words, never file content
          ingested_unix INTEGER,
          learned_hash TEXT,                       -- K5: content_hash facts were last learned from
          UNIQUE (root_id, rel_path))

chunk (id INTEGER PRIMARY KEY,
       doc_id INTEGER NOT NULL REFERENCES document ON DELETE CASCADE,
       ordinal INTEGER NOT NULL,
       text TEXT NOT NULL,
       span TEXT,                                  -- JSON {page, heading, start, end}
       text_hash TEXT NOT NULL,                    -- so an edit keeps unchanged chunks
       tokens INTEGER NOT NULL)                    -- estimate, chars / 4

chunk_fts  -- FTS5 external content over chunk.text, tokenize='unicode61 remove_diacritics 2',
           -- kept in step by after-insert, after-delete and after-update triggers
```

`chunk_fts` with external content needs the triggers; the store spec already
keeps triggers compiled in for this reason.

### Full-text tokenizer and languages

`unicode61` splits on Unicode word boundaries but **does not segment Chinese,
Japanese or Korean**: a contiguous run of CJK characters is one token, so a
query for a word inside a sentence finds nothing. The earlier draft said it
"works for every language"; it works for scripts that separate words with
spaces. K1 either records the limit in `docs/knowledge.md` or adds a second FTS
table with the `trigram` tokenizer for chunks detected as CJK (a trigram index
is roughly three times larger and needs a query of at least three characters,
so a two-character CJK word still misses). Vectors (K3) cover some of the gap
only if the embedding model is multilingual, which `nomic-embed-text` is not
claimed to be. See open question 13.

The stop-word list used to prune a query is English only. For another language
it prunes nothing, which costs a little ranking quality and never breaks
syntax, because every term is quoted.

### Embeddings migration (K3)

```
chunk_embedding (chunk_id INTEGER NOT NULL REFERENCES chunk ON DELETE CASCADE,
                 model TEXT NOT NULL,              -- "ollama:nomic-embed-text:768"
                 dim INTEGER NOT NULL,
                 vec BLOB NOT NULL,                -- little-endian f32, unit length
                 UNIQUE (chunk_id, model))
```

The key is the model identity string `provider:model:dim`. See "Embeddings".

### Memory migration (K4)

```
memory_item (id INTEGER PRIMARY KEY,
             kind TEXT NOT NULL CHECK (kind IN
                  ('fact','preference','correction','profile_field','style')),
             key TEXT,                      -- profile_field: the FieldKind name;
                                            -- correction: the action id; else NULL
             text BLOB NOT NULL,            -- DPAPI envelope, per value
             structured BLOB,               -- DPAPI envelope, JSON
             dedupe TEXT NOT NULL,          -- keyed hash of the normalized text, see below
             confidence REAL NOT NULL DEFAULT 1.0,
             created_unix INTEGER NOT NULL,
             updated_unix INTEGER NOT NULL,
             last_used_unix INTEGER,
             uses INTEGER NOT NULL DEFAULT 0,
             pinned INTEGER NOT NULL DEFAULT 0,
             local_only INTEGER NOT NULL DEFAULT 0,  -- never attached to a cloud request
             reviewed_unix INTEGER,         -- NULL = new, shown by "Learned N things: Review"
             UNIQUE (kind, key, dedupe))

memory_source (id INTEGER PRIMARY KEY,
               item_id INTEGER NOT NULL REFERENCES memory_item ON DELETE CASCADE,
               source_kind TEXT NOT NULL,   -- doc | action | user
               doc_id INTEGER REFERENCES document ON DELETE SET NULL,
               label TEXT NOT NULL,         -- file name or action id at the time
               span TEXT,                   -- JSON {page, quote_hash}
               added_unix INTEGER NOT NULL)
```

Why `memory_source` is its own table: an item learned from two documents has two
sources (#94: "become one item with two sources"), and a deleted document must
leave the item with its label ("resume.pdf, removed") rather than a dangling id.
This replaces the single `source_kind`, `source_ref`, `source_span` columns the
store spec reserved.

**Dedupe without reading plaintext.** Text is encrypted per value, so equal
texts cannot be found by SQL. `dedupe` is HMAC-SHA-256 of the lower-cased,
whitespace-collapsed text under a random 32-byte key kept in `meta`, itself a
DPAPI envelope. The unique index then merges "same text, different source" on
insert (the new source is added to the existing item). Windows CNG
(`BCryptHash`, already reachable through the `Win32_Security_Cryptography`
feature this crate enables) provides SHA-256 and HMAC with no new crate; the
wrapper is one small file tested against published vectors. Near-duplicates (not
equal after normalization) are #94's job and are found in Rust over decrypted
text.

The profile slice (store slice 3) keeps `profile_field`; when K4 lands, its
migration copies each row into `memory_item` with `kind = 'profile_field'` and
drops the old table in the same migration, as the store spec says. From then on
the profile reads and writes `memory_item`; there is no view and no second copy
that could diverge. See "Slice plan".

### Changes to the store spec's reserved tables

| reserved there | here | why |
|---|---|---|
| `document.path UNIQUE` | `root_id` plus `rel_path` | removing a folder from the list is one cascade; moving a vault does not look like a new corpus |
| `chunk.embed_model`, `embed_dim`, `embedding` columns | a separate `chunk_embedding` table keyed by model | two models can coexist while a re-embed runs; "drop all vectors for model X" is one `DELETE`; nothing about vectors lives in the table full-text search reads |
| `memory_item.source_kind/source_ref/source_span` | `memory_source` table | one item, many sources |
| `memory_item.last_used_unix`, `uses`, `pinned`, `confidence` | unchanged | |

The store spec's decisions stand: authored tables are per-value DPAPI, knowledge
chunks and the full-text index are plaintext under the owner-only directory
(a resume sits as plaintext in Documents already, so encrypting its chunks would
protect nothing the file itself does not expose), vectors are a BLOB scored in
Rust behind `VectorIndex`. One consequence worth saying: facts extracted from a
document are encrypted, the document's chunks are not.

## Ingest pipeline

### Watching

- One thread, started only when at least one root is enabled and Knowledge is
  on; absent otherwise. It opens each root with `CreateFileW`
  (`FILE_LIST_DIRECTORY`, `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED`)
  and calls `ReadDirectoryChangesW` with `bWatchSubtree`, a 64 KB buffer and the
  filter name, directory name, size and last-write. It then blocks in
  `WaitForMultipleObjects` on the per-root events plus one stop event, with an
  infinite timeout. No timer, no polling.
- When an event arrives it records the path in an in-memory pending set and
  switches to a bounded wait (750 ms) so a burst from one save collapses to one
  entry. The deadline exists only while the set is non-empty. This is what meets
  "ingest within 2 s" in #81: 750 ms debounce plus the file-in-use retry below.
- A zero-byte completion means the buffer overflowed: mark the root dirty and
  run one reconciliation pass (below).
- A pair `RENAMED_OLD_NAME` and `RENAMED_NEW_NAME` updates `document.rel_path` in
  place: same row, same chunks, no re-embedding.
- Ignored: dot-folders (`.obsidian`, `.git`, `.trash`), `~$*` Office lock files,
  `*.tmp`, files over 25 MB, a root's own `README.txt` created by Wingman.
- Cloud-synced files that are not on disk (OneDrive Files On-Demand shows them
  with `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` or `FILE_ATTRIBUTE_OFFLINE`) are
  marked `skipped: not downloaded`. Reading them would silently download them.
- A file the editor still holds open fails to open: retry three times with
  backoff inside the same pending window, then mark `pending` and let the next
  event for it retry.

**Reconciliation** is the only walk: once at start-up when Knowledge is on, and
after an overflow. It lists each root and compares `(rel_path, size, mtime)`
with `document`; only a mismatch is hashed. It catches changes made while
Wingman was not running. It is a start-up step, not a timer, runs at background
priority, and is bounded by the file cap (default 20,000 files per root, beyond
which the root shows "too many files" and indexes the first 20,000 by path).

### Per file

1. **Hash.** SHA-256 of the file bytes through CNG. Equal to `content_hash` and
   `parser_ver` current: stop. This is what makes an unchanged file cost
   nothing, including after a rename seen only by reconciliation (same hash,
   old path gone: update the path).
2. **Parse** to text with markers (`\f` page breaks and `# heading` lines kept as
   text, so chunking can read page and heading from them). md, txt, csv, json and
   html in process; pdf and docx through `wingman-parse`; png, jpg and similar
   through `ocr::recognize`. JSON and CSV are flattened as `key: value` lines and
   rows with their header, capped at 2 MB of text. A parse failure sets
   `status = failed` with a plain reason and keeps any older chunks (a
   half-saved file must not erase a good index).
3. **Chunk.** About 500 tokens, counted as characters divided by four (no tokenizer
   crate; the figure only sizes chunks and budgets): target 2,000 characters,
   200 overlap, split at heading, paragraph, then sentence boundaries, never
   inside a table row. `span` records page and nearest heading. The text embedded
   and indexed is the chunk text; the heading path is added to the text that is
   embedded.
4. **Mask payment and national-id shaped text.** Card-number and IBAN spans (the
   shared `payment_denylist` span functions) and national-id spans (the new
   `payment_denylist` function K4 describes, label-gated except for the SSN
   shape `ddd-dd-dddd`) in a chunk are replaced with `[card number]`, `[IBAN]` and
   `[national id]` before the chunk is stored. A contract with an IBAN is still
   searchable; the database never holds one. This is the "no payment data" rule
   applied to knowledge, not only to memory. The national-id function ships in K1
   (it is a pure string scan), not K4, so a chunk is never stored with an SSN
   that a cloud snippet could later carry; K4 reuses it for memory writes.
5. **Diff and store.** In one transaction: chunks whose `text_hash` already exists
   for this document keep their row (and their embeddings); new ones are
   inserted; gone ones are deleted; `document` is updated. FTS rows follow by
   trigger. Search works from this point, before any embedding exists.
6. **Embed** (K3) is queued behind the cheap steps and runs only if allowed (next
   section). **Learn** (K5) is not part of the pipeline: it runs only on an
   explicit press (open question 7).

Throttling: one worker thread, one file at a time, created when the queue gets
its first entry and ended when it drains (nothing resident). It calls
`SetThreadPriority(THREAD_MODE_BACKGROUND_BEGIN)` (lowers I/O and memory
priority too). Writes are one transaction per file and release the store mutex
between files, so a user's request is never stuck behind an import (store spec,
threading). Embedding goes in batches of 16 chunks, and the local model work per
batch is capped (2,000 chunks, see "Size and performance budgets"), so a folder
of 200 notes cannot hold the iGPU. On battery, step 6 is deferred until AC power
or an explicit "Index now" (cheap steps 1 to 5 still run). **How the app learns
AC power returned without polling:** it registers once for the power-source
notification (`RegisterPowerSettingNotification` with the AC/DC power source
GUID, delivered as `WM_POWERBROADCAST` to the main window, which needs the
`Win32_System_Power` feature), and the handler wakes the ingest worker if the
queue is non-empty. No timer. If the registration fails, the deferred steps run
only on "Index now", and the status line says so; the safe outcome is never
"run on battery". While Wingman is Paused, nothing in this pipeline makes
a request of any kind, including loopback ones; the queue stays marked `pending`
and resumes when Pause ends.

### Deletes and renames

- Deleted file: the row and its chunks, embeddings and FTS entries go in the
  `ON DELETE CASCADE` (with `secure_delete` on, then the WAL checkpoint the store
  spec requires after user-initiated deletes; a watcher-seen delete is the user's
  action, so it checkpoints too).
- Memory learned from it is **never deleted silently**. The ingest step first
  collects the items whose only source was this document and posts a notice:
  "resume.pdf was removed. Forget the 14 things Wingman learned from it?" with
  Forget and Keep. Keep leaves the items with their label (`resume.pdf, removed`).
  This is the only place a document event touches authored data, and it asks.
- Removing a root from the list asks the same question per root.

## Embeddings

A sibling trait next to `Provider`, in `provider/embed.rs`:

```rust
pub trait Embedder: Send + Sync {
    fn id(&self) -> &'static str;                  // "ollama" | "openai" | "gemini"
    fn ready(&self) -> bool;
    fn identity(&self) -> String;                  // "ollama:nomic-embed-text:768"
    fn embed(&self, texts: &[String], role: Role) -> anyhow::Result<Vec<Vec<f32>>>;
}
pub enum Role { Document, Query }                  // nomic wants "search_document: " / "search_query: "
```

It sends through `provider/common.rs`, never directly, so the Offline guard,
the send gate, the egress record and the existing source-scanning test that
forbids direct HTTP in `src/provider/` all apply without new code. That the
embedder cannot bypass the guard is a property of where it lives, not of care.

- **Local (default).** Ollama `POST http://127.0.0.1:11434/api/embed` with
  `model`, `input` (an array, batch of 16), `truncate: false` and a top-level
  `keep_alive` (short during a batch, `0` on the last so the model unloads and
  does not pin laptop memory). `127.0.0.1`, never `localhost` (rule 6). The embed
  endpoint has no `think` field; the chat call used for fact extraction (K5)
  sets `think` explicitly. Default model `nomic-embed-text`, 768 dimensions. If
  the model is not pulled, one notice offers the pull through the existing
  `ollama_admin` pull-with-progress; until then Knowledge works by words only.
  The stock Ollama tray stealing the port is handled by the existing health
  checks; an embed failure from it is a card like any provider failure.
- **Cloud (opt-in, never the default).** OpenAI `text-embedding-3-small`
  (`POST /v1/embeddings`) and Gemini (`gemini-embedding-001`, batch embed
  endpoint). Dimensions: request 768 where the API allows a reduced size, so
  the identity string and storage are the same shape; the model name and
  dimension are recorded either way. Vendor limits (inputs per call, tokens per
  call, task-type fields) are re-read from the vendor docs when K3 is written;
  they are not restated here because they change.
- **Offline mode:** the embedder list is filtered the same way `mode::select_providers`
  filters chat providers. Only loopback survives. With no local model the pipeline
  runs words-only and says so once; it does not fail.
- **Model identity and switching.** Vectors are only comparable inside one
  identity. The active identity is in `meta`. Retrieval joins `chunk_embedding`
  on the active identity only, so changing the model (or the dimension) can never
  mix vectors: chunks without a vector for the new identity are simply absent from
  the vector half of ranking and still present in the full-text half. A background
  re-embed (local only, or cloud only if the user clicked it) fills them in, and
  the Knowledge page says "2,310 of 2,400 chunks indexed for nomic-embed-text".
  When the re-embed finishes, an explicit "Remove vectors from the old model"
  button deletes the old rows; nothing deletes them automatically.
- **Cost guard (cloud).** Before the first cloud embedding run for a folder the
  user sees the chunk count, an estimate from the price table `usage.rs` already
  owns and the first few file names, and confirms for that folder; usage and
  egress record it like any request.

## Vector search and hybrid retrieval

Decision (extends store spec decision 2): **brute-force cosine in Rust behind
`VectorIndex`, no `sqlite-vec`.** Reasons: it is alpha on crates.io, it is a
second C build and an `unsafe` extension registration, and the store spec
already turned loadable extensions off to save size. The scale arithmetic is
unchanged: 10,000 chunks at 768 dimensions is 7.7 million multiply-adds and
30 MB of vectors. THEORY (unverified): tens of milliseconds. K3 measures it on a
real corpus and records the number here before the choice is final. The scan
streams rows through a statement and keeps only a top-50 heap, so it never holds
the corpus in memory (idle RSS rule). If K3 measures over 150 ms at the corpus
sizes below, the order of remedies is: store int8-quantized vectors (a quarter
of the bytes), then an in-memory `hnsw_rs` built on first use, then `sqlite-vec`.

Retrieval for one request:

1. Turn the user's text (typed question, selection, or posting text) into a query.
   Full-text: quote each term, drop a short stop-word list, join with `OR`, keep
   at most 12 terms, so punctuation can never be FTS syntax.
2. Full-text: `bm25()` over `chunk_fts`, top 50.
3. Vectors (if the active identity has vectors): embed the query once (one
   `Embedder` call, `Role::Query`), cosine top 50.
4. Reciprocal-rank fusion: score is the sum of `1 / (60 + rank)` over the lists a
   chunk appears in.
5. Merge adjacent chunks of one document into one snippet, then take the highest
   snippets until the **token budget** (default 2,000, setting) is full, at most
   8 snippets.
6. Memory items are retrieved separately (they are few and encrypted), and only
   when the chip is on: load non-`correction` items that are **reviewed**
   (`reviewed_unix` not NULL), decrypt in memory, score by term overlap in Rust,
   take up to 300 tokens; reviewed corrections for the running action id are
   included. For a cloud provider, items with `local_only` and `contact`
   corrections are skipped (see "Memory and outbound PII"). Plaintext is held
   only for the request and zeroized (the same discipline as `secrets.rs`).

Snippets are numbered `[1]..[n]`. The prompt marks them as untrusted data (the
same convention as page text and linked pages): a document may contain
instructions and they must not be followed. A knowledge-using action never has
an executor that acts without the existing confirm step.

### Ask my documents (#92): one question, one card

An `actions.toml` action (a generic action, so it works now that custom actions
run) with `inputs = ["text", "knowledge"]` and `proposal` `knowledge_answer`.
The `text` input is the palette line, which this track makes the first thing to
gather it. The schema is ordered evidence first:

```
{ evidence: [{ n: int, quote: string }],   // which snippet, and the words in it
  answer: string,
  found: bool }
```

Property order is load-bearing (rule 3): with `evidence` before `answer` the
model commits to the supporting words before it writes the answer. Wingman then
checks each `quote` appears (case and whitespace normalized) in snippet `n`,
drops any that do not, and builds the citation line itself from `document` and
`span` ("resume.pdf, page 2"). The model never supplies a file name. If nothing
verifies, the card says the documents did not contain a supported answer, and
shows the closest snippets' sources. This is the whole feature: no history, no
follow-up (no chat).

## What leaves the machine, and when

The sentence in `PRIVACY.md` that Wingman "makes a request only in direct
response to you pressing the hotkey" stays true **only if** ingest makes no
background request to a remote host. That is the design constraint, and the
defaults follow from it.

| event | trigger | what is sent, to where | default |
|---|---|---|---|
| index a file (hash, parse, chunk, full-text) | file event | nothing | on |
| embed chunks, local | file event, on AC power or "Index now" | chunk text to `127.0.0.1:11434` | on if a local model is present, capped per batch (see "Throttling") |
| embed chunks, cloud | only after the user chose a cloud embedder for **that folder** and pressed Index, after the preview | redacted chunk text to that provider | **off** |
| learn facts, local | an explicit "Learn from this file" or "Learn from this folder" press | document text to `127.0.0.1:11434` | **button only** (open question 7) |
| learn facts, cloud | an explicit press, per file, after the preview | redacted document text to the active provider | **off** |
| embed the query | a key press whose action uses knowledge | the question text to the embedder | follows the embedder choice |
| attach snippets and memory | a key press, the request lists store content, chip on | snippets and items inside the request | per action (chip) |

Rules that follow:

- Mode Auto never picks a cloud embedder or cloud extraction for background work.
  Cloud use for indexing is a separate, explicit, **per-provider and per-folder**
  consent shown in Settings, with the sentence "The text of files in this folder
  will be sent to <provider> to index it." Before the first run the user sees a
  preview: the number of chunks, an estimate from the price table `usage.rs`
  already owns, and the first few file names. Dropping a file is the user's
  consent to index locally; it is not consent to send the file to a vendor and
  not consent to run a model over it.
- **Redaction before anything is embedded or sent to a cloud provider.**
  Embedding input and extraction input go through `Redactions` (emails, card
  numbers, IBANs, key-like tokens), **extended with the national-id check** (see
  "The payment-data denylist at the boundary"), and the extraction response is
  restored with `restore_in_json`, so facts hold real values while the vendor saw
  markers. The masked-at-ingest payment and national-id spans never reach it.
  Cost accepted: a query for an email address ranks worse in the cloud embedder
  than the local one. Outbound snippets are redacted again on the way out, since
  a chunk stored before a rule existed could still hold a shape the rule now
  catches.
- **Egress log.** Every embedding batch, extraction call and snippet attachment is
  one `egress` row like any request: provider, model, bytes, outcome, cost. New
  `AttachmentKind` values: `knowledge` (count and bytes), `memory` (count) and
  `embed` (chunk count and bytes). `snippets` carries one short label per item
  sent, following store decision 5: `resume.pdf#chunk 12:ab12cd34` (file name,
  chunk id, first 8 hex characters of the chunk hash), and `memory#41:ef56ab78`
  for a fact. Never the text. **File names can themselves be sensitive**
  ("divorce-settlement.pdf"). The local Egress page shows them, because the user
  needs to see what was attached, and `PRIVACY.md` says so. The diagnostics bundle
  (which a user may share) carries `doc#<id>#chunk 12:ab12cd34` instead, with no
  file name.
- **Chip off means nothing.** A test fails the build if a request-building path
  reads the store's knowledge **or memory** when the chip is off or the action
  lists neither input (a source-scanning test with a non-empty candidate set,
  plus a behavioural test with a fake provider that records the request body).
  There is no memory path that bypasses the chip: Reply and Rewrite attach style
  and corrections only through the same chip (see "Use in actions").
- **Pause and Offline.** Pause stops the pipeline (above). Offline filters
  embedders to loopback and disables cloud extraction outright.
- A document cannot make Wingman send more than the user chose: snippets only
  ever go in the one request the user started, and the retrieval output is data.
- **Memory is data, never instructions.** Everything retrieved from the store is
  placed in a marked, untrusted block in the prompt. See "Memory and prompt
  injection".

## Memory model

### Items and kinds

| kind | meaning | `key` | written by |
|---|---|---|---|
| `fact` | something true about the user ("employer: Acme, 2019 to 2022") | NULL | extraction, Remember this |
| `preference` | how the user wants things ("short replies") | NULL | Remember this, model chip |
| `correction` | a diff between a proposal and what the user confirmed | the action id | the confirm step |
| `profile_field` | the existing profile fields | the `FieldKind` name | Profile page, fill |
| `style` | tone, sentence length, sign-off | `reply` or `rewrite` | K7 |

`confidence` is 1.0 for anything the user typed or clicked, and the model's own
figure for extracted facts (shown, never used to hide an item). `pinned` exempts
an item from the hygiene suggestions and nothing else. `uses` and
`last_used_unix` update when an item is attached to a request (one write per
request, inside the usage transaction path), which feeds #94.

### The payment-data denylist at the boundary

`store/memory.rs` is the only writer. Every write function (insert, update,
import, merge) runs `check_memory_text(kind, text)` before the statement and
returns a typed `Refused(reason)` the caller turns into a card, as the store spec
requires, so no future write path can skip it. The check reuses what exists and
adds what the plan names:

- card numbers (Luhn) and IBANs, the CVV label, and bank-account labels, from
  `payment_denylist` unchanged;
- **national ids**, a new function in `payment_denylist.rs` (added in K1 for
  chunk masking, reused here): a US social security
  shape (`ddd-dd-dddd`, or nine digits next to the words "ssn" or "social
  security"), a UK National Insurance shape, and any six or more digit run
  following "national id", "passport number" or "tax id". Label-gated where a bare
  digit run would also match phone numbers and zip codes, which resumes are full
  of and which are allowed;
- secrets: anything `redact::sensitive_spans` flags as a key-like token, and a
  "password:" or "passcode:" label.

The refusal text names the kind of match and never echoes the value (the
convention of `profile::denylist`). Extraction handles refusal per fact: one
refused fact does not drop the other thirteen; the review card says "2 were not
saved because they looked like card or id numbers." Import runs the same check on
every item, so a hand-edited export cannot smuggle one in.

Fail-open check (the guard cannot run): if the denylist function cannot complete
(it only does string scans, so the realistic failure is a panic caught by the
worker), the write is refused and the item is reported, never stored unchecked.

### Where memory comes from, and who authorizes it

| source | authorization | detail |
|---|---|---|
| a document in a Knowledge root | an explicit "Learn from this file" press (open question 7) | extraction can only produce `fact` items, which land with `reviewed_unix NULL`; the review card lets the user edit or delete |
| **Remember this** on a result card, or a palette line "remember that ..." | explicit | text is what the user sees and edits before saving; may be `fact` or `preference` |
| `remember[]` in a proposal response | click | rendered as chips; nothing stored until clicked; each passes the denylist |
| a proposal edited before Do it | automatic, per action | stored as a `correction`: field, field kind, before, after. The next run of that action includes it (behind the chip) and the card says so |
| writing samples | explicit "Learn my style" button | a `style` item (K7) |

**Only user-authored sources may create `preference` and `style` items.** Those
kinds steer how every later request is written, so a document must never be
able to plant one. Extraction output with any kind other than `fact` is dropped
and counted. A correction is stored only for fields the schema marks editable
and never for a value the denylist refuses; it is capped at the 10 newest per
action.

### Memory and prompt injection

Memory is a persistent channel into every later request, so a poisoned source
must not be able to use it silently.

- **Unreviewed items are not attached.** An item with `reviewed_unix NULL`
  (everything extraction just produced) is excluded from retrieval until the
  user has opened the review or marked it reviewed. The Memory window and the
  review card list them with their source, so a planted fact is visible before
  it can act.
- **Extracted facts are marked untrusted in the prompt**, in the same block as
  snippets ("Facts from your documents (untrusted)"), and the model is told they
  are data. Facts the user typed or confirmed go in a separate "Things you told
  Wingman" block, still data, never instructions.
- **Deleting the source file does not delete a fact**, by design (the Keep
  choice), so a fact planted by a document survives it. That is why review
  comes first, and why the item keeps its label ("resume.pdf, removed") and a
  "Forget everything from this source" action.
- A fact that contains something shaped like an instruction ("ignore previous",
  "always reply with", a URL to send data to) is stored but flagged
  `reviewed_unix NULL` until a person looks, even if the review was otherwise
  done in bulk. The check is a short deny-list of phrases, tested; it is a
  tripwire, not the defence, which is the structure above.
- A test plants an injected fact through a fixture document and asserts the
  request body carries it only after review, and only inside the marked block.

### Memory and outbound PII

Corrections store arbitrary before and after values: names, addresses, phone
numbers. The payment denylist does not cover them and `Redactions` covers only
emails, cards, IBANs and key-like tokens, so they would reach a cloud model.
Rules:

- Memory is attached only through the chip, for every action (see "Use in
  actions"). No action reads it implicitly.
- A `correction` carries a `field_kind` (`tone`, `salutation`, `signoff`,
  `wording`, or `contact` for names, addresses, phone numbers, emails and
  anything else personal). `contact` corrections are attached to **local**
  providers only; a cloud request leaves them out and says "2 personal
  corrections kept on this machine" on the preview.
- Facts and preferences carry the same rule: an item the `Redactions` or
  national-id check would change is attached to a cloud request only in its
  redacted form, and a user can mark an item `local only`.
- Open question 15 asks whether a cloud request may ever carry a `contact`
  item.

### Fact extraction (#87)

One model call per document (a long document is split by section, at most 6
calls, and the user sees the count before a cloud run). Schema, in this order:

```
{ facts: [{ text, kind, subject, source_quote, confidence }] }
```

`source_quote` comes first in effect: the model must quote the supporting words.
Wingman locates the quote in the document text, derives the page and offset
itself, and **drops any fact whose quote it cannot find** (the guard against
invented facts; counted and shown as "N skipped, not found in the file"). Facts
that pass the denylist and dedupe are stored with their source. A small local
model will extract less well than a cloud one; that is shown as lower
confidence and an honest count, not hidden. Extraction runs again only when
`content_hash` differs from `learned_hash`, and re-extraction merges by `dedupe`
(existing items keep edits and pins; a user-deleted fact comes back only if the
text changes; deleted items are remembered as a `dedupe` tombstone in `meta` so
re-learning does not resurrect them).

### Review and surfaces

The "Learned 14 things from resume.pdf: Review" line is delivered like the
store's other notices (the pending-notice channel on the next card), and as an
immediate card when the extraction was started by an explicit press. Review opens
the Memory page filtered to unreviewed items from that source. Viewing marks
them reviewed. Dropping a file never pops a window by itself.

### Memory page, without waiting on WebView2

The expansion plan puts Memory in the WebView2 settings window (#44), which is
undecided. This design does not depend on it:

- The page's logic is a pure module, `ui/memory_model.rs` (rows, search, filter by
  kind and source, sort, selected item, edit and delete operations returning the
  store calls to make), in the style of `preview.rs` and `palette_model.rs`:
  no Win32 imports, fully unit tested.
- The first view is a Win32 window of its own, created when opened and destroyed
  when closed (idle cost zero): a search edit, a virtual-mode list view
  (`LVS_OWNERDATA`, so ten thousand rows cost nothing), a detail pane with edit,
  Delete, Pin, "Forget everything from this source", Export, Import. The native
  list view comes with keyboard navigation, accessibility and high contrast for
  free, which the custom-painted card does not. Theming it (dark mode, owner
  draw) needs common controls v6, and **master has no application manifest**
  (`assets/app.rc` is the icon only), so a list view there is a classic v5 one.
  K4 does not assume v6: it follows the manifest decision in the Settings
  dark-mode spec (its Slice 0 and 0b), and if that slice is not merged, the
  window ships as a v5 list view that follows the system colours (correct in
  high contrast, light in dark mode) rather than a half-themed one.
- If #44 lands, only the view layer is replaced; the model and the store calls
  stay. If #44 is rejected, nothing is lost. Dependency on #44: none.
- Export is JSON (the lossless format: kind, key, text, structured, sources with
  labels, pinned, confidence, timestamps) and Markdown (readable, one section per
  kind; not re-imported). Import reads JSON, runs the denylist per item, merges
  by `dedupe` and reports counts on a card. Delete and "forget by source" end in
  the WAL checkpoint (store spec).

### Hygiene (#94)

No timer. At start-up, if the last suggestion was more than 30 days ago and
there are unpinned items unused for 180 days, one notice offers "Forget these N?"
with a review list; nothing is deleted without a click. Near-duplicate merge runs
when the Memory page opens or on import, in Rust over decrypted text (normalized
token-set similarity at or above 0.9, then a confirm), keeping both sources.
Exact duplicates already merge on insert via `dedupe`.

## Use in actions

- **Input kinds.** `knowledge` and `memory` join `InputKind`. `generic_inputs::plan`
  gathers them: retrieval, redaction, numbering, labelled "Knowledge from your
  documents (untrusted)" and "Memory (untrusted)". **An action that lists neither
  never reads the store**, and the chip is the single gate for both. The built-in
  Reply and Rewrite actions list `memory`, so their style and corrections go
  through it too.
- **Typed text.** Ask my documents uses `inputs = ["text", "knowledge"]`, and
  nothing gathers `InputKind::Text` today. K1 adds it: the palette passes its line
  with the action id (the run message carries a small owned struct instead of just
  the id) and `generic_inputs::plan` gathers it. Tests cover the line reaching
  the action and an empty line being refused with a card.
- **Knowledge and memory chip (#89).** The palette shows `Knowledge: 3 snippets
  from resume.pdf, notes.md` and `Memory: 2 items` for the line it is about to
  run, as one toggle like any chip. Default is on for an action whose `inputs`
  lists `knowledge` or `memory` (choosing that action is the consent, and the
  preview lists what goes), and the chip is absent when the store holds nothing.
  The existing "Knowledge snippets" count in the "show me what you're sending"
  preview stops being 0 and lists the labels. Off means nothing is read.
  Unreviewed memory is never counted (see "Memory and prompt injection"), and
  `contact` items are left out of a cloud request and the preview says so.
- **Fill this form (#90).** Its candidate values gain a second source: memory items
  retrieved for the form's field labels (the compact top 60, not everything). The
  preview model gets an optional per-field `source` ("Profile", "resume.pdf, page
  2"); the preview's "Source" column shows it. Values from the model still pass
  the payment denylist before the fill executor (unchanged).
- **Reply and Rewrite.** Include the `style` item and the action's `correction`
  items, behind the chip; the card line says why a choice was made ("using your
  usual sign-off").
- **Cover letter (#91).** `actions.toml`, `inputs = ["screen", "knowledge"]`,
  output text with insert at cursor through the existing `replace_text` executor,
  whose confirm preview shows the text first. A prompt requirement: cite only
  items present in the snippets; verified like Ask my documents quotes.
- **Obsidian (#96).** A vault is a root of `kind = vault` (extra folder, K1
  machinery). Markdown front matter and `[[wikilinks]]` are stripped to text with
  tags kept as searchable words. "Save as note" is an executor, so it follows Look,
  Propose, Confirm, Do: the preview shows the target path; the write creates a
  new file named `YYYY-MM-DD HHmm Title.md` inside a root with `note_target = 1`,
  opened with `CREATE_NEW` so it can never overwrite, after canonicalizing and
  refusing any path that escapes the root (`..`, a junction, a symlink). **There is
  no own-write ledger.** The executor does not ingest; the watcher is the only
  ingest path, so there is one writer to the index and no race between a ledger
  entry and the debounce. When the watcher sees the new file it indexes it through
  the normal path, and the content-hash check makes any second event for it a
  no-op. A test creates a note and asserts exactly one `document` row.

## UI surfaces summary

| surface | where | slice |
|---|---|---|
| Tray: Knowledge submenu: Open folder, Index now, a status line ("128 files, 3 waiting") | tray menu (Win32) | K1 |
| Settings: Knowledge section: folders (add, remove), indexing source (Local only, or a named cloud provider with the consent sentence), learn facts on/off, "Forget all knowledge" | existing Win32 settings | K1, grows per slice |
| Knowledge status list (file, status, reason): shows `failed` and `skipped` with plain reasons | same window or the Memory window's second tab | K1 |
| Card: Ask my documents answer with citations | card | K1 |
| Memory window | own Win32 window | K4 |
| "Learn from this file" and "Learn from this folder" buttons, with the call count shown first | Knowledge status list, Memory window | K5 |
| Review notice and card | notice channel, card | K5 |
| Knowledge chip | palette | K6 |
| Remember this button, `remember[]` chips | result card | K7 |

Creating the folder is lazy: the default folder and its README are created the
first time the user chooses "Open folder" in the tray (or turns Knowledge on in
Settings), not at install. A user who never does so has no folder in Documents
and no watcher thread. (#81 says "created on first run"; see open question 1.)

Accessibility: the tray status line is plain text in the tooltip and menu, the
status list and the Memory window use native controls (keyboard navigation,
screen reader names, high contrast), every card this track shows follows the
card accessibility rules (a system alert event when it appears), and no state is
conveyed by colour alone (a failed file says "failed" in words). The Memory
window gets a Narrator and high contrast pass as a named manual check in K4.

Manual-checked Win32 pieces name their observable in each slice, as the store
spec does.

## Size and performance budgets

All figures are targets to be measured by their slice, not measurements; where a
number came from arithmetic it is marked.

**The exe gate is shared.** `perf-budgets.json` records `exe_size_bytes` measured
3,800,576 against a gate of 3,900,000, about 99 KB of headroom, and the options
card, Settings dark mode and overlay specs draw on the same 99 KB. This track
cannot fit in it. Per-slice estimates for `wingman.exe` (code and any new
`windows` features, no new crate), each THEORY (unverified) until a CI measurement
replaces it:

| slice | `wingman.exe` growth | what it is |
|---|---|---|
| K1 | about 70 to 90 KB | store tables and queries, watcher, chunker, five in-process parsers, retrieval, the `Text` input and palette change, tray and Settings entries, the status list, the action; plus the `Win32_Storage_FileSystem` and `Win32_System_IO` features |
| K2 | about 5 KB | parser dispatch, Job object limits (`Win32_System_JobObjects`); the parsers themselves are in `wingman-parse.exe` |
| K3 | about 15 KB | `Embedder`, `VectorIndex`, fusion, the AC power notification (`Win32_System_Power`) |
| K4 | about 40 KB | memory store, CNG hashing, the denylist additions, the Memory window and its model |
| K5 | about 10 KB | extraction, quote verification, the review card |
| K6 | about 8 KB | chip, labels, the Fill source column |
| K7 | about 12 KB | Remember this, corrections, style, hygiene |
| K8 | about 8 KB | vault handling, Save as note |

That is roughly 170 to 190 KB in all. **K1 alone may use most of the headroom, and
the track needs a gate raise.** The earlier draft's "K1 adds at most 60 KB" was
optimistic for the list above. The plan: each PR records the CI-measured size and
raises `exe_size_bytes.max` in the same PR with the measured number and the
reason, as the QR decoder, the live model lists and the store did; the order the
four specs merge in decides who raises it first.

| budget | target | slice that measures it |
|---|---|---|
| `wingman-parse.exe` | measured and recorded in K2; not gated by the tray's budget, but listed in the release notes | K2 |
| idle RSS with watcher running and the store open | within 0.5 MB of the same build with Knowledge off (the CI harness already reports idle RSS) | K1 |
| idle CPU and power | 0% CPU over a 10-minute idle capture with the watcher on; no change in idle watts at the project's existing resolution | K1, manual |
| time to searchable after a save | under 2 s for a markdown file (#81) | K1 |
| first index of 100 markdown files (about 1 MB) | under 5 s, full-text only | K1 |
| query, full-text only, 10,000 chunks | p95 under 50 ms | K1 |
| query, hybrid, 10,000 chunks | under 150 ms before the model call | K3 |
| disk | about 1.3 times the extracted text for chunks plus full-text; plus 3 KB per chunk for f32 vectors (arithmetic: 768 times 4 bytes), about 30 MB at 10,000 chunks | K1, K3 |
| background priority | ingest thread in background mode; `wingman-parse` below normal | K1, K2 |
| caps | 25 MB per file, 2 MB extracted text per file, 20,000 files per root, 8 roots | K1 |
| local model work per batch | at most 2,000 chunks embedded, and at most 5 files extracted, per AC session or per "Index now"; the rest stays queued | K3, K5 |

## Failures: every one ends in a card

| failure | behavior |
|---|---|
| a file cannot be parsed, is encrypted, has no text layer | `status = failed or skipped` with a plain reason; the older index of that file is kept; one notice per batch ("3 files could not be read"), the list on the Knowledge page |
| `wingman-parse` crashes, times out or exceeds memory | that file fails with "this file could not be read safely"; the tray is unaffected |
| `wingman-parse.exe` is missing | pdf and docx are `skipped` with "the file reader is missing, reinstall"; one card; everything else works |
| embedder unreachable, model missing | words-only search keeps working; one notice, with a Pull button for the local model; the chunks stay un-vectored and are retried on the next change or "Index now" |
| watcher cannot open a root (deleted, permission, network drive gone) | the root shows "unavailable"; one notice; not retried on a timer; retried at start-up and on "Index now" |
| store write fails during ingest | the file stays `pending`, the existing store failure card path applies |
| Ask my documents finds nothing supported | a card saying so, with the closest sources |
| memory write refused by the denylist | a card naming the kind of match, not the value |
| a request fails after attaching snippets | the normal provider failure card; the egress row records the attempt |
| the AC power notification cannot be registered | deferred embedding runs only on "Index now"; one notice says so; nothing runs on battery |
| extraction or memory yields an item that looks like an instruction | stored, held as unreviewed and flagged on the review card; never attached before a person reviews it |
| a cloud request would carry `contact` or `local_only` items | they are left out; the preview says how many; never a silent drop |
| the national-id check cannot run on a chunk | the chunk is not stored (fail closed) and the file is `failed` with a plain reason |

"Notice" means the store's pending-notice channel: it rides the next result card
and shows in the tray tooltip. Background work never opens a dialog box (rule 7).
An action the user started (Index now, Learn from this file, Ask my documents)
gets its card immediately.

## Test strategy

Pure logic is unit tested; Win32 and the model are checked by hand with a named
observable (rule 8). Tests never use production names (rule 9): temp roots and
`Store::open_at`.

- **Chunker:** table tests for boundaries, overlap, tables, a single giant line,
  empty file, non-BMP text, and that spans point at the right page and heading.
- **Ingest state machine:** a fake file system trait; unchanged hash is a no-op;
  one edited paragraph keeps every other chunk and its embedding (the done-when of
  #83); rename keeps ids; delete leaves no `chunk`, `chunk_fts` or
  `chunk_embedding` rows; a failed parse keeps the old chunks; a crash between
  steps leaves `pending`.
- **Watcher:** a real temp directory test (not `#[ignore]`d) that creates, edits,
  renames and deletes files and asserts the pending set; an overflow test marks
  the root dirty; a test that the thread's wait has no timeout when the set is
  empty (a source-scanning test, CRLF-normalized, non-empty candidate set).
- **Parsers:** checked-in tiny fixtures (a two-page PDF with headings, a .docx with
  styles and a table, a corrupt PDF, a zip bomb, a password-protected PDF); the
  helper's exit codes map to typed errors; the Job-object limits are checked by a
  fixture that allocates past the cap.
- **Retrieval:** known vectors and a known corpus; BM25 ranks the phrase first; RRF
  against hand-computed ranks; a model-switch test (two identities in the table,
  only the active one is scored); a prompt-injection fixture (a chunk saying
  "ignore previous instructions") is wrapped as data and never reaches an executor.
- **Citations:** a model answer whose quote is not in the snippet loses the
  citation; the file name on the card always comes from the database.
- **Denylist:** every public write function refuses a 16-digit card, an IBAN, a
  CVV phrase, a national-id shape and an API-key-like token, and accepts a phone
  number and a postcode; import refuses them too; a source-scanning test asserts
  nothing outside `store/` names the knowledge or memory tables.
- **Egress:** a fake provider records the body; chip off sends no snippet; chip on
  logs labels, ids and hashes and never text; embeddings in Offline never leave
  loopback (the existing Offline guard test pattern).
- **Redaction:** a document with an email and a card number embedded through a
  fake cloud embedder shows markers only; extraction restores the real email in
  the stored fact.
- **Migrations:** one test per migration step plus the snapshot and contiguity
  tests from the store spec; the memory migration test round-trips a profile row.
- **Export and import:** round trip equality for every item including sources
  and pins.
- **Injection and PII:** a fixture document that tries to plant a `preference`
  yields no `preference` item (extraction may only write `fact`); an unreviewed
  fact is absent from the request body and present after review, inside the
  marked block; a `contact` correction never appears in a cloud request body but
  does in a local one; a chunk with an SSN shape is stored masked and an outbound
  snippet is redacted again.
- **Power and caps:** a fake power-notification source wakes the worker with no
  timer in the code path (source-scanning, non-empty candidate set); a queue of
  3,000 chunks embeds 2,000 and stays queued; a folder of 20 notes triggers no
  extraction without a press.
- **Typed text:** the palette line reaches Ask my documents; an empty line is
  refused with a card.
- **Tokenizer:** a Latin fixture finds a word; a CJK fixture documents the limit
  (or passes, if the trigram table is adopted).
- **Manual checks (named per slice):** the watcher with a real editor save (the
  atomic-rename save pattern), a OneDrive on-demand file, idle watts and RSS with
  the watcher on, a real PDF resume, the real Ollama embed call with
  `size_vram` unchanged by the embed model, the Memory window with a screen
  reader and in high contrast.

## Slice plan

Each slice names the caller and the observable that would differ if it were
wired to nothing. Docs ship with each slice (#97): `docs/knowledge.md` starts in
K1 and `PRIVACY.md` gains its Knowledge section in the same PR that first stores
anything, because its own rule is that it changes in the change that changes the
behavior.

| # | slice | issues | useful alone because | caller | observable |
|---|---|---|---|---|---|
| K1 | Knowledge root, watcher, reconciliation, md/txt/html/csv/json parsers, chunker with payment and national-id masking (the national-id scan is a pure function added to `payment_denylist.rs` here), `document`/`chunk`/`chunk_fts`, full-text retrieval, **the `Text` input and the palette passing its line to an action**, Ask my documents, tray and Settings entries, `docs/knowledge.md` and PRIVACY.md section. No new crate, no network at all. Adds the `Win32_Storage_FileSystem` and `Win32_System_IO` features. Records the exe size and raises the gate if it must | #81, parts of #82, #83, #85, #92, #97 | the user can ask a question of their notes offline, with citations, today | the watcher thread; `generic_inputs::plan` for `knowledge` and `text`; the Ask my documents action | Drop `notes.md` into the folder; within 2 s the tray status shows 1 file; type a question in the palette, run Ask my documents, and the card cites `notes.md`; delete the file and `chunk` has no rows for it; idle CPU is 0% |
| K2 | `wingman-parse` second binary: pdf and docx, Job limits (`Win32_System_JobObjects`), packaging, signing and the install check, CI smoke test; image OCR in process | #82 | resumes and contracts are searchable | the ingest worker's parser dispatch | A two-page PDF resume and a .docx both answer a question; a corrupt PDF becomes a `failed` row and the tray stays up; the installed package contains both signed files |
| K3 | `Embedder`, local Ollama first, model pull offer, `chunk_embedding`, `VectorIndex`, RRF, the measurement, the AC power notification (`Win32_System_Power`), per-batch caps, cloud embedders behind the per-folder setting and the preview, a generic JSON post helper in `common.rs` if the check needs one | #84, #85 | "employer" finds "company"; Offline still works | the ingest worker's embed step; retrieval | With Offline on a packet capture shows only loopback during ingest; the phrase from the resume ranks first; a model switch leaves search working and the page shows the re-embed count; unplugging and replugging changes when the deferred batch runs, with no timer |
| K4 | Memory migration, `store/memory.rs`, denylist at the boundary (reusing K1's national-id scan; also applied to outbound snippet redaction), profile folded in (see below), Memory window (list, search, edit, delete, pin, forget by source), export and import; depends on the manifest decision for the list view | #86, #88 | the user can see and fix what Wingman knows | the Memory window; Profile read path | A 16-digit number typed in is refused with a card; delete a fact and a retrieval no longer returns it; export then import round-trips |
| K5 | Fact extraction behind an explicit button, quote verification, `fact`-only output, unreviewed items not attached, review notice and card | #87 | a resume becomes facts without typing | the Learn button, the ingest worker's learn step | Press Learn on a resume; the notice names N facts each with a source span; none is attached to a request until reviewed; deleting one removes it from retrieval |
| K6 | Knowledge and memory chip, egress labels, Fill this form and cover letter use retrieval, preview source column | #89, #90, #91 | the original job-application use | `generic_inputs::plan`, the fill path, the palette | Fill a job application; the Source column names resume.pdf; the egress log names the snippets; chip off sends none |
| K7 | Remember this, `remember[]` chips, corrections, style, hygiene | #93, #94, #95 | Wingman stops repeating itself | result card, confirm step | Edit a salutation once; the next draft uses it and says why |
| K8 | Vault root, wikilink handling, Save as note executor (no ledger) | #96 | notes written back where the user already keeps them | the executor registry | A vault is indexed; Save as note creates a file Obsidian shows at once and exactly one `document` row appears |

K1 is the smallest useful slice: it has no dependency on the embedding work, the
memory model, the parser binary or the Memory window, adds no crate, and makes no
network request except the one the user starts by asking. K2 to K8 can be
reordered after it. K4 needs the store's profile slice. K6 needs the generic
`knowledge` input from K1 and K4's memory for facts (it works on chunks alone
before K4).

**Migration order and the store spec.** The store spec's slice table lists
profile (v3), memory (v4), knowledge (v5). This track lands K1's knowledge tables
before K4's memory table, so the roles of rows 4 and 5 may swap. Version numbers
are assigned at merge (store spec). When this spec is accepted, K1's PR adds a
one-line order note under the store spec's slice table, so the two documents do
not disagree (not done here, to keep this change to one file).

**The profile in K4.** The store spec's own rule applies: the K4 migration copies
each `profile_field` row into `memory_item` with `kind = 'profile_field'` and
**drops the old table in the same migration**, and `Profile::load` and `save`
read and write `memory_item` from then on. There is no period in which two
tables hold the profile and no "view" over them; `profile.bin` and the
`profile_field` table are not written after the copy, so they cannot diverge.

## Open questions, with a recommended answer each

1. **Create the folder at first run, or on first use?** #81 says first run.
   Recommended: on first use (tray "Open folder" or turning Knowledge on). A
   folder in Documents and a watcher thread for users who never use the feature is
   a cost with no benefit.
2. **Cloud embedding and extraction: allowed at all?** Recommended: yes, behind an
   explicit per-provider **and per-folder** setting with the consent sentence and
   a preview of chunk count, estimated cost and the first file names, never chosen
   by Mode Auto, never in Offline, always redacted. It is the only way to index
   with good quality on a machine with no local model.
3. **Parsers in a second binary or in the tray?** Recommended: second binary
   (crash isolation under `panic = "abort"` and the 99 KB size headroom). The
   cost is packaging and signing one more file and an install check; K2 handles
   it.
4. **sqlite-vec or Rust brute force?** Recommended: Rust brute force behind
   `VectorIndex`; revisit only on K3's measurement, then int8, then `hnsw_rs`,
   then `sqlite-vec` once it has a stable release.
5. **DPAPI on chunks?** The store spec decided no (full-text search needs
   plaintext and the source file is plaintext anyway). Recommended: keep that, and
   say it in PRIVACY.md and docs/knowledge.md in plain words, including that
   facts are encrypted and the same text in chunks is not, and that file names
   appear in the local egress log.
6. **Is dropping a file consent to extract with a cloud model?** Plan section 18
   says dropping is consent. Recommended: it is consent to index locally only.
   Cloud extraction is explicit (question 2). Otherwise Wingman would send a
   resume to a vendor on a file event, which contradicts the no-background-requests
   promise.
7. **Automatic local extraction or button only?** Revised recommendation: **button
   only by default**, with a count and an estimate of local calls shown before it
   runs ("Learn from 6 files, about 14 model calls"). Earlier draft: automatic on
   AC power with a ready local chat model. Counter-argument for automatic: a
   user who drops a resume expects facts without another click, and AC power
   removes the battery cost. Against: dropping a folder of 200 notes would queue up
   to 6 local calls per file on the iGPU in the background, and AGENTS.md treats
   idle drain as a bug measured to the tenth of a watt. Middle option if the
   button feels like friction: automatic only for a designated `Profile`
   subfolder, with the per-batch cap (5 files) in "Size and performance budgets".
8. **Knowledge chip default.** Recommended: one chip for knowledge and memory,
   on for actions whose `inputs` list `knowledge` or `memory`, absent when the
   store is empty, off for every other action, which never read the store.
9. **Scanned PDFs.** Recommended: out of K2; mark `skipped: no text layer`. A later
   slice renders with `Windows.Data.Pdf` and OCRs, with no new crate.
10. **Memory page: Win32 first or wait for WebView2 (#44)?** Recommended: Win32
    list view first, on a pure model that the WebView2 page can reuse. The list
    view's theming depends on the common-controls manifest question in the
    Settings dark-mode spec (no manifest exists today), so K4 either adopts that
    manifest or draws the list itself.
11. **File hashing: CNG or `sha2`?** Recommended: CNG through the `windows` crate
    already in use (zero added size); `sha2` (MIT OR Apache-2.0) if the wrapper is
    judged not worth owning.
12. **Chunk size in tokens without a tokenizer.** Recommended: characters divided by
    four, labelled an estimate, since it only sizes chunks and budgets. A real
    tokenizer adds a crate and a vocabulary file for no visible gain.
13. **Full-text tokenizer.** Earlier recommendation: `unicode61 remove_diacritics 2`,
    no stemming, "works for every language". That is wrong for Chinese, Japanese
    and Korean (see "Full-text tokenizer and languages"). Recommended now:
    `unicode61` in K1 with the CJK limit documented, and a `trigram` table added
    only if a user needs it. Counter-argument: `trigram` fixes CJK and substring
    search from the start, at about three times the index size and with a
    three-character minimum, and adding it later means a re-index. Decide from a
    measured fixture in K1.
14. **Retention of removed documents' facts.** Recommended: ask on removal, keep by
    default (the user may have deleted the file for tidiness, and a fact the user
    edited is authored data). Kept facts stay marked with their source label.
15. **May a cloud request ever carry personal memory (`contact` items)?**
    Recommended: no by default; they go to local providers only, and the preview
    says how many were left out. Counter-argument: a cover letter or a fill is
    exactly where the user's name, address and phone matter, and the user has
    already chosen a cloud provider for that action. Middle option: a per-item
    "ok to send" the user sets once.
16. **Unreviewed facts: excluded from requests, or included and marked?**
    Recommended: excluded until reviewed, so a poisoned document cannot steer
    anything before a person has seen it. Counter-argument: the point of
    extraction is to save typing, and an extra review step before first use is
    friction. Middle option: attach unreviewed facts to the request that triggered
    the learn, never to later ones.

## Risks

- **Parser attack surface.** Untrusted files from the user's disk. Mitigated by the
  separate process, the Job limits, the caps and fixtures. Residual: a parser
  vulnerability still runs as the user, in a process with no network code.
- **Prompt injection from documents.** Snippets are data in a marked block and no
  action executes without confirm; the Ask card shows only verified quotes.
- **A small local model extracts badly.** Shown as confidence and counts; review is
  one screen; extraction can be switched off.
- **Plaintext chunks.** An account-level attacker with the user's own session reads
  them; so they can the original files. Stated plainly in docs.
- **OneDrive or antivirus interaction.** Reading many files can trigger downloads or
  scans; the on-demand skip, the file cap and background priority limit it.
- **Exe size.** About 99 KB of headroom, shared with three other specs, against an
  estimate of roughly 170 to 190 KB for this track. A gate raise is planned, one
  measured PR at a time (see "Size and performance budgets").
- **Memory as an injection channel.** Mitigated by review before attach,
  fact-only extraction, user-authored-only `preference` and `style`, and the
  marked block. Residual: a user who reviews in bulk without reading.
- **Migration order.** K1 and K4 both add migrations; numbers are assigned at merge,
  the contiguity test catches a race, and K1's PR adds a one-line note to the store
  spec that the roles of its rows 4 and 5 may swap.
