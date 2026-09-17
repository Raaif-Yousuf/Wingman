//! The confirmation type-state (#31): `Proposal<P>` is what a provider
//! response becomes once parsed; `Confirmed<P>` is the only thing an
//! `Executor` accepts. See
//! `docs/superpowers/specs/2026-09-17-executor-design.md` ("The type-state
//! pair") for why this module, specifically, is the one place in the crate
//! that can construct a `Confirmed<P>`.
//!
//! Rust field privacy is scoped to the defining module and its descendants,
//! not to the crate: `Confirmed`'s `value` field has no `pub`, so writing
//! `Confirmed { value: x }` anywhere outside this module -- including from
//! `executors/`, which is a sibling module, not a descendant -- is
//! `error[E0451]`. `pub(crate)` on the constructor functions below only
//! widens who may *call* them, never who may write the field directly, and
//! calling [`confirm`] still requires a [`ConfirmationToken`], which has the
//! same private-field shape one level up with its own single sanctioned
//! source, [`user_confirmed`].

/// A typed proposal parsed from a provider's completion, before any user has
/// seen it. Construction is unrestricted: nothing unsafe happens by having a
/// proposal, only by acting on one.
// Unused outside tests until the confirm-card issue calls the real pipeline
// (provider response -> Proposal -> user confirm -> Confirmed -> executor)
// -- see the 2026-09-17 executor design doc's "Out of scope".
#[allow(dead_code)]
pub struct Proposal<P> {
    pub value: P,
}

#[allow(dead_code)]
impl<P> Proposal<P> {
    pub fn new(value: P) -> Self {
        Self { value }
    }
}

/// Proof that the user pressed the confirm card's "Do it" button (or Enter).
/// A private-field newtype, not a `bool`: the only way to obtain one is
/// [`user_confirmed`], so nothing outside this module can call [`confirm`]
/// either, even though `confirm` itself is `pub(crate)`. Today nothing calls
/// `user_confirmed()` yet -- the confirm card's "Do it" button does not
/// exist (expansion plan §17 Phase 2, "Card preview state with Do it, Edit,
/// Cancel buttons"); this is the call site that future work wires up.
pub struct ConfirmationToken(());

/// The proposal a real user has confirmed, and the only thing
/// [`crate::executors::Executor::execute`] accepts.
///
/// An executor cannot fabricate one of these: the field is private to this
/// module, and the only constructor is `pub(crate)`, in this module only.
///
/// ```compile_fail
/// // This snippet documents the boundary; it is NOT run by `cargo test`.
/// // wingman is a [[bin]]-only crate (no [lib] target in Cargo.toml), and
/// // Cargo only extracts and runs doc tests against a library target, so
/// // `cargo test --doc` finds nothing to test here regardless of what this
/// // comment says. See the 2026-09-17 executor design doc, "The
/// // compile-fail doc test, and why it cannot run here". The privacy
/// // violation itself was verified with a standalone `rustc` compile
/// // (MEASURED 2026-09-17, same doc), not through this doctest.
/// # struct Confirmed<P> { value: P }
/// # fn from_outside_the_module<P>(p: P) -> Confirmed<P> {
/// Confirmed { value: p } // error[E0451]: field `value` is private
/// # }
/// ```
#[allow(dead_code)]
pub struct Confirmed<P> {
    value: P,
}

#[allow(dead_code)]
impl<P> Confirmed<P> {
    pub fn value(&self) -> &P {
        &self.value
    }

    pub fn into_value(self) -> P {
        self.value
    }
}

/// The one path a real user confirmation takes. `token` can only have come
/// from [`user_confirmed`].
#[allow(dead_code)]
pub(crate) fn confirm<P>(proposal: Proposal<P>, _token: ConfirmationToken) -> Confirmed<P> {
    Confirmed {
        value: proposal.value,
    }
}

/// Stand-in for the future confirm card's "Do it" / Enter handler. Nothing
/// calls this yet (see [`ConfirmationToken`]'s doc comment); it exists now
/// so [`confirm`] has exactly one sanctioned caller shape to compile
/// against.
#[allow(dead_code)]
pub(crate) fn user_confirmed() -> ConfirmationToken {
    ConfirmationToken(())
}

/// The preview card's actual "Do it" / Enter handler (#26): builds the
/// `Confirmed<Value>` from a [`crate::ui::preview::PreviewModel`]'s
/// *currently shown* values -- after any edits, never the original proposal
/// the provider returned. `card.rs`'s `WM_COMMAND`/`WM_KEYDOWN` handler is
/// the one real caller; both take a `ConfirmationToken`, so this is still
/// only reachable via [`user_confirmed`].
///
/// This function is the literal implementation of #26's Done-when ("Enter
/// yields a `Confirmed` equal to what was shown"): `model.to_value()` is
/// read once, here, and becomes the `Confirmed`'s value with nothing else
/// in between -- there is no second read of the model, no re-fetch of the
/// original proposal, so what an executor later receives cannot diverge
/// from what the card had on screen at the moment "Do it" was pressed.
#[allow(dead_code)]
pub(crate) fn confirm_preview(
    model: &crate::ui::preview::PreviewModel,
    token: ConfirmationToken,
) -> Confirmed<serde_json::Value> {
    confirm(Proposal::new(model.to_value()), token)
}

/// The one exception to "only a real user confirmation produces a
/// `Confirmed<P>`": a read-only executor whose action has `confirm = false`
/// never shows a preview (expansion plan §6: "Read-only actions show a
/// result card straight away"). Refuses any executor that does not declare
/// [`crate::executors::Effect::ReadOnly`] -- this function produces a
/// `Confirmed<P>` without a `ConfirmationToken` on purpose, so it is
/// deliberately the only other function in the crate allowed to.
#[allow(dead_code)]
pub(crate) fn auto_confirm_read_only(
    executor: &dyn crate::executors::Executor,
    proposal: Proposal<serde_json::Value>,
) -> anyhow::Result<Confirmed<serde_json::Value>> {
    anyhow::ensure!(
        executor.effect() == crate::executors::Effect::ReadOnly,
        "executor \"{}\" is not read-only and cannot auto-confirm",
        executor.name()
    );
    Ok(Confirmed {
        value: proposal.value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executors::{Effect, Executor, Undo};

    struct ReadOnlyStub;
    impl Executor for ReadOnlyStub {
        fn name(&self) -> &'static str {
            "read-only-stub"
        }
        fn effect(&self) -> Effect {
            Effect::ReadOnly
        }
        fn execute(&self, _confirmed: Confirmed<serde_json::Value>) -> anyhow::Result<Undo> {
            Ok(Undo::none("stub"))
        }
    }

    struct WritingStub;
    impl Executor for WritingStub {
        fn name(&self) -> &'static str {
            "writing-stub"
        }
        fn effect(&self) -> Effect {
            Effect::Writes
        }
        fn execute(&self, _confirmed: Confirmed<serde_json::Value>) -> anyhow::Result<Undo> {
            Ok(Undo::none("stub"))
        }
    }

    // -- type-state: the sanctioned path --------------------------------

    #[test]
    fn confirm_carries_the_proposals_value_through_unchanged() {
        let proposal = Proposal::new(42);
        let token = user_confirmed();
        let confirmed = confirm(proposal, token);
        assert_eq!(*confirmed.value(), 42);
    }

    #[test]
    fn into_value_yields_the_same_value_confirm_was_given() {
        let proposal = Proposal::new("hello".to_string());
        let token = user_confirmed();
        let confirmed = confirm(proposal, token);
        assert_eq!(confirmed.into_value(), "hello");
    }

    // -- confirm_preview (#26): "Enter yields a Confirmed equal to what was
    // shown" ------------------------------------------------------------

    fn calendar_model_with(overrides: &[(&str, &str)]) -> crate::ui::preview::PreviewModel {
        let schema = crate::actions::schema::schema_for("calendar_event", false)
            .expect("calendar_event is registered");
        let value = serde_json::json!({
            "title": "Standup", "start": "09:00", "end": "09:15",
            "location": "Room 2", "notes": "bring laptop"
        });
        let mut model = crate::ui::preview::PreviewModel::from_schema(&schema, &value);
        for (name, v) in overrides {
            model.set_value(name, v.to_string());
        }
        model
    }

    #[test]
    fn confirm_preview_with_no_edits_matches_the_original_proposal() {
        let model = calendar_model_with(&[]);
        let shown = model.to_value();
        let confirmed = confirm_preview(&model, user_confirmed());
        assert_eq!(*confirmed.value(), shown);
    }

    #[test]
    fn confirm_preview_after_an_edit_yields_a_confirmed_equal_to_what_was_shown() {
        // #26's literal Done-when, proven through the real Confirmed<Value>
        // type (see `ui::preview`'s own unit test of the same property at
        // the PreviewModel layer).
        let model = calendar_model_with(&[("start", "10:30")]);
        let shown = model.to_value(); // exactly what the card would be showing
        let confirmed = confirm_preview(&model, user_confirmed());
        assert_eq!(
            *confirmed.value(),
            shown,
            "the confirmed value must equal exactly what was on screen"
        );
        assert_eq!(confirmed.value()["start"], "10:30");
    }

    #[test]
    fn confirm_preview_ignores_a_refused_edit_to_a_non_editable_field() {
        let mut model = calendar_model_with(&[]);
        let changed = model.set_value("location", "Room 9"); // not editable
        assert!(!changed);
        let confirmed = confirm_preview(&model, user_confirmed());
        assert_eq!(confirmed.value()["location"], "Room 2");
    }

    // -- auto_confirm_read_only ------------------------------------------

    #[test]
    fn auto_confirm_read_only_succeeds_for_a_read_only_executor() {
        let proposal = Proposal::new(serde_json::json!({"text": "hi"}));
        let confirmed = auto_confirm_read_only(&ReadOnlyStub, proposal)
            .expect("a read-only executor may auto-confirm");
        assert_eq!(confirmed.value()["text"], "hi");
    }

    #[test]
    fn auto_confirm_read_only_refuses_a_writing_executor() {
        let proposal = Proposal::new(serde_json::json!({"text": "hi"}));
        let err = auto_confirm_read_only(&WritingStub, proposal)
            .err()
            .expect("a writing executor must never auto-confirm");
        assert!(
            err.to_string().contains("writing-stub"),
            "error should name the offending executor: {err}"
        );
        assert!(
            !err.to_string().contains('\u{2014}'),
            "no em dashes in card-facing text (rule 11): {err}"
        );
    }
}
