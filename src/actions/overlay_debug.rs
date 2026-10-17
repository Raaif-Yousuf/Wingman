//! The first real caller of the in-place overlay (#809): a debug-only action
//! that marks the longest word on screen. It has no prompt and a read-only
//! executor, which is what makes an action model-free (`actions::validate`),
//! and `output = "overlay"` with a `detector` sends it down the generic
//! runner's model-free overlay branch. It is listed in debug builds only: a
//! release build never offers it, because it exists to prove the window, the
//! hooks and the matcher work, not to be useful.

#[cfg(any(debug_assertions, test))]
use super::{Action, InputKind, Output, Prefer};

pub const ACTION_ID: &str = "debug-mark-word";

/// The action itself. Built in debug builds and tests only: a release build has
/// no caller for it.
#[cfg(any(debug_assertions, test))]
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Mark the longest word (debug)".to_string(),
        group: Some("Debug".to_string()),
        inputs: vec![InputKind::Screen],
        proposal: "overlay_marks".to_string(),
        executor: "none".to_string(),
        confirm: false,
        prompt: String::new(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        output: Output::Overlay,
        detector: Some(crate::overlay::detect::MARK_WORD.to_string()),
        enabled: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_a_model_free_overlay_action_naming_a_known_detector() {
        let a = builtin_action();
        assert_eq!(a.output, Output::Overlay);
        assert!(a.prompt.is_empty(), "a prompt would make it a model action");
        assert_eq!(
            a.overlay_detector(),
            Some(crate::overlay::detect::MARK_WORD)
        );
        assert!(super::super::validate_action(&a).is_ok());
    }

    #[test]
    fn a_prompt_or_an_unknown_detector_takes_it_off_the_overlay_path() {
        let mut a = builtin_action();
        a.prompt = "do something".to_string();
        assert_eq!(a.overlay_detector(), None);
        let mut b = builtin_action();
        b.detector = Some("no_such_detector".to_string());
        assert_eq!(b.overlay_detector(), None);
        let mut c = builtin_action();
        c.output = Output::Card;
        assert_eq!(c.overlay_detector(), None);
        let mut d = builtin_action();
        d.detector = None;
        assert_eq!(d.overlay_detector(), None);
    }

    #[test]
    fn it_is_loadable_in_debug_builds_only_and_never_in_the_example_list() {
        let loadable = super::super::loadable_builtins()
            .iter()
            .any(|a| a.id == ACTION_ID);
        assert_eq!(loadable, cfg!(debug_assertions));
        // `builtin_actions` is what `actions.example.toml` lists, so it is
        // the same in every build.
        assert!(!super::super::builtin_actions()
            .iter()
            .any(|a| a.id == ACTION_ID));
    }

    #[test]
    fn a_user_entry_cannot_make_a_card_action_an_overlay() {
        // `output` and `detector` are not fields a user entry may carry.
        let toml = "[[actions]]\nid = \"x\"\nname = \"X\"\ninputs = [\"screen\"]\nproposal = \"verdict\"\nexecutor = \"none\"\nconfirm = false\nprompt = \"p\"\noutput = \"overlay\"\n";
        assert!(toml::from_str::<super::super::UserActionsFile>(toml).is_err());
    }
}
