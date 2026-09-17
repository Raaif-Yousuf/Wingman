//! Input gathering (expansion plan §4, `inputs/` row): everything that turns
//! screen, window, selection, clipboard or UIA state into typed data the
//! router and `actions/` can use. Per the row's contract, nothing in here
//! knows about providers, and nothing in here writes anything.
//!
//! Today this holds only [`uia`] (#27); `selection.rs` and `ocr.rs` are
//! later rows in the same table, not built yet.

pub mod uia;
