//! Regression test for issue #162 (CLAUDE.md rule 11): no em dash (U+2014)
//! may appear in a string literal anywhere under `src/`. Comments are
//! exempt (the rule explicitly allows them), and `#[cfg(test)]` modules are
//! skipped as dev-only text rather than user-facing strings.
//!
//! Deliberately standalone: this crate has no `[lib]` target (see
//! `Cargo.toml`'s `[[bin]]`), so an integration test cannot `use
//! wingman::...`. It re-reads the same source files as plain text with a
//! small hand-rolled tokenizer instead, so it works regardless of how the
//! rest of the crate is organized.

use std::path::{Path, PathBuf};

/// One lexical context for the character-by-character scan below.
#[derive(Clone, Copy, PartialEq)]
enum State {
    Normal,
    LineComment,
    BlockComment(u32), // nesting depth
    Str,
    RawStr(u32), // number of `#` in the raw string's delimiter
    Char,
}

struct Violation {
    file: PathBuf,
    line: usize,
}

/// Recursively collects every `*.rs` file under `dir`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Scans one file's source text for U+2014 inside string / raw-string
/// literals, outside comments and outside any `#[cfg(test)]` module body.
fn scan(path: &Path, src: &str) -> Vec<Violation> {
    let mut violations = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut state = State::Normal;

    // Brace-depth tracking, used only to find the extent of a
    // `#[cfg(test)] mod ... { ... }` body so it can be skipped.
    let mut depth: i32 = 0;
    let mut test_mod_depths: Vec<i32> = Vec::new();
    let mut pending_test_attr = false;
    let mut awaiting_test_mod_brace = false;

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
        }

        match state {
            State::LineComment => {
                if c == '\n' {
                    state = State::Normal;
                }
                i += 1;
                continue;
            }
            State::BlockComment(d) => {
                if c == '/' && chars.get(i + 1) == Some(&'*') {
                    state = State::BlockComment(d + 1);
                    i += 2;
                    continue;
                }
                if c == '*' && chars.get(i + 1) == Some(&'/') {
                    state = if d > 1 { State::BlockComment(d - 1) } else { State::Normal };
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            State::Str => {
                if c == '\\' {
                    i += 2; // skip the escaped character, whatever it is
                    continue;
                }
                if c == '"' {
                    state = State::Normal;
                    i += 1;
                    continue;
                }
                if c == '\u{2014}' && test_mod_depths.is_empty() {
                    violations.push(Violation { file: path.to_path_buf(), line });
                }
                i += 1;
                continue;
            }
            State::RawStr(hashes) => {
                if c == '"' {
                    let close_ok = (0..hashes).all(|k| chars.get(i + 1 + k as usize) == Some(&'#'));
                    if close_ok {
                        state = State::Normal;
                        i += 1 + hashes as usize;
                        continue;
                    }
                }
                if c == '\u{2014}' && test_mod_depths.is_empty() {
                    violations.push(Violation { file: path.to_path_buf(), line });
                }
                i += 1;
                continue;
            }
            State::Char => {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '\'' {
                    state = State::Normal;
                }
                i += 1;
                continue;
            }
            State::Normal => {}
        }

        // -- State::Normal --

        if c == '/' && chars.get(i + 1) == Some(&'/') {
            state = State::LineComment;
            i += 2;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            state = State::BlockComment(1);
            i += 2;
            continue;
        }
        if c == '"' {
            state = State::Str;
            i += 1;
            continue;
        }
        // Raw string: optional `b`, then `r`, then zero-or-more `#`, then `"`.
        {
            let mut j = i;
            if chars.get(j) == Some(&'b') {
                j += 1;
            }
            if chars.get(j) == Some(&'r') {
                let mut k = j + 1;
                let mut hashes = 0u32;
                while chars.get(k) == Some(&'#') {
                    hashes += 1;
                    k += 1;
                }
                if chars.get(k) == Some(&'"') {
                    state = State::RawStr(hashes);
                    i = k + 1;
                    continue;
                }
            }
        }
        if c == '\'' {
            // Distinguish a char literal from a lifetime/generic tick
            // ('a, 'static, 'de, ...), which is not a literal at all.
            if chars.get(i + 1) == Some(&'\\') {
                let mut k = i + 2;
                let limit = (i + 12).min(chars.len());
                while k < limit && chars.get(k) != Some(&'\'') && chars.get(k) != Some(&'\n') {
                    k += 1;
                }
                if chars.get(k) == Some(&'\'') {
                    state = State::Char;
                    i += 1;
                    continue;
                }
            } else if chars.get(i + 2) == Some(&'\'') {
                // 'x' -- a plain single-character literal.
                state = State::Char;
                i += 1;
                continue;
            }
            // Otherwise: a lifetime. Step past the tick and keep scanning
            // normally; the identifier that follows needs no special care.
            i += 1;
            continue;
        }
        if c == '{' {
            depth += 1;
            if awaiting_test_mod_brace {
                test_mod_depths.push(depth);
                awaiting_test_mod_brace = false;
            }
            i += 1;
            continue;
        }
        if c == '}' {
            if test_mod_depths.last() == Some(&depth) {
                test_mod_depths.pop();
            }
            depth -= 1;
            i += 1;
            continue;
        }
        // Detect the `#[cfg(test)]` attribute, and whether the item it
        // guards is a `mod` (the only case worth skipping the body of).
        if c == '#' && chars[i..].iter().take(12).collect::<String>() == "#[cfg(test)]" {
            pending_test_attr = true;
            i += 12;
            continue;
        }
        if pending_test_attr && !c.is_whitespace() {
            let next3: String = chars[i..].iter().take(3).collect();
            let word_boundary = !chars.get(i + 3).is_some_and(|c| c.is_alphanumeric() || *c == '_');
            if next3 == "mod" && word_boundary {
                awaiting_test_mod_brace = true;
            }
            pending_test_attr = false;
            // fall through: this character still needs ordinary handling.
        }

        i += 1;
    }

    violations
}

#[test]
fn no_em_dash_in_user_facing_string_literals() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let src_dir = Path::new(manifest_dir).join("src");

    let mut files = Vec::new();
    collect_rs_files(&src_dir, &mut files);
    files.sort();
    assert!(!files.is_empty(), "expected to find *.rs files under {src_dir:?}");

    let mut violations = Vec::new();
    for file in &files {
        let src =
            std::fs::read_to_string(file).unwrap_or_else(|e| panic!("failed to read {file:?}: {e}"));
        violations.extend(scan(file, &src));
    }

    if !violations.is_empty() {
        let mut msg = String::from(
            "found U+2014 (em dash) in a string literal outside comments / #[cfg(test)] \
             modules (CLAUDE.md rule 11 -- use a full stop, colon, or the word the dash \
             was hiding):\n",
        );
        for v in &violations {
            msg.push_str(&format!("  {}:{}\n", v.file.display(), v.line));
        }
        panic!("{msg}");
    }
}

/// Exercises `scan` directly against a fixed fixture (rather than the real
/// `src/` tree) so the tokenizer's edge cases stay covered even once the
/// last real em dash is fixed: a `'"'` char literal, a `'static` lifetime,
/// a comment, a plain string, a raw string, and a `#[cfg(test)]` module
/// body all containing (or, for the first three, not containing) U+2014.
#[test]
fn scan_finds_dash_in_string_but_ignores_comments_char_literals_and_test_mods() {
    let src = "\
fn f() {
    let ok_char = '\"';
    let ok_lifetime_use: &'static str = \"fine\";
    // a comment with an em dash \u{2014} is fine
    let bad = \"oops \u{2014} em dash\";
    let raw_bad = r#\"raw em dash \u{2014} here\"#;
}

#[cfg(test)]
mod tests {
    fn t() {
        let ignored = \"test-only em dash \u{2014}\";
    }
}
";
    let violations = scan(Path::new("fixture.rs"), src);
    let lines: Vec<usize> = violations.iter().map(|v| v.line).collect();
    assert_eq!(lines, vec![5, 6], "expected exactly the two real-code violations, got {lines:?}");
}
