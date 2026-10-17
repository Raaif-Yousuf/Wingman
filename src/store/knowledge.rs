//! Knowledge roots and documents: the rows the ingest scan reads and writes.
//! The SQL for these tables stays in `store/` (knowledge spec, test strategy).

use super::{Store, StoreError};
use rusqlite::params;

/// What the store remembers about one file, enough to decide whether it
/// changed without reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentRow {
    pub id: i64,
    pub rel_path: String,
    pub size: Option<i64>,
    pub mtime_unix: Option<i64>,
    pub content_hash: Option<String>,
    pub parser_ver: Option<i64>,
    pub status: String,
    pub ingested_unix: Option<i64>,
}

/// The fields of a document row the scan sets. `content_hash` is `None` while
/// the file has never been read, and is kept as is by [`Store::mark_document`]
/// when a failure leaves the older index in place.
#[derive(Debug, Clone, Default)]
pub struct DocumentUpdate<'a> {
    pub title: Option<&'a str>,
    pub content_hash: Option<&'a str>,
    pub size: Option<i64>,
    pub mtime_unix: Option<i64>,
    pub parser: Option<&'a str>,
    pub parser_ver: Option<i64>,
    pub ingested_unix: Option<i64>,
}

/// One full-text match: a chunk with where it came from. `span` is the raw
/// JSON the chunker wrote.
#[derive(Debug, Clone)]
pub struct ChunkHit {
    pub doc_id: i64,
    pub ordinal: i64,
    pub text: String,
    pub span: Option<String>,
    pub rel_path: String,
}

impl Store {
    /// The best `limit` chunks for an FTS5 `MATCH` expression, best first
    /// (`bm25`), from enabled roots only. The caller builds `fts_query` from
    /// quoted terms (`knowledge::retrieve::fts_query`), never from raw text.
    #[cfg_attr(not(test), expect(dead_code))]
    pub fn search_chunks(
        &self,
        fts_query: &str,
        limit: usize,
    ) -> Result<Vec<ChunkHit>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT c.doc_id, c.ordinal, c.text, c.span, d.rel_path
             FROM chunk_fts
             JOIN chunk c ON c.id = chunk_fts.rowid
             JOIN document d ON d.id = c.doc_id
             JOIN knowledge_root r ON r.id = d.root_id
             WHERE chunk_fts MATCH ?1 AND r.enabled = 1
             ORDER BY bm25(chunk_fts), c.id
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |r| {
            Ok(ChunkHit {
                doc_id: r.get(0)?,
                ordinal: r.get(1)?,
                text: r.get(2)?,
                span: r.get(3)?,
                rel_path: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The id of the root at `path`, creating it as a `default` root if new.
    pub fn ensure_root(&self, path: &str, now_unix: i64) -> Result<i64, StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO knowledge_root (path, kind, added_unix)
             VALUES (?1, 'default', ?2)",
            params![path, now_unix],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM knowledge_root WHERE path = ?1",
            [path],
            |r| r.get(0),
        )?)
    }

    /// Removes every `default` root except the one at `keep` (none: all of
    /// them), with its documents, chunks and full-text rows, then checkpoints
    /// the log so the text does not linger in it. Touches nothing when there
    /// is nothing to remove. Returns how many roots were removed.
    pub fn retire_default_roots_except(&self, keep: Option<&str>) -> Result<usize, StoreError> {
        let conn = self.lock();
        let removed = conn.execute(
            "DELETE FROM knowledge_root
             WHERE kind = 'default' AND (?1 IS NULL OR path <> ?1)",
            params![keep],
        )?;
        if removed > 0 {
            // Best effort: a busy checkpoint is not a failed delete.
            let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
        Ok(removed)
    }

    /// Every document of a root.
    pub fn documents_in_root(&self, root_id: i64) -> Result<Vec<DocumentRow>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, rel_path, size, mtime_unix, content_hash, parser_ver, status, ingested_unix
             FROM document WHERE root_id = ?1",
        )?;
        let rows = stmt.query_map([root_id], |r| {
            Ok(DocumentRow {
                id: r.get(0)?,
                rel_path: r.get(1)?,
                size: r.get(2)?,
                mtime_unix: r.get(3)?,
                content_hash: r.get(4)?,
                parser_ver: r.get(5)?,
                status: r.get(6)?,
                ingested_unix: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Creates or finds the document row for `rel_path` and sets its status
    /// and the fields in `u`. A field left `None` keeps its stored value, so a
    /// failed re-read does not forget the hash its older chunks belong to.
    /// `error` is plain words, never file content. Returns the document id.
    pub fn mark_document(
        &self,
        root_id: i64,
        rel_path: &str,
        status: &str,
        error: Option<&str>,
        u: &DocumentUpdate<'_>,
    ) -> Result<i64, StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO document (root_id, rel_path, status) VALUES (?1, ?2, ?3)
             ON CONFLICT (root_id, rel_path) DO NOTHING",
            params![root_id, rel_path, status],
        )?;
        conn.execute(
            "UPDATE document SET
                 status = ?3, error = ?4,
                 title = COALESCE(?5, title),
                 content_hash = COALESCE(?6, content_hash),
                 size = COALESCE(?7, size),
                 mtime_unix = COALESCE(?8, mtime_unix),
                 parser = COALESCE(?9, parser),
                 parser_ver = COALESCE(?10, parser_ver),
                 ingested_unix = COALESCE(?11, ingested_unix)
             WHERE root_id = ?1 AND rel_path = ?2",
            params![
                root_id,
                rel_path,
                status,
                error,
                u.title,
                u.content_hash,
                u.size,
                u.mtime_unix,
                u.parser,
                u.parser_ver,
                u.ingested_unix
            ],
        )?;
        conn.query_row(
            "SELECT id FROM document WHERE root_id = ?1 AND rel_path = ?2",
            params![root_id, rel_path],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    /// Removes documents (and, by cascade and trigger, their chunks and
    /// full-text rows), then checkpoints the log so the deleted text does not
    /// linger in it. A delete the user caused by deleting the file.
    pub fn delete_documents(&self, ids: &[i64]) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.lock();
        for id in ids {
            conn.execute("DELETE FROM document WHERE id = ?1", [id])?;
        }
        // Best effort: a busy checkpoint is not a failed delete.
        let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        Ok(())
    }

    /// The stored chunks of one document as `(chunk id, text)`, by ordinal.
    #[cfg(test)]
    pub(crate) fn chunks_of(
        &self,
        root_id: i64,
        rel_path: &str,
    ) -> Result<Vec<(i64, String)>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.text FROM chunk c JOIN document d ON d.id = c.doc_id
             WHERE d.root_id = ?1 AND d.rel_path = ?2 ORDER BY c.ordinal",
        )?;
        let rows = stmt.query_map(params![root_id, rel_path], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// `(status, error)` of one document, if it has a row.
    #[cfg(test)]
    pub(crate) fn document_state(
        &self,
        root_id: i64,
        rel_path: &str,
    ) -> Result<Option<(String, Option<String>)>, StoreError> {
        use rusqlite::OptionalExtension;
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT status, error FROM document WHERE root_id = ?1 AND rel_path = ?2",
                params![root_id, rel_path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// The stored title of one document.
    #[cfg(test)]
    pub(crate) fn document_title(
        &self,
        root_id: i64,
        rel_path: &str,
    ) -> Result<Option<String>, StoreError> {
        Ok(self.lock().query_row(
            "SELECT title FROM document WHERE root_id = ?1 AND rel_path = ?2",
            params![root_id, rel_path],
            |r| r.get(0),
        )?)
    }

    /// Turns a root on or off by its path.
    #[cfg(test)]
    pub(crate) fn set_root_enabled(&self, path: &str, enabled: bool) -> Result<(), StoreError> {
        self.lock().execute(
            "UPDATE knowledge_root SET enabled = ?2 WHERE path = ?1",
            params![path, enabled],
        )?;
        Ok(())
    }

    /// How many roots, documents and chunks the store holds.
    #[cfg(test)]
    pub(crate) fn knowledge_counts(&self) -> Result<(i64, i64, i64), StoreError> {
        let conn = self.lock();
        let n = |t: &str| conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0));
        Ok((n("knowledge_root")?, n("document")?, n("chunk")?))
    }

    /// How many `chunk_fts` rows match `term`: the full-text index followed
    /// the chunk rows.
    #[cfg(test)]
    pub(crate) fn fts_hits(&self, term: &str) -> Result<i64, StoreError> {
        let conn = self.lock();
        Ok(conn.query_row(
            "SELECT count(*) FROM chunk_fts WHERE chunk_fts MATCH ?1",
            [format!("\"{term}\"")],
            |r| r.get(0),
        )?)
    }
}
