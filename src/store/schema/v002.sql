-- Version 2: the egress log, one row per outbound request attempt.
-- Never edit this file once shipped; fix mistakes with a later version.
--
-- Rows hold kinds, sizes and counts, never content. `attachments`,
-- `snippets` and `redactions` are JSON written and read with the row and
-- never queried on their own. `trigger_label` is `trigger` in the Rust type
-- (TRIGGER is an SQL keyword).

CREATE TABLE egress (
    id             INTEGER PRIMARY KEY,
    time_unix      INTEGER NOT NULL,
    provider       TEXT NOT NULL,
    model          TEXT,
    retried        INTEGER NOT NULL DEFAULT 0,
    trigger_label  TEXT,
    attachments    TEXT NOT NULL DEFAULT '[]',
    snippets       TEXT NOT NULL DEFAULT '[]',
    redactions     TEXT NOT NULL DEFAULT '{}',
    bytes_sent     INTEGER NOT NULL DEFAULT 0,
    bytes_received INTEGER NOT NULL DEFAULT 0,
    outcome        TEXT NOT NULL,
    error          TEXT,
    cost           REAL
);

CREATE INDEX egress_time ON egress (time_unix);
