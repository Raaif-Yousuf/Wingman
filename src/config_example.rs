//! Keeps the committed `config.example.toml` in sync with `Config::default()`.
//!
//! A separate module (not folded into `config.rs`) so this stays a small,
//! isolated diff while another agent is renaming paths inside `config.rs`.
//!
//! Check: `cargo test config_example`
//! Regenerate after an intentional default change:
//! `UPDATE_EXAMPLES=1 cargo test config_example`, then commit the result.
//!
//! `actions.example.toml` is intentionally not generated here: actions (the
//! typed-proposal contribution surface described in the expansion plan)
//! do not exist in the codebase yet, so there is no `Default` to generate an
//! example from. See issue #9's comment for that call.

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use std::path::PathBuf;

    fn example_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config.example.toml")
    }

    #[test]
    fn config_example_matches_defaults() {
        let expected = toml::to_string_pretty(&Config::default())
            .expect("Config::default() must serialize to TOML");

        let path = example_path();

        if std::env::var("UPDATE_EXAMPLES").is_ok() {
            std::fs::write(&path, &expected)
                .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
        }

        // With core.autocrlf=true (the Git for Windows default, and GitHub's
        // windows-latest runners) a checkout turns the committed LF file into
        // CRLF, which is not drift.
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
            "config.example.toml has drifted from Config::default(). \
             Regenerate with `UPDATE_EXAMPLES=1 cargo test config_example` and commit the result."
        );
    }

    #[test]
    fn config_example_never_ships_a_nonempty_api_key() {
        // Config::default()'s api_key fields are empty strings. Guard the
        // committed file against ever carrying a real-looking key, even if
        // someone hand-edits it after a regeneration.
        let committed = std::fs::read_to_string(example_path())
            .expect("config.example.toml must exist; run UPDATE_EXAMPLES=1 first");
        assert!(
            committed.contains("api_key = \"\""),
            "config.example.toml must ship empty api_key placeholders, never a real key"
        );
    }
}
