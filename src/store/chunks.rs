//! Re-indexing one document's chunks after an edit.
//!
//! A chunk whose text hash is unchanged keeps its row (and, later, its
//! embedding). But its `ordinal` and `span` (page, heading, offsets) describe
//! where the text sits now, so they are rewritten on every kept row in the
//! same transaction. Otherwise a citation such as "resume.pdf, page 2" could
//! name a page the text has since left.

use super::{Store, StoreError};
use crate::knowledge::chunk::Chunk;
use crate::knowledge::diff::{diff_chunks, ChunkDiff};
use rusqlite::params;

/// A freshly chunked piece of text with the hash of its text.
pub struct IndexedChunk {
    pub chunk: Chunk,
    pub text_hash: String,
}

impl Store {
    /// Brings the stored chunks of `doc_id` in line with `new`, in one
    /// transaction: vanished text is deleted, kept rows get their new ordinal
    /// and span, new text is inserted. Returns the diff that was applied. On
    /// any error the transaction rolls back and the old rows stay as they were.
    #[cfg_attr(not(test), expect(dead_code))]
    pub fn replace_chunks(
        &self,
        doc_id: i64,
        new: &[IndexedChunk],
    ) -> Result<ChunkDiff, StoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let existing: Vec<(i64, String)> = {
            let mut stmt = tx.prepare("SELECT id, text_hash FROM chunk WHERE doc_id = ?1")?;
            let rows = stmt.query_map([doc_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let hashes: Vec<String> = new.iter().map(|n| n.text_hash.clone()).collect();
        let diff = diff_chunks(&existing, &hashes);
        for id in &diff.delete {
            tx.execute("DELETE FROM chunk WHERE id = ?1", [id])?;
        }
        for (id, ordinal) in &diff.keep {
            let n = new_at(new, *ordinal)?;
            tx.execute(
                "UPDATE chunk SET ordinal = ?1, span = ?2 WHERE id = ?3",
                params![*ordinal as i64, n.chunk.span_json(), id],
            )?;
        }
        for ordinal in &diff.insert {
            let n = new_at(new, *ordinal)?;
            tx.execute(
                "INSERT INTO chunk (doc_id, ordinal, text, span, text_hash, tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    doc_id,
                    *ordinal as i64,
                    n.chunk.text,
                    n.chunk.span_json(),
                    n.text_hash,
                    n.chunk.tokens as i64
                ],
            )?;
        }
        tx.commit()?;
        Ok(diff)
    }
}

/// The new chunk at `ordinal`. The diff is built from `new`, so a miss means
/// a bug; failing rolls the transaction back instead of dropping a chunk.
fn new_at(new: &[IndexedChunk], ordinal: usize) -> Result<&IndexedChunk, StoreError> {
    new.get(ordinal)
        .ok_or_else(|| StoreError::Other(format!("chunk index {ordinal} is out of range")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::chunk::chunk_text;
    use crate::knowledge::hash::sha256_hex;

    fn indexed(text: &str) -> Vec<IndexedChunk> {
        chunk_text(text)
            .into_iter()
            .map(|chunk| IndexedChunk {
                text_hash: sha256_hex(chunk.text.as_bytes()).unwrap(),
                chunk,
            })
            .collect()
    }

    fn store_with_doc() -> Store {
        let s = Store::in_memory();
        s.lock()
            .execute_batch(
                "INSERT INTO knowledge_root (id, path, kind, added_unix)
                     VALUES (1, 'C:/Notes', 'default', 10);
                 INSERT INTO document (id, root_id, rel_path, status)
                     VALUES (1, 1, 'notes.md', 'indexed');",
            )
            .unwrap();
        s
    }

    /// `(ordinal, span json, first line)` of each stored chunk, by ordinal.
    fn stored(s: &Store) -> Vec<(i64, String, String)> {
        let conn = s.lock();
        let mut stmt = conn
            .prepare("SELECT ordinal, span, text FROM chunk WHERE doc_id = 1 ORDER BY ordinal")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                let text: String = r.get(2)?;
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    text.lines().next().unwrap_or("").to_string(),
                ))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn doc(sections: &[(&str, &str)]) -> String {
        sections
            .iter()
            .map(|(h, b)| format!("# {h}\n{}\n\n", b.repeat(40)))
            .collect()
    }

    #[test]
    fn a_kept_chunk_follows_its_text_when_a_section_is_inserted_before_it() {
        let s = store_with_doc();
        let before = doc(&[("Work", "I joined Acme. "), ("Skills", "Rust and Win32. ")]);
        s.replace_chunks(1, &indexed(&before)).unwrap();
        let first = stored(&s);
        assert_eq!(first.len(), 2);

        let after = doc(&[
            ("Intro", "Hello there. "),
            ("Work", "I joined Acme. "),
            ("Skills", "Rust and Win32. "),
        ]);
        let want = indexed(&after);
        let d = s.replace_chunks(1, &want).unwrap();
        assert_eq!(d.keep.len(), 2, "both old sections are kept");
        assert_eq!(d.insert.len(), 1);

        let now = stored(&s);
        assert_eq!(now.len(), 3);
        for (i, w) in want.iter().enumerate() {
            assert_eq!(now[i].0, i as i64, "ordinal of {}", now[i].2);
            assert_eq!(now[i].1, w.chunk.span_json(), "span of {}", now[i].2);
        }
        // The kept "Work" chunk moved from offset 0, so its span changed.
        assert_ne!(first[0].1, now[1].1);
    }

    #[test]
    fn an_unchanged_file_changes_nothing() {
        let s = store_with_doc();
        let a = "# One\nsame body text\n";
        s.replace_chunks(1, &indexed(a)).unwrap();
        let again = s.replace_chunks(1, &indexed(a)).unwrap();
        assert!(again.insert.is_empty() && again.delete.is_empty());
        assert_eq!(stored(&s).len(), 1);
    }

    #[test]
    fn a_failed_reindex_leaves_the_old_rows() {
        let s = store_with_doc();
        let old = doc(&[("A", "alpha. "), ("B", "beta. ")]);
        s.replace_chunks(1, &indexed(&old)).unwrap();
        let before = stored(&s);
        assert_eq!(before.len(), 2);
        // The new text deletes "A", moves the kept "B", then inserts two
        // chunks; the second insert fails, after the delete and the update.
        s.lock()
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON chunk WHEN NEW.ordinal = 2
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let new = doc(&[("B", "beta. "), ("C", "gamma. "), ("D", "delta. ")]);
        let err = s.replace_chunks(1, &indexed(&new));
        assert!(err.is_err());
        assert_eq!(stored(&s), before, "rolled back, spans included");
    }

    #[test]
    fn an_impossible_index_is_an_error_not_a_dropped_chunk() {
        let new = indexed(
            "# A
alpha
",
        );
        assert!(new_at(&new, 0).is_ok());
        assert!(new_at(&new, 1).is_err());
    }
}
