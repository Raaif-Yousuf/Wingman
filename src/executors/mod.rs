//! Executors (#31): deterministic Rust that turns a
//! [`crate::ui::confirm::Confirmed`] proposal into a real effect. See
//! `docs/superpowers/specs/2026-09-17-executor-design.md` for the type
//! system this module implements and why.
//!
//! `Executor` is object-safe (stored as `Box<dyn Executor>` in
//! [`registry::resolve`]), so it is not generic over the proposal type: it
//! always takes `Confirmed<serde_json::Value>`, the same JSON currency
//! `actions::schema`'s proposal registry already speaks.

mod calendar_add;
mod clipboard;
mod fill_form;
mod image_clipboard;
mod none;
pub mod registry;
mod replace_text;
mod target;
pub mod uia_guard;

use crate::ui::confirm::Confirmed;

/// Whether an executor only reads state (never writes anything a user did
/// not already have) or writes something. The one thing this gates: only a
/// `ReadOnly` executor may run through
/// [`crate::ui::confirm::auto_confirm_read_only`] without a real user
/// confirmation (expansion plan §6: "Read-only actions show a result card
/// straight away").
///
/// **Decided (issue #402):** `ClipboardExecutor` and `ImageClipboardExecutor`
/// stay `ReadOnly` even though both overwrite the real OS clipboard. This
/// was raised as a possible bug (a `ReadOnly` label letting a clobbering
/// write skip confirmation), but it matches what the 2026-09-16 expansion
/// plan already specifies: the actions table's `confirm` column is "false
/// only for read-only actions", and its own example -- "Extract text
/// (offline)  screen -> clipboard  local" -- is exactly this shape, an
/// action whose entire purpose is landing a computed result on the
/// clipboard with no extra click. Per AGENTS.md's Look-Propose-Confirm-Do
/// loop, the result card the read-only fast path still shows *is* the
/// propose/confirm step for this class of action: the user pressed the key
/// to get something copied, and requiring a second confirmation on top of
/// the card would be the same tap twice that the fast path exists to avoid.
/// A distinct `Effect::Clipboard` variant was considered and rejected: nothing
/// in either spec calls for a third policy bucket, and inventing one here
/// would be undocumented architecture, which AGENTS.md rule 12 reserves for
/// a spec update, not a bugfix.
///
/// This does not excuse silently destroying whatever was on the clipboard
/// before: see `executors::clipboard`'s and `executors::image_clipboard`'s
/// own doc comments for the residual gap (arboard's `ContentNotAvailable`
/// cannot tell "clipboard was empty" from "clipboard held something this
/// executor cannot preserve") and issue #410 tracking it separately.
///
/// Nothing in `app.rs` calls an executor yet (the confirm card's "Do it"
/// button does not exist -- see the 2026-09-17 executor design doc's "Out
/// of scope"), so this and every other item in this module are unused
/// outside their own tests until that issue lands, the same status
/// `provider::Caps` has until Phase 2's router (`#[allow(dead_code)]`).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    ReadOnly,
    Writes,
}

/// What an executor did, and how to undo it. `restore` is a closure, not a
/// serialized snapshot: an executor is in the best position to know exactly
/// what "restore" means for its own effect (expansion plan §6, "Undo where
/// the platform allows"), and this keeps `Undo` usable for effects (like the
/// clipboard) that have no natural serialized representation.
#[allow(dead_code)] // see Effect's doc comment
pub struct Undo {
    pub summary: String,
    restore: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
}

#[allow(dead_code)] // see Effect's doc comment
impl Undo {
    /// For an executor with no prior state to restore (read-only actions:
    /// there is nothing to undo because nothing was written).
    pub fn none(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            restore: Box::new(|| Ok(())),
        }
    }

    /// For an executor that changed something and can put it back.
    pub fn recording(
        summary: impl Into<String>,
        restore: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    ) -> Self {
        Self {
            summary: summary.into(),
            restore: Box::new(restore),
        }
    }

    pub fn undo(self) -> anyhow::Result<()> {
        (self.restore)()
    }
}

/// Deterministic Rust that acts on a confirmed proposal. Never runs without
/// a [`Confirmed`] value (the type system enforces it: there is no other way
/// to obtain one -- see `ui::confirm`), and "preview equals execution": the
/// confirmed value is consumed exactly once, by value, so there is no path
/// to execute a stale or re-derived proposal.
#[allow(dead_code)] // see Effect's doc comment
pub trait Executor: Send + Sync {
    fn name(&self) -> &'static str;
    fn effect(&self) -> Effect;
    fn execute(&self, confirmed: Confirmed<serde_json::Value>) -> anyhow::Result<Undo>;
}
