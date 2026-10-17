//! Which stored chunks survive an edit.
//!
//! Chunks are matched by the hash of their text. A chunk whose text did not
//! change keeps its row (and, later, its embedding); only new text is inserted
//! and only vanished text is deleted. This is the done-when of #83: edit one
//! paragraph and every other chunk stays.

use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq, Default)]
pub struct ChunkDiff {
    /// `(row id, new ordinal)` for each stored chunk that stays.
    pub keep: Vec<(i64, usize)>,
    /// Indexes into the new chunk list that need a new row.
    pub insert: Vec<usize>,
    /// Row ids that no longer have matching text.
    pub delete: Vec<i64>,
}

/// `existing` is `(row id, text_hash)` as stored, `new_hashes` the hashes of
/// the freshly chunked text in order. Identical text appearing twice is
/// matched one for one: a stored row is never claimed by two new chunks.
pub fn diff_chunks(existing: &[(i64, String)], new_hashes: &[String]) -> ChunkDiff {
    let mut by_hash: HashMap<&str, Vec<i64>> = HashMap::new();
    for (id, hash) in existing {
        by_hash.entry(hash.as_str()).or_default().push(*id);
    }
    // Hand out the lowest id first, so the oldest row is the one that stays.
    for ids in by_hash.values_mut() {
        ids.sort_unstable_by(|a, b| b.cmp(a));
    }
    let mut out = ChunkDiff::default();
    for (ordinal, hash) in new_hashes.iter().enumerate() {
        match by_hash.get_mut(hash.as_str()).and_then(|ids| ids.pop()) {
            Some(id) => out.keep.push((id, ordinal)),
            None => out.insert.push(ordinal),
        }
    }
    let mut left: Vec<i64> = by_hash.into_values().flatten().collect();
    left.sort_unstable();
    out.delete = left;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(rows: &[(i64, &str)]) -> Vec<(i64, String)> {
        rows.iter().map(|(i, h)| (*i, h.to_string())).collect()
    }
    fn h(hashes: &[&str]) -> Vec<String> {
        hashes.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_unchanged_file_keeps_everything() {
        let d = diff_chunks(&e(&[(1, "a"), (2, "b")]), &h(&["a", "b"]));
        assert_eq!(d.keep, vec![(1, 0), (2, 1)]);
        assert!(d.insert.is_empty() && d.delete.is_empty());
    }

    #[test]
    fn one_edited_paragraph_keeps_every_other_chunk() {
        let d = diff_chunks(&e(&[(1, "a"), (2, "b"), (3, "c")]), &h(&["a", "B2", "c"]));
        assert_eq!(d.keep, vec![(1, 0), (3, 2)]);
        assert_eq!(d.insert, vec![1]);
        assert_eq!(d.delete, vec![2]);
    }

    #[test]
    fn a_chunk_that_moves_keeps_its_row_with_a_new_ordinal() {
        let d = diff_chunks(&e(&[(1, "a"), (2, "b")]), &h(&["x", "a", "b"]));
        assert_eq!(d.keep, vec![(1, 1), (2, 2)]);
        assert_eq!(d.insert, vec![0]);
        assert!(d.delete.is_empty());
    }

    #[test]
    fn repeated_text_is_matched_one_for_one() {
        let d = diff_chunks(&e(&[(1, "a"), (2, "a")]), &h(&["a", "a", "a"]));
        assert_eq!(d.keep, vec![(1, 0), (2, 1)]);
        assert_eq!(d.insert, vec![2]);
        let d = diff_chunks(&e(&[(1, "a"), (2, "a"), (3, "a")]), &h(&["a"]));
        assert_eq!(d.keep, vec![(1, 0)]);
        assert_eq!(d.delete, vec![2, 3]);
    }

    #[test]
    fn nothing_stored_inserts_everything_and_an_empty_file_deletes_everything() {
        let d = diff_chunks(&[], &h(&["a", "b"]));
        assert_eq!(d.insert, vec![0, 1]);
        let d = diff_chunks(&e(&[(1, "a"), (2, "b")]), &[]);
        assert_eq!(d.delete, vec![1, 2]);
        assert!(d.keep.is_empty());
    }
}
