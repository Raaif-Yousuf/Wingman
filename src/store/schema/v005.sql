-- Version 5: the memory model (spec 2026-10-16-knowledge-and-memory-design,
-- "Memory migration (K4)").
-- Never edit this file once shipped; fix mistakes with a later version.
--
-- memory_item: one thing Wingman knows about the user. `text` and
--   `structured` are each their own DPAPI envelope, so the table never holds
--   readable text. Because of that, equal texts cannot be found by SQL:
--   `dedupe` is a keyed hash of the normalised text (the key is in `meta`).
--   `reviewed_unix` NULL means a person has not looked at it yet; such an item
--   is never attached to a request. `local_only` keeps it off cloud requests.
-- memory_source: where an item came from. One item can have many sources, and
--   a deleted document leaves the item with its label (doc_id becomes NULL).
--
-- The profile still lives in profile_field. Folding it into memory_item is a
-- later migration.

CREATE TABLE memory_item (
    id             INTEGER PRIMARY KEY,
    kind           TEXT NOT NULL CHECK (kind IN
                       ('fact', 'preference', 'correction', 'profile_field', 'style')),
    key            TEXT,
    text           BLOB NOT NULL,
    structured     BLOB,
    dedupe         TEXT NOT NULL,
    confidence     REAL NOT NULL DEFAULT 1.0,
    created_unix   INTEGER NOT NULL,
    updated_unix   INTEGER NOT NULL,
    last_used_unix INTEGER,
    uses           INTEGER NOT NULL DEFAULT 0,
    pinned         INTEGER NOT NULL DEFAULT 0,
    local_only     INTEGER NOT NULL DEFAULT 0,
    reviewed_unix  INTEGER,
    UNIQUE (kind, key, dedupe)
);

-- SQLite treats NULL keys as distinct in the UNIQUE above, so facts and
-- preferences (key NULL) need this to be unique too.
CREATE UNIQUE INDEX memory_item_dedupe ON memory_item (kind, COALESCE(key, ''), dedupe);

CREATE TABLE memory_source (
    id          INTEGER PRIMARY KEY,
    item_id     INTEGER NOT NULL REFERENCES memory_item (id) ON DELETE CASCADE,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('doc', 'action', 'user')),
    doc_id      INTEGER REFERENCES document (id) ON DELETE SET NULL,
    label       TEXT NOT NULL,
    span        TEXT,
    added_unix  INTEGER NOT NULL
);

CREATE INDEX memory_source_item ON memory_source (item_id);
