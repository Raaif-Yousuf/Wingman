-- Version 1: the bookkeeping table and the usage totals.
-- Never edit this file once shipped; fix mistakes with a later version.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

-- One row per local date, provider and model. The token columns are NULL
-- for rows imported from the old spend.json, which never recorded them.
CREATE TABLE usage_day (
    date               TEXT NOT NULL,
    provider           TEXT NOT NULL,
    model              TEXT NOT NULL,
    requests           INTEGER NOT NULL DEFAULT 0,
    unpriced           INTEGER NOT NULL DEFAULT 0,
    input_tokens       INTEGER,
    output_tokens      INTEGER,
    cache_write_tokens INTEGER,
    cache_read_tokens  INTEGER,
    usd                REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (date, provider, model)
) WITHOUT ROWID;
