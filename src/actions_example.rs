//! Keeps the committed `actions.example.toml` in sync with
//! `actions::builtin_actions()`, mirroring `config_example.rs`.
//!
//! A separate module (not folded into `actions/mod.rs`) for the same reason
//! `config_example.rs` is separate from `config.rs`: a small, isolated diff
//! while other agents work elsewhere in the tree.
//!
//! Check: `cargo test actions_example`
//! Regenerate after an intentional built-in action change:
//! `UPDATE_EXAMPLES=1 cargo test actions_example`, then commit the result.

#[cfg(test)]
mod tests {
    use crate::actions::{builtin_actions, ActionsFile};
    use std::path::PathBuf;

    fn example_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("actions.example.toml")
    }

    fn example_file() -> ActionsFile {
        ActionsFile {
            actions: builtin_actions(),
            disabled_groups: Vec::new(),
        }
    }

    #[test]
    fn actions_example_matches_builtin_actions() {
        let expected = toml::to_string_pretty(&example_file())
            .expect("ActionsFile must serialize to TOML");

        let path = example_path();

        if std::env::var("UPDATE_EXAMPLES").is_ok() {
            std::fs::write(&path, &expected)
                .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
        }

        // With core.autocrlf=true (the Git for Windows default, and GitHub's
        // windows-latest runners) a checkout turns the committed LF file
        // into CRLF, which is not drift -- same normalization
        // `config_example.rs` applies.
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| {
                panic!(
                    "failed to read {}: {e}. Run with UPDATE_EXAMPLES=1 to generate it, then commit the result.",
                    path.display()
                )
            })
            .replace("\r\n", "\n");

        assert_eq!(
            committed, expected,
            "actions.example.toml has drifted from actions::builtin_actions(). \
             Regenerate with `UPDATE_EXAMPLES=1 cargo test actions_example` and commit the result."
        );
    }

    #[test]
    fn actions_example_round_trips_through_the_real_parser() {
        // The example must actually be a valid actions.toml, parsed the
        // same way a user's own file is -- not merely valid TOML.
        let committed = std::fs::read_to_string(example_path())
            .expect("actions.example.toml must exist; run UPDATE_EXAMPLES=1 first");
        let parsed = crate::actions::parse_actions_file(&committed)
            .expect("actions.example.toml must parse as a valid actions file");
        assert_eq!(parsed.actions, builtin_actions());
    }
}
