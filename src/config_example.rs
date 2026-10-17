//! Keeps the committed `config.example.toml` in sync with `Config::default()`.
//!
//! A separate module (not folded into `config.rs`) so this stays a small,
//! isolated diff while another agent is renaming paths inside `config.rs`.
//!
//! Check: `cargo test config_example`
//! Regenerate after an intentional default change:
//! `UPDATE_EXAMPLES=1 cargo test config_example`, then commit the result.
//!
//! `actions.example.toml` is not generated here: it is kept in sync with
//! `actions::builtin_actions()` by `src/actions_example.rs`.

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// #956: with UPDATE_EXAMPLES=1 one test rewrites the example file, so every
    /// test that touches the file holds this lock; otherwise a reader can see it
    /// half written.
    static EXAMPLE_FILE: Mutex<()> = Mutex::new(());

    fn example_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config.example.toml")
    }

    /// The serialiser writes no comments, so notes that belong in the example
    /// are added here, after the section header they describe.
    fn with_notes(generated: &str) -> String {
        const HEADER: &str = "[palette]\n";
        const NOTE: &str = "# Not used yet: the key never runs a clipboard option on the model's\n\
                            # confidence alone (#859). Changing it has no effect today.\n";
        assert!(
            generated.contains(HEADER),
            "the [palette] section moved; update with_notes"
        );
        generated.replacen(HEADER, &format!("{HEADER}{NOTE}"), 1)
    }

    #[test]
    fn config_example_matches_defaults() {
        let _lock = EXAMPLE_FILE.lock().unwrap_or_else(|e| e.into_inner());
        let expected = with_notes(
            &toml::to_string_pretty(&Config::default())
                .expect("Config::default() must serialize to TOML"),
        );

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
        let _lock = EXAMPLE_FILE.lock().unwrap_or_else(|e| e.into_inner());
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
