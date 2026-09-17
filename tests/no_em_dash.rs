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

#[derive(Debug)]
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
                    state = if d > 1 {
                        State::BlockComment(d - 1)
                    } else {
                        State::Normal
                    };
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            State::Str => {
                if c == '\\' {
                    // A `\u{XXXX}` Unicode escape is more than the 2 source
                    // characters every other escape (`\n`, `\"`, `\\`, ...)
                    // takes. Blindly skipping 2 chars left the cursor on
                    // `{`, so the remaining hex digits and `}` were then
                    // scanned as ordinary string text -- never matching the
                    // real U+2014 codepoint the escape spells (issue #179).
                    // Decode it explicitly and compare the codepoint instead.
                    if chars.get(i + 1) == Some(&'u') && chars.get(i + 2) == Some(&'{') {
                        let mut k = i + 3;
                        let mut hex = String::new();
                        while chars.get(k).is_some_and(|hc| *hc != '}') {
                            hex.push(chars[k]);
                            k += 1;
                        }
                        if chars.get(k) == Some(&'}') {
                            if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                                if cp == 0x2014 && test_mod_depths.is_empty() {
                                    violations.push(Violation {
                                        file: path.to_path_buf(),
                                        line,
                                    });
                                }
                            }
                            i = k + 1;
                            continue;
                        }
                        // No closing brace found before the string ended or
                        // EOF was reached: not a well-formed unicode escape.
                        // Fall through to the generic skip below rather than
                        // having scanned all the way to EOF for nothing.
                    }
                    i += 2; // skip the escaped character, whatever it is
                    continue;
                }
                if c == '"' {
                    state = State::Normal;
                    i += 1;
                    continue;
                }
                if c == '\u{2014}' && test_mod_depths.is_empty() {
                    violations.push(Violation {
                        file: path.to_path_buf(),
                        line,
                    });
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
                    violations.push(Violation {
                        file: path.to_path_buf(),
                        line,
                    });
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
            let word_boundary = !chars
                .get(i + 3)
                .is_some_and(|c| c.is_alphanumeric() || *c == '_');
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

/// Deliberate, narrow exemptions from the scan above (issue #179): a string
/// literal that legitimately needs to CONTAIN a real em dash in order to
/// MATCH one, rather than DISPLAY one to the user, so it is not what CLAUDE.md
/// rule 11 is actually about even though it is a string literal outside a
/// comment or `#[cfg(test)]` module.
///
/// Matched by a substring of the violating line's own source text, not by
/// file+line number (line numbers drift) -- so an edit that removes the
/// exempted text stops exempting anything on that line, rather than
/// silently exempting whatever code ends up there instead. Adding an entry
/// here is a conscious decision, not a general escape hatch: name the exact
/// function and why.
///
/// `Config::repair_refusal_trigger`'s `TRIGGER` constant (`src/config.rs`,
/// owned by a different agent in this session, hence not edited directly)
/// matches a real, historical em dash already sitting inside users' saved
/// `config.toml` prompts, written before rule 11 existed, so that sentence
/// can be rewritten away; `TRIGGER` itself is never shown to the user. Fixing
/// this scanner's `\u{2014}`-escape blind spot (the rest of this issue) is
/// what makes that constant visible to `scan` at all -- previously it was
/// only invisible by accident of the parser, which is exactly the hole issue
/// #179 was filed about. This is the "mark it deliberately" resolution its
/// "Done when" names as an alternative to rewriting `TRIGGER`.
const DELIBERATE_EM_DASH_MATCH_EXEMPTIONS: &[&str] = &["This is your scratchpad"];

/// Whether `line_text` (the exact source line a violation was found on)
/// matches one of `DELIBERATE_EM_DASH_MATCH_EXEMPTIONS`.
fn is_deliberately_exempt(line_text: &str) -> bool {
    DELIBERATE_EM_DASH_MATCH_EXEMPTIONS
        .iter()
        .any(|needle| line_text.contains(needle))
}

#[test]
fn deliberate_exemption_matches_its_named_line_only() {
    assert!(is_deliberately_exempt(
        "    \"This is your scratchpad \\u{2014} reason it out before committing to a verdict.\""
    ));
    // Narrow, not a blanket match on any mention of a scratchpad or an em
    // dash: an unrelated line must still be caught.
    assert!(!is_deliberately_exempt(
        "let bad = \"oops \\u{2014} scratchpad em dash\";"
    ));
    assert!(!is_deliberately_exempt(
        "let bad = \"an unrelated em dash \\u{2014} here\";"
    ));
}

#[test]
fn no_em_dash_in_user_facing_string_literals() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let src_dir = Path::new(manifest_dir).join("src");

    let mut files = Vec::new();
    collect_rs_files(&src_dir, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "expected to find *.rs files under {src_dir:?}"
    );

    let mut violations = Vec::new();
    for file in &files {
        let src = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("failed to read {file:?}: {e}"));
        for v in scan(file, &src) {
            let line_text = src.lines().nth(v.line.saturating_sub(1)).unwrap_or("");
            if is_deliberately_exempt(line_text) {
                continue;
            }
            violations.push(v);
        }
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
    assert_eq!(
        lines,
        vec![5, 6],
        "expected exactly the two real-code violations, got {lines:?}"
    );
}

/// Issue #179: a normal string literal that spells the em dash as the Rust
/// escape sequence `\u{2014}` (8 source characters: `\`, `u`, `{`, `2`, `0`,
/// `1`, `4`, `}`) rather than pasting the literal U+2014 character. The old
/// `State::Str` escape handling unconditionally skipped 2 characters for
/// every backslash escape, which is right for `\n`/`\"`/`\\` but leaves the
/// cursor sitting on `{` for a unicode escape -- the remaining `2014}` is
/// then scanned as ordinary text, none of which is the real codepoint, so
/// the violation was invisible. This must be caught exactly like a pasted
/// literal em dash.
#[test]
fn scan_detects_em_dash_written_as_a_unicode_escape_in_a_normal_string() {
    let src = "\
fn f() {
    let bad = \"scratchpad \\u{2014} reason it out\";
}
";
    let violations = scan(Path::new("fixture.rs"), src);
    let lines: Vec<usize> = violations.iter().map(|v| v.line).collect();
    assert_eq!(
        lines,
        vec![2],
        "expected the \\u{{2014}} escape to be caught on its line"
    );
}

/// The escape-decoding fix must compare the actual decoded codepoint, not
/// just recognise the `\u{...}` shape -- an unrelated unicode escape (here,
/// U+2013 EN DASH, one codepoint below the em dash) must not false-positive.
#[test]
fn scan_does_not_flag_an_unrelated_unicode_escape() {
    let src = "\
fn f() {
    let ok = \"an en dash \\u{2013} is not an em dash\";
}
";
    let violations = scan(Path::new("fixture.rs"), src);
    assert!(
        violations.is_empty(),
        "a non-em-dash unicode escape must not be flagged, got {violations:?}"
    );
}

/// A raw string has no escapes at all in real Rust syntax, so the literal
/// text `\u{2014}` inside one is 8 ordinary characters, not a codepoint --
/// this must keep behaving exactly like today (no violation), proving the
/// new escape-decoding logic is scoped to `State::Str` only.
#[test]
fn scan_does_not_decode_escapes_inside_raw_strings() {
    let src = "\
fn f() {
    let ok = r\"literal backslash-u-brace text \\u{2014} in a raw string\";
}
";
    let violations = scan(Path::new("fixture.rs"), src);
    assert!(
        violations.is_empty(),
        "a raw string must never decode escapes, got {violations:?}"
    );
}

/// A malformed/unterminated unicode escape (no closing brace before the
/// string itself closes) must not panic or hang the scanner; it degrades to
/// the old best-effort 2-character skip.
#[test]
fn scan_tolerates_an_unterminated_unicode_escape() {
    let src = "\
fn f() {
    let odd = \"broken \\u{2014 no closing brace\";
}
";
    // Must not panic; the exact violation set here isn't the point (the
    // string's own closing quote is inside the "escape", so the parse of
    // this single malformed literal is inherently ambiguous) -- the test
    // asserts only that `scan` returns rather than looping or panicking.
    let _ = scan(Path::new("fixture.rs"), src);
}
