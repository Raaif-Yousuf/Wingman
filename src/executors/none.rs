//! The `"none"` executor (#31): what "Check my work" resolves to today. A
//! read-only action that only shows a card has nothing for an executor to
//! do, so this stays a no-op by construction -- there is no state to record
//! and nothing to undo.

use crate::ui::confirm::Confirmed;

use super::{Effect, Executor, Undo};

// Unused outside tests until the confirm-card issue calls a resolved
// executor for real -- see `executors::mod`'s `Effect` doc comment.
#[allow(dead_code)]
pub struct NoneExecutor;

#[allow(dead_code)]
impl Executor for NoneExecutor {
    fn name(&self) -> &'static str {
        "none"
    }

    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    fn execute(&self, _confirmed: Confirmed<serde_json::Value>) -> anyhow::Result<Undo> {
        Ok(Undo::none("nothing to do"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::confirm::Proposal;

    #[test]
    fn none_executor_is_read_only() {
        assert_eq!(NoneExecutor.effect(), Effect::ReadOnly);
    }

    #[test]
    fn none_executor_always_succeeds_and_undo_is_a_no_op() {
        let confirmed = crate::ui::confirm::auto_confirm_read_only(
            &NoneExecutor,
            Proposal::new(serde_json::json!({})),
        )
        .unwrap();
        let undo = NoneExecutor.execute(confirmed).unwrap();
        assert_eq!(undo.summary, "nothing to do");
        undo.undo().expect("undoing a no-op must also succeed");
    }
}
