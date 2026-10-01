// Issue #256, positive case: the sanctioned construction path
// (`user_confirmed_send` -> `authorize_send`) still compiles from a sibling
// module, through the real `src/ui/confirm.rs`'s own `pub(crate)`
// functions -- proving `send_authorized_is_private.rs`'s failure is
// specifically about the direct tuple-struct construction, not about
// nothing in `confirm` being reachable at all. See `tests/compile_fail.rs`'s
// doc comment for why this `include!`s the real source instead of depending
// on `wingman` as a crate.
//
// Same stand-ins as `send_authorized_is_private.rs`; kept duplicated rather
// than shared so each fixture is independently readable as its own crate (a
// trybuild fixture is compiled standalone, so sharing a helper file would
// need its own `include!` anyway).
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

// `caller` is a sibling of `confirm`, same relationship as `attacker` in
// send_authorized_is_private.rs -- but this goes through the sanctioned
// `pub(crate)` functions instead of constructing the tuple struct directly,
// so it must compile.
mod caller {
    pub fn sanctioned_send_authorized() -> super::confirm::SendAuthorized {
        let token = super::confirm::user_confirmed_send();
        super::confirm::authorize_send(token)
    }
}

fn main() {
    let _authorized = caller::sanctioned_send_authorized();
}
