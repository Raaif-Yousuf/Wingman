//! Issue #203: automates the compile-fail check that `ui::confirm::Confirmed`'s
//! field-privacy boundary previously only had a `` ```compile_fail `` doc
//! comment for, which `cargo test` never actually compiled -- `wingman` has
//! no `[lib]` target (see `Cargo.toml`'s `[[bin]]` section), and Cargo only
//! extracts and runs doctests against a library target.
//!
//! `trybuild` normally needs the crate under test to be a dependency the
//! fixture can `use` -- the exact same obstacle an ordinary `tests/*.rs`
//! integration test hits against a bin-only crate (see `tests/no_em_dash.rs`'s
//! own doc comment). This harness avoids that entirely: each fixture in
//! `tests/compile_fail/` never names `wingman` at all. Instead it
//! `include!`s the REAL `src/ui/confirm.rs`, verbatim except for one
//! mechanical transform (see "Why a generated copy" below), as its own
//! `confirm` module, with small hand-written stand-ins for the two
//! `crate::` types that file's non-test code names
//! (`crate::ui::preview::PreviewModel`, `crate::executors::{Effect,
//! Executor, Undo}`). Because the include is content-derived from the real
//! file on every run, any future edit to that file's field or module
//! privacy flows into this test automatically -- there is nothing here to
//! fall out of sync.
//!
//! This also makes the check sensitive to the *specific* regression #203's
//! body names: `value: P` widened to `pub(crate) value: P` so
//! `src/executors/*.rs` could construct one directly. That is an
//! intra-crate concern (private vs. `pub(crate)`, not private vs. fully
//! `pub`), which an external crate depending on `wingman::...` could never
//! observe either way (both are invisible from outside the crate). Building
//! the fixture as its own single crate with a `confirm` module and a sibling
//! `attacker`/`caller` module recreates the exact intra-crate relationship
//! `src/executors/*.rs` has to `src/ui/confirm.rs`, so widening the field to
//! `pub(crate)` -- not just to fully `pub` -- is exactly what turns
//! `confirmed_is_private.rs` from a compile failure into a compile success,
//! which is what this test is watching for.
//!
//! # Why a generated copy, not a direct `include!` of the real file
//!
//! MEASURED 2026-09-18: a direct `include!("../../src/ui/confirm.rs")`
//! inside a `mod confirm { ... }` block fails with `error[E0753]: expected
//! outer doc comment` on every `//!` line at the top of the real file.
//! Inner doc comments (`//!`) are only legal as the literal first tokens of
//! the enclosing item as WRITTEN IN SOURCE; rustc does not extend that
//! allowance through a macro/`include!` expansion, even when the included
//! content genuinely is the first thing in the enclosing `mod` block. This
//! is a rustc parser restriction on `//!`, not a fact about privacy or about
//! this crate.
//!
//! Since a `//!` comment and a `//` comment are semantically identical
//! (comments have zero effect on privacy, types or behavior), this test
//! generates a byte-identical copy of `src/ui/confirm.rs` with every
//! line-initial `//!` rewritten to `//`, on every run, before invoking
//! trybuild -- see [`write_include_safe_copy`]. Nothing else about the file
//! is touched: every field, function, `pub`/`pub(crate)` annotation and
//! `///` item-doc-comment (which has no such position restriction) is
//! copied unchanged, so the actual boundary under test is still the real
//! one. The generated copy lives under `tests/compile_fail/generated/`,
//! which is gitignored (see `.gitignore`'s own comment) and never hand-edited.
//!
//! See `confirmed_is_private.rs` (the violation, expected to fail) and
//! `confirmed_sanctioned_path_compiles.rs` (the sanctioned path, expected to
//! pass) for the two fixtures.

use std::fs;
use std::path::Path;

/// Writes an `include!`-safe copy of `src/ui/confirm.rs` to
/// `tests/compile_fail/generated/confirm_no_inner_doc.rs`, rewriting every
/// line-initial `//!` to `//` (see this file's doc comment for why) and
/// leaving everything else byte-identical. Both fixtures `include!` the
/// generated path, never the original, so this must run before
/// `trybuild::TestCases` compiles them.
fn write_include_safe_copy() {
    let original = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/confirm.rs");
    let source = fs::read_to_string(&original)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", original.display()));

    let rewritten: String = source
        .lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("//!") {
                format!("//{rest}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compile_fail/generated");
    fs::create_dir_all(&out_dir)
        .unwrap_or_else(|e| panic!("could not create {}: {e}", out_dir.display()));
    let out_path = out_dir.join("confirm_no_inner_doc.rs");
    fs::write(&out_path, rewritten)
        .unwrap_or_else(|e| panic!("could not write {}: {e}", out_path.display()));
}

#[test]
fn confirmed_privacy_boundary() {
    write_include_safe_copy();

    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/confirmed_is_private.rs");
    t.pass("tests/compile_fail/confirmed_sanctioned_path_compiles.rs");
}
