// Issue #203, negative case: a sibling module fabricating a `Confirmed`
// directly, the exact shape an executor in `src/executors/*.rs` would need
// if it ever bypassed `ui::confirm::confirm`/`auto_confirm_read_only`. See
// `tests/compile_fail.rs`'s doc comment for why this `include!`s the real
// `src/ui/confirm.rs` instead of depending on `wingman` as a crate.
//
// Hand-written stand-ins for the two `crate::` types confirm.rs's non-test
// code names. `confirm.rs` is `include!`d below as this file's OWN crate
// root's `confirm` module, so its `crate::` paths resolve to THIS crate, not
// the real wingman crate -- these stubs are that resolution target, not a
// copy of the real types' behavior (this test only needs them to exist and
// type-check, never to run).
mod ui {
    pub mod preview {
        pub struct PreviewModel;
        impl PreviewModel {
            pub fn to_value(&self) -> serde_json::Value {
                serde_json::Value::Null
            }
        }
    }
}

mod executors {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Effect {
        ReadOnly,
        Writes,
    }
    pub struct Undo;
    pub trait Executor {
        fn name(&self) -> &'static str;
        fn effect(&self) -> Effect;
    }
}

mod confirm {
    // Generated fresh by tests/compile_fail.rs on every run from the real
    // src/ui/confirm.rs (byte-identical except `//!` -> `//`, a syntax-only
    // rewrite -- see that file's doc comment for why a direct include! of
    // the original fails to compile for an unrelated reason).
    include!("generated/confirm_no_inner_doc.rs");
}

// The relationship this test exists to check: `attacker` is a sibling of
// `confirm`, exactly like `src/executors/*.rs` is a sibling of
// `src/ui/confirm.rs` in the real crate -- not a descendant, so ordinary
// Rust field privacy (scoped to the defining module and its descendants)
// must refuse this regardless of `confirm`'s own pub(crate)/pub status.
mod attacker {
    fn fabricate() -> super::confirm::Confirmed<i32> {
        super::confirm::Confirmed { value: 1 } // error[E0451]: field `value` is private
    }
}

fn main() {}
