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
/// This is now an automated gate, not just a doc comment (#203):
/// `tests/compile_fail.rs` (via `tests/compile_fail/confirmed_is_private.rs`)
/// `include!`s this very file into a throwaway crate with a sibling
/// `attacker` module -- the same module relationship `src/executors/*.rs`
/// has to this file -- and proves that module still cannot write the
/// `value` field. `cargo test` (or an equivalent gate) now fails if that
/// boundary is ever widened enough for `src/executors/*.rs` to construct one
/// directly, which the illustrative snippet below never could: `wingman` is
/// a `[[bin]]`-only crate (no `[lib]` target in `Cargo.toml`), and Cargo only
/// extracts and runs doctests against a library target, so `cargo test
/// --doc` finds nothing to run here regardless of what this comment says.
///
/// The two-module shape matters: a `struct`/`fn` pair with no `mod`
/// boundary between them sit in the SAME module, where a private field is
/// always visible, so a flattened one-module version of this snippet would
/// compile fine and silently prove nothing (MEASURED 2026-09-18, in the
/// course of building the automated gate above). The illustration below
/// keeps the module split for that reason.
///
/// ```compile_fail
/// // Illustrative only; not run by `cargo test` (see above) -- the real
/// // check is tests/compile_fail.rs.
/// mod confirm {
///     pub struct Confirmed<P> { value: P }
/// }
/// mod executors {
///     fn fabricate<P>(p: P) -> super::confirm::Confirmed<P> {
///         super::confirm::Confirmed { value: p } // error[E0451]: field `value` is private
///     }
/// }
/// # fn main() {}
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

// ---------------------------------------------------------------------
// #105 "Show me what you're sending": the same type-state shape as
// `ConfirmationToken`/`Confirmed<P>` above, applied to a different
// decision -- not "run the executor on this proposal" but "let this
// request actually leave the machine". See `provider::common`'s
// `send_preview_guard`/`with_send_authorized` for where the resulting
// `SendAuthorized` is spent, and that module's doc comment for what still
// has to call `user_confirmed_send` before this is wired end to end.
// ---------------------------------------------------------------------

/// Proof the user pressed "Send" on the egress preview card (#105). A
/// private-field newtype, exactly like [`ConfirmationToken`]: the only way
/// to obtain one is [`user_confirmed_send`].
pub struct SendToken(());

/// The one thing [`crate::provider::common::with_send_authorized`] accepts.
/// Unlike [`Confirmed<P>`], this carries no value -- the request itself
/// never passes through this module on the send path, only proof that a
/// decision happened. Field-private for the same reason `Confirmed`'s is:
/// nothing outside this module can construct one directly.
#[allow(dead_code)]
pub struct SendAuthorized(());

/// Stand-in for the future preview card's "Send" / Enter handler, mirroring
/// [`user_confirmed`]'s doc comment: nothing calls this yet (#105's
/// remaining wiring is `App::ask` showing `ui::preview::RequestPreview` via
/// the existing `Card::show_preview` machinery and calling this only on
/// "Send"), so it exists now purely so [`authorize_send`] has exactly one
/// sanctioned caller shape to compile against.
#[allow(dead_code)]
pub(crate) fn user_confirmed_send() -> SendToken {
    SendToken(())
}

/// The only way to turn a [`SendToken`] into a [`SendAuthorized`]. Since
/// `token` can only have come from [`user_confirmed_send`], and
/// `SendAuthorized`'s field is private to this module, nothing outside this
/// module can ever produce a `SendAuthorized` -- which is exactly what
/// makes `provider::common::send_preview_guard` a structural gate rather
/// than a checked boolean a caller could simply forget to set.
#[allow(dead_code)]
pub(crate) fn authorize_send(_token: SendToken) -> SendAuthorized {
    SendAuthorized(())
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

    // -- #105: SendToken / SendAuthorized ---------------------------------

    #[test]
    fn user_confirmed_send_then_authorize_send_produces_a_send_authorized() {
        // The compile-time property is the real guarantee here (this
        // function's whole body would fail to compile if `SendAuthorized`
        // could be built any other way -- see the module doc comment's
        // "Rust field privacy" note) -- this test just exercises the
        // sanctioned path end to end.
        let token = user_confirmed_send();
        let _authorized: SendAuthorized = authorize_send(token);
    }

    #[test]
    fn with_send_authorized_runs_the_closure_and_returns_its_value() {
        let authorized = authorize_send(user_confirmed_send());
        let result = crate::provider::common::with_send_authorized(authorized, || 42);
        assert_eq!(result, 42);
    }
}
