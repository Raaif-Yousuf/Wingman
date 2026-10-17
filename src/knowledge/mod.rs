//! Knowledge folder: the pure foundation (spec
//! `docs/superpowers/specs/2026-10-16-knowledge-and-memory-design.md`, K1).
//!
//! This module holds only pure code: parsing text-like files, chunking, and
//! deciding which stored chunks survive an edit. There is no watcher, no
//! thread, no timer and no network here, so nothing runs while idle (AGENTS.md
//! rule 5). Later slices call it from the ingest worker.
//!
//! Every function takes bytes or text and returns a value or a plain-words
//! error; none panics on file contents.

pub mod chunk;
pub mod diff;
pub mod hash;
pub mod parse;

#[cfg(test)]
mod tests {
    use super::chunk::{chunk_text, TARGET_CHARS};
    use super::diff::diff_chunks;
    use super::hash::sha256_hex;
    use super::parse::{parse, Parser};

    fn hashes(md: &str) -> Vec<String> {
        let parsed = parse(Parser::Markdown, md.as_bytes()).unwrap();
        chunk_text(&parsed.text)
            .iter()
            .map(|c| sha256_hex(c.text.as_bytes()).unwrap())
            .collect()
    }

    #[test]
    fn editing_one_section_keeps_the_chunks_of_the_others() {
        // The done-when of #83, on the pure pipeline: parse, chunk, hash, diff.
        let section = |name: &str, body: &str| format!("# {name}\n{}\n\n", body.repeat(40));
        let before = [
            section("Work", "I joined Acme in 2019. "),
            section("Education", "BSc Physics, 2015. "),
            section("Skills", "Rust and Win32. "),
        ]
        .concat();
        let after = before.replace("BSc Physics, 2015.", "MSc Physics, 2017.");
        let old = hashes(&before);
        let new = hashes(&after);
        assert_eq!(old.len(), new.len());
        let stored: Vec<(i64, String)> = old
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, h)| (i as i64 + 1, h))
            .collect();

        let d = diff_chunks(&stored, &new);

        assert!(
            !d.keep.is_empty(),
            "unchanged sections must keep their rows"
        );
        assert!(!d.insert.is_empty(), "the edited section is new text");
        assert_eq!(d.insert.len(), d.delete.len());
        assert_eq!(d.keep.len() + d.insert.len(), new.len());
        // An unchanged file keeps everything.
        let same = diff_chunks(&stored, &old);
        assert_eq!(same.keep.len(), old.len());
        assert!(same.insert.is_empty() && same.delete.is_empty());
    }

    /// A small deterministic generator, so the test needs no crate and no seed
    /// from the clock.
    fn next(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 33
    }

    #[test]
    fn arbitrary_bytes_never_panic_and_chunks_stay_well_formed() {
        let mut state = 7u64;
        for round in 0..300 {
            let len = (next(&mut state) % 6000) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|_| match next(&mut state) % 10 {
                    0 => b'\n',
                    1 => b'#',
                    2 => b'|',
                    3 => b' ',
                    4 => 0x0c,
                    5 => (next(&mut state) % 256) as u8,
                    _ => b'a' + (next(&mut state) % 26) as u8,
                })
                .collect();
            for parser in [Parser::Text, Parser::Markdown] {
                let Ok(parsed) = parse(parser, &bytes) else {
                    continue;
                };
                let chunks = chunk_text(&parsed.text);
                for (i, c) in chunks.iter().enumerate() {
                    assert_eq!(c.ordinal, i, "round {round}");
                    assert_eq!(parsed.text.get(c.start..c.end), Some(c.text.as_str()));
                    assert!(c.text.chars().count() <= TARGET_CHARS);
                }
            }
        }
    }
}
