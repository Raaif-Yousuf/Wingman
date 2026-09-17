//! Input gathering (expansion plan §4, `inputs/` row): everything that turns
//! screen, window, selection, clipboard or UIA state into typed data the
//! router and `actions/` can use. Per the row's contract, nothing in here
//! knows about providers, and nothing in here writes anything.
//!
//! Today this holds [`uia`] (#27) and [`selection`] (#28); `ocr.rs` is a
//! later row in the same table, not built yet.

pub mod selection;
pub mod uia;
