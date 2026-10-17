-- Version 4: the knowledge folder (spec 2026-10-16-knowledge-and-memory-design).
-- Never edit this file once shipped; fix mistakes with a later version.
--
-- knowledge_root: a folder the user lets Wingman read. `path` is absolute and
--   canonical. `kind` is default | extra | vault.
-- document: one file under a root. `rel_path` is relative to the root, so
--   removing a root is one cascade and moving a vault does not look like a new
--   corpus. `content_hash` is the SHA-256 of the file bytes: an equal hash and
--   a current `parser_ver` mean the file is skipped. `error` is plain words,
--   never file content.
-- chunk: a slice of a document's text. `span` is JSON
--   {"page", "heading", "start", "end"}. `text_hash` lets an edit keep the
--   chunks that did not change. Chunks and the full-text index are plaintext
--   under the owner-only store directory (spec decision, same as the files).
-- chunk_fts: full-text index over chunk.text, kept in step by triggers. The
--   unicode61 tokenizer does not split Chinese, Japanese or Korean runs into
--   words (documented in docs/knowledge.md).
--
-- Later migrations add embeddings and memory; nothing here is a placeholder.

CREATE TABLE knowledge_root (
    id          INTEGER PRIMARY KEY,
    path        TEXT NOT NULL UNIQUE,
    kind        TEXT NOT NULL CHECK (kind IN ('default', 'extra', 'vault')),
    enabled     INTEGER NOT NULL DEFAULT 1,
    note_target INTEGER NOT NULL DEFAULT 0,
    added_unix  INTEGER NOT NULL
);

CREATE TABLE document (
    id            INTEGER PRIMARY KEY,
    root_id       INTEGER NOT NULL REFERENCES knowledge_root (id) ON DELETE CASCADE,
    rel_path      TEXT NOT NULL,
    title         TEXT,
    content_hash  TEXT,
    size          INTEGER,
    mtime_unix    INTEGER,
    parser        TEXT,
    parser_ver    INTEGER,
    status        TEXT NOT NULL CHECK (status IN ('pending', 'indexed', 'failed', 'skipped')),
    error         TEXT,
    ingested_unix INTEGER,
    learned_hash  TEXT,
    UNIQUE (root_id, rel_path)
);

CREATE TABLE chunk (
    id        INTEGER PRIMARY KEY,
    doc_id    INTEGER NOT NULL REFERENCES document (id) ON DELETE CASCADE,
    ordinal   INTEGER NOT NULL,
    text      TEXT NOT NULL,
    span      TEXT,
    text_hash TEXT NOT NULL,
    tokens    INTEGER NOT NULL
);

CREATE INDEX chunk_doc ON chunk (doc_id, ordinal);

CREATE VIRTUAL TABLE chunk_fts USING fts5 (
    text,
    content = 'chunk',
    content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER chunk_fts_insert AFTER INSERT ON chunk BEGIN
    INSERT INTO chunk_fts (rowid, text) VALUES (new.id, new.text);
END;

CREATE TRIGGER chunk_fts_delete AFTER DELETE ON chunk BEGIN
    INSERT INTO chunk_fts (chunk_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;

CREATE TRIGGER chunk_fts_update AFTER UPDATE OF text ON chunk BEGIN
    INSERT INTO chunk_fts (chunk_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO chunk_fts (rowid, text) VALUES (new.id, new.text);
END;
