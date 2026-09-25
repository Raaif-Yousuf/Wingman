//! Input gathering (expansion plan §4, `inputs/` row): everything that turns
//! screen, window, selection, clipboard or UIA state into typed data the
//! router and `actions/` can use. Per the row's contract, nothing in here
//! knows about providers, and nothing in here writes anything.
//!
//! Today this holds [`uia`] (#27) and [`selection`] (#28); `ocr.rs` is a
//! later row in the same table, not built yet.

pub mod selection;
pub mod uia;
pub(crate) mod uia_automation;

/// Serializes every test in this module tree that drives real UIA over COM.
/// MEASURED 2026-09-17 (#208): UIA integration tests in `uia` and
/// `selection` fail intermittently with E_FAIL when run in parallel.
#[cfg(test)]
pub(crate) static UIA_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn lock_uia_test() -> std::sync::MutexGuard<'static, ()> {
    UIA_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
