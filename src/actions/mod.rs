//! The `Action` model (#23): built-in actions merged with a user
//! `%APPDATA%\Wingman\actions.toml`, the group-disable rule (#199), and the
//! precedence rules that let the one action Wingman runs today ("Check my
//! work") route through this model with no observable behaviour change.
//! See `docs/superpowers/specs/2026-09-17-action-model-design.md` for the
//! rules this file implements and why.

pub mod schema;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The one built-in action's id, and the id `app.rs`'s worker looks up to
/// run the hotkey's default action. A constant (not a magic string
/// scattered across `mod.rs` and `app.rs`) so the two can never drift.
pub const DEFAULT_ACTION_ID: &str = "check-my-work";

/// What an action gathers before the model is asked. Only `Screen` is ever
/// produced today (`capture::grab_raw`); the rest are inert until their
/// gathering code lands (expansion plan §7 "Inputs"). A closed enum, not a
/// bare `String`, so an action naming an input Wingman doesn't gather yet
/// (a typo, or a forward reference to a later phase) is a parse error
/// instead of a silently-ignored string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Screen,
    Window,
    Region,
    Selection,
    Clipboard,
    Text,
    Uia,
    Context,
}

/// `prefer = { mode = "auto" }` in an action's TOML block. Only `mode`
/// exists today (expansion plan §6's worked examples); kept as its own
/// struct (not folded into `Action`) because the plan already shows it as a
/// nested table, and a second `prefer.*` field (a preferred model, say) is
/// a plausible near-term addition that would otherwise force renaming
/// `Action`'s own fields to disambiguate.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Prefer {
    pub mode: String,
}

impl Default for Prefer {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
        }
    }
}

/// `hotkey = { vk = 0x33, ctrl = true, shift = true }` in an action's TOML
/// block. Same shape as `hotkey::Chord`, not reused directly: `Chord` is
/// the two fixed global bindings (`hotkeys.primary`/`secondary`) with no
/// notion of "this hotkey belongs to this action", and coupling the two
/// would mean a hotkey module change forces an actions module change for
/// unrelated reasons. Inert today -- nothing reads `Action::hotkey` yet, the
/// same "unused until its caller exists" status `Caps` and `Shot::width`
/// have in `provider/mod.rs` -- until per-action hotkeys (expansion plan
/// §17 Phase 2, "per-action hotkeys") exist to bind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ActionHotkey {
    pub vk: u32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
}

/// One `[[actions]]` entry, built-in or user-supplied. `deny_unknown_fields`
/// on this and every nested struct is what turns a typo'd key in a
/// hand-edited `actions.toml` into a clear parse error (#23's Done-when)
/// instead of a silently-ignored field.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub id: String,
    pub name: String,
    /// #199: navigation grouping (Study, Finance, Writing, Work, Code, ...).
    /// `None` for an ungrouped action; never an empty string (there is
    /// nothing an empty-string group name would let `disabled_groups`
    /// mean that `None` doesn't already mean).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub inputs: Vec<InputKind>,
    /// Schema-registry key (`schema::schema_for`'s lookup), e.g.
    /// `"verdict"`. Kept as a plain `String` (not the registry's own enum)
    /// so a new proposal kind can be *named* in `actions.toml` before the
    /// registry implements it -- `schema::schema_for` returning `None` is
    /// then a clear load error, not a parse-time rejection of a
    /// forward-referenced action nobody can run yet anyway.
    pub proposal: String,
    /// Executor id, or `"none"` for a read-only action. Only `"none"` is
    /// implemented -- no executor exists yet (expansion plan §17 Phase 2)
    /// -- so any other value is accepted as data but never resolved to
    /// anything; that resolution is a later issue's job, not this one's.
    pub executor: String,
    pub confirm: bool,
    pub prompt: String,
    #[serde(default)]
    pub prefer: Prefer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<ActionHotkey>,
    /// #197 part 2: ask for and render the 1-10/Ultra difficulty badge for
    /// this action. Only meaningful for the `"verdict"` proposal kind
    /// today. Defaults to `false` -- see the design spec's "Origin
    /// tracking" section for why this ORs with `config.ui.show_difficulty`
    /// rather than replacing it.
    #[serde(default)]
    pub rate_difficulty: bool,
    /// The disabled flag from #23's Done-when. `true` unless the action (or
    /// its `group`, via `disabled_groups`) is explicitly turned off.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// The root of `actions.toml` (built-in or user). `disabled_groups` (#199)
/// lives at this level, not per-action, because it names *groups*, which
/// span actions and are meaningless attached to any single one.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ActionsFile {
    pub actions: Vec<Action>,
    pub disabled_groups: Vec<String>,
}

/// Whether a resolved action's fields came from the built-in default or
/// from a user `actions.toml` override. See the design spec's "Origin
/// tracking" section: this is what lets `app.rs` decide between
/// `config.ui.prompt` (today's only prompt source) and the action's own
/// `prompt` (an explicit user override) without a fragile text comparison
/// against `provider::DEFAULT_PROMPT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Builtin,
    User,
}

/// One action after merge, paired with where its current field values came
/// from.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub action: Action,
    pub origin: Origin,
}

/// The one built-in action Wingman ships: today's physics/statistics check,
/// unchanged in behaviour (expansion plan §6, "Check my work (exists)").
pub fn builtin_actions() -> Vec<Action> {
    vec![Action {
        id: DEFAULT_ACTION_ID.to_string(),
        name: "Check my work".to_string(),
        group: Some("Study".to_string()),
        inputs: vec![InputKind::Screen],
        proposal: "verdict".to_string(),
        executor: "none".to_string(),
        confirm: false,
        prompt: crate::provider::DEFAULT_PROMPT.to_string(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        enabled: true,
    }]
}

/// Replaces each built-in whose `id` a user action matches (wholesale, not
/// per-field -- see the design spec's "Merge rule"), appends any user
/// action with a new `id`, and records which is which via `Origin`. Pure:
/// no filesystem, no filtering by `enabled`/`disabled_groups` (that is
/// [`visible`]'s job) -- kept separate so a future actions-editor UI can
/// merge without also deciding visibility.
pub fn merge_actions(builtins: Vec<Action>, user_actions: Vec<Action>) -> Vec<Resolved> {
    let mut merged: Vec<Resolved> = builtins
        .into_iter()
        .map(|action| Resolved {
            action,
            origin: Origin::Builtin,
        })
        .collect();

    for user_action in user_actions {
        if let Some(existing) = merged.iter_mut().find(|r| r.action.id == user_action.id) {
            existing.action = user_action;
            existing.origin = Origin::User;
        } else {
            merged.push(Resolved {
                action: user_action,
                origin: Origin::User,
            });
        }
    }

    merged
}

/// Filters `merged` down to what should actually run or be offered: an
/// action with `enabled = false`, or whose `group` appears in
/// `disabled_groups`, is dropped. Runs after merge so an override's fields
/// still apply if the group is later re-enabled without touching the
/// action's own `enabled` flag.
pub fn visible(merged: Vec<Resolved>, disabled_groups: &[String]) -> Vec<Resolved> {
    merged
        .into_iter()
        .filter(|r| {
            if !r.action.enabled {
                return false;
            }
            match &r.action.group {
                Some(group) => !disabled_groups.iter().any(|d| d == group),
                None => true,
            }
        })
        .collect()
}

/// Resolves an action's `executor` field (today validated as present but
/// never resolved to anything, per the action-model design doc's "What
/// stays out of scope") to a real [`crate::executors::Executor`]. The one
/// call site connecting the two: see
/// `docs/superpowers/specs/2026-09-17-executor-design.md` ("Registry",
/// "Minimal wiring into `actions/`"). Nothing calls this outside its own
/// test yet -- wiring `app.rs`'s worker to run the resolved executor after
/// a real confirm click is the confirm-card issue's job, not #31's.
#[allow(dead_code)]
pub fn resolve_executor(action: &Action) -> anyhow::Result<Box<dyn crate::executors::Executor>> {
    crate::executors::registry::resolve(&action.executor)
}

/// Finds the default action (today, the only one the hotkey ever runs) in
/// an already-merged, already-visibility-filtered list. `None` means the
/// default action was disabled, disabled via its group, or removed by a
/// user override that changed its `id` -- `app.rs` turns that into an error
/// card (rule 7), never a panic.
pub fn default_action(actions: &[Resolved]) -> Option<&Resolved> {
    actions.iter().find(|r| r.action.id == DEFAULT_ACTION_ID)
}

/// Parses one `actions.toml` document. `deny_unknown_fields` (set on every
/// struct in this module) is what makes a typo'd key surface here as a
/// named `Err` rather than being silently dropped by `toml`'s default
/// "ignore what you don't recognise" behaviour.
pub fn parse_actions_file(toml_str: &str) -> Result<ActionsFile> {
    toml::from_str(toml_str).context("actions.toml did not parse")
}

/// `%APPDATA%\Wingman\actions.toml`, mirroring `Config::path()`.
pub fn path() -> Result<PathBuf> {
    let base = crate::known_folder::roaming_app_data()
        .context("could not determine the platform config directory")?;
    Ok(base.join("Wingman").join("actions.toml"))
}

/// The full pipeline against an arbitrary path: read (if present), parse,
/// merge with the built-ins, and filter to what's visible. A missing file
/// is not an error -- it means "no overrides", exactly like `Config::load`
/// treats a missing `config.toml` as "use the defaults" rather than a
/// failure (unlike `Config`, this never creates the file: there is nothing
/// yet to write that a user would want to see, since no Settings UI edits
/// actions.toml).
///
/// Store-free and path-injectable (rule 9): every test calls this directly
/// against a scratch directory. Only [`load_actions`] touches the real
/// `%APPDATA%`.
pub fn load_actions_from(user_path: &Path) -> Result<Vec<Resolved>> {
    let user_file = if user_path.exists() {
        let contents = fs::read_to_string(user_path)
            .with_context(|| format!("failed to read {}", user_path.display()))?;
        parse_actions_file(&contents)
            .with_context(|| format!("{} is not a valid actions file", user_path.display()))?
    } else {
        ActionsFile::default()
    };

    let merged = merge_actions(builtin_actions(), user_file.actions);
    Ok(visible(merged, &user_file.disabled_groups))
}

/// [`load_actions_from`] against the real `%APPDATA%\Wingman\actions.toml`.
/// The only entry point `app.rs` calls.
pub fn load_actions() -> Result<Vec<Resolved>> {
    load_actions_from(&path()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch_path(tag: &str) -> PathBuf {
        let unique = format!(
            "wingman-actions-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("actions.toml")
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    // -- builtin_actions ---------------------------------------------------

    #[test]
    fn builtin_check_my_work_matches_todays_behaviour() {
        let builtins = builtin_actions();
        assert_eq!(builtins.len(), 1);
        let a = &builtins[0];
        assert_eq!(a.id, DEFAULT_ACTION_ID);
        assert_eq!(a.name, "Check my work");
        assert_eq!(a.group.as_deref(), Some("Study"));
        assert_eq!(a.inputs, vec![InputKind::Screen]);
        assert_eq!(a.proposal, "verdict");
        assert_eq!(a.executor, "none");
        assert!(!a.confirm, "read-only action, no confirm step");
        assert_eq!(a.prompt, crate::provider::DEFAULT_PROMPT);
        assert!(!a.rate_difficulty, "#197 part 2: default off");
        assert!(a.enabled);
    }

    // -- executor resolution (#31) -------------------------------------------

    #[test]
    fn builtin_check_my_work_resolves_to_the_none_executor() {
        let action = &builtin_actions()[0];
        let executor = resolve_executor(action).expect("\"none\" is a built-in executor");
        assert_eq!(executor.name(), "none");
    }

    #[test]
    fn unknown_executor_name_is_a_named_error_not_a_panic() {
        let mut action = builtin_actions()[0].clone();
        action.executor = "does-not-exist".to_string();
        let err = resolve_executor(&action)
            .err()
            .expect("an unknown executor name must be an error");
        assert!(err.to_string().contains("does-not-exist"));
    }

    // -- TOML parse of the CONTRIBUTING example -----------------------------

    #[test]
    fn parses_the_contributing_md_translate_selection_example() {
        // Verbatim from CONTRIBUTING.md's "Add an action in 20 minutes",
        // plus a trailing `enabled` this module requires (CONTRIBUTING's
        // sample predates #23's `enabled` field default).
        let toml_str = r#"
[[actions]]
id       = "translate-selection"
name     = "Translate selection"
inputs   = ["selection", "screen"]
proposal = "text_answer"
executor = "none"
confirm  = false
prompt   = "Translate the selected text."

[actions.prefer]
mode = "auto"
"#;
        let file = parse_actions_file(toml_str).expect("CONTRIBUTING's example must parse");
        assert_eq!(file.actions.len(), 1);
        let a = &file.actions[0];
        assert_eq!(a.id, "translate-selection");
        assert_eq!(a.inputs, vec![InputKind::Selection, InputKind::Screen]);
        assert_eq!(a.proposal, "text_answer");
        assert_eq!(a.prefer.mode, "auto");
        assert!(a.group.is_none());
        assert!(a.enabled, "enabled defaults to true when absent");
        assert!(
            !a.rate_difficulty,
            "rate_difficulty defaults to false when absent"
        );
    }

    #[test]
    fn unknown_input_kind_is_a_parse_error() {
        let toml_str = r#"
[[actions]]
id       = "x"
name     = "X"
inputs   = ["screeen"]
proposal = "text_answer"
executor = "none"
confirm  = false
prompt   = "p"
"#;
        let err = parse_actions_file(toml_str).unwrap_err();
        assert!(err.to_string().contains("did not parse"));
    }

    #[test]
    fn unknown_field_is_rejected_with_a_clear_error_not_a_crash() {
        let toml_str = r#"
[[actions]]
id        = "x"
name      = "X"
inputs    = ["screen"]
proposal  = "text_answer"
executor  = "none"
confirm   = false
prompt    = "p"
not_a_real_field = true
"#;
        let err = parse_actions_file(toml_str).unwrap_err();
        // `toml`'s deny_unknown_fields error names the offending key.
        assert!(
            format!("{err:#}").contains("not_a_real_field"),
            "error should name the unknown field: {err:#}"
        );
    }

    // -- merge / override / disable -----------------------------------------

    #[test]
    fn user_action_overriding_a_builtins_prompt_is_what_runs() {
        // #23's literal Done-when.
        let mut overridden = builtin_actions()[0].clone();
        overridden.prompt = "custom prompt text".to_string();
        let merged = merge_actions(builtin_actions(), vec![overridden]);
        let resolved = default_action(&merged).expect("check-my-work still present");
        assert_eq!(resolved.action.prompt, "custom prompt text");
        assert_eq!(resolved.origin, Origin::User);
    }

    #[test]
    fn merge_is_wholesale_not_per_field() {
        // A user override that only bothers to change `name` still
        // replaces the WHOLE record -- every other field comes from what
        // the user wrote, not a leftover built-in value. Regression guard
        // for the "replace by id, not per-field" rule (design spec).
        let user_action = Action {
            id: DEFAULT_ACTION_ID.to_string(),
            name: "Renamed".to_string(),
            group: None, // deliberately dropped, unlike the builtin's "Study"
            inputs: vec![InputKind::Screen],
            proposal: "verdict".to_string(),
            executor: "none".to_string(),
            confirm: false,
            prompt: "irrelevant".to_string(),
            prefer: Prefer::default(),
            hotkey: None,
            rate_difficulty: false,
            enabled: true,
        };
        let merged = merge_actions(builtin_actions(), vec![user_action]);
        let resolved = default_action(&merged).unwrap();
        assert_eq!(resolved.action.name, "Renamed");
        assert!(
            resolved.action.group.is_none(),
            "the builtin's group must NOT survive a wholesale override"
        );
    }

    #[test]
    fn user_action_with_a_new_id_is_appended_not_merged() {
        let mut new_action = builtin_actions()[0].clone();
        new_action.id = "translate-selection".to_string();
        new_action.name = "Translate selection".to_string();
        let merged = merge_actions(builtin_actions(), vec![new_action]);
        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged[0].action.id, DEFAULT_ACTION_ID,
            "builtin stays first"
        );
        assert_eq!(merged[1].action.id, "translate-selection");
        assert_eq!(merged[1].origin, Origin::User);
    }

    #[test]
    fn unmodified_builtin_has_builtin_origin() {
        let merged = merge_actions(builtin_actions(), vec![]);
        assert_eq!(default_action(&merged).unwrap().origin, Origin::Builtin);
    }

    #[test]
    fn disabled_flag_hides_an_action() {
        let mut disabled = builtin_actions()[0].clone();
        disabled.enabled = false;
        let merged = merge_actions(builtin_actions(), vec![disabled]);
        let shown = visible(merged, &[]);
        assert!(
            default_action(&shown).is_none(),
            "disabled action must not be visible"
        );
    }

    #[test]
    fn enabled_action_stays_visible_with_no_disabled_groups() {
        let merged = merge_actions(builtin_actions(), vec![]);
        let shown = visible(merged, &[]);
        assert!(default_action(&shown).is_some());
    }

    // -- group disable (#199) -----------------------------------------------

    #[test]
    fn disabling_a_group_hides_every_action_in_it() {
        let merged = merge_actions(builtin_actions(), vec![]); // group "Study"
        let shown = visible(merged, &["Study".to_string()]);
        assert!(default_action(&shown).is_none());
    }

    #[test]
    fn disabling_an_unrelated_group_leaves_the_action_visible() {
        let merged = merge_actions(builtin_actions(), vec![]); // group "Study"
        let shown = visible(merged, &["Finance".to_string()]);
        assert!(default_action(&shown).is_some());
    }

    #[test]
    fn ungrouped_action_is_never_hidden_by_a_disabled_group() {
        let mut ungrouped = builtin_actions()[0].clone();
        ungrouped.group = None;
        let merged = merge_actions(vec![ungrouped], vec![]);
        // Disabling every plausible group name must not touch an
        // ungrouped action.
        let shown = visible(merged, &["Study".to_string(), "".to_string()]);
        assert!(default_action(&shown).is_some());
    }

    // -- load_actions_from (full pipeline, path-injectable per rule 9) ------

    #[test]
    fn missing_user_file_yields_just_the_builtins() {
        let path = scratch_path("missing");
        assert!(!path.exists());
        let resolved = load_actions_from(&path).expect("a missing file is not an error");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].action.id, DEFAULT_ACTION_ID);
        assert_eq!(resolved[0].origin, Origin::Builtin);
        assert!(!path.exists(), "load_actions_from never creates the file");
    }

    #[test]
    fn malformed_user_file_is_reported_as_an_error_not_a_crash() {
        let path = scratch_path("malformed");
        write(&path, "this is not [[[ valid toml");
        let err = load_actions_from(&path).unwrap_err();
        assert!(format!("{err:#}").contains("actions.toml"));
        cleanup(&path);
    }

    #[test]
    fn user_file_overriding_the_default_action_is_loaded_end_to_end() {
        let path = scratch_path("override");
        write(
            &path,
            r#"
[[actions]]
id       = "check-my-work"
name     = "Check my work"
group    = "Study"
inputs   = ["screen"]
proposal = "verdict"
executor = "none"
confirm  = false
prompt   = "overridden end to end"
rate_difficulty = true
enabled  = true
"#,
        );
        let resolved = load_actions_from(&path).unwrap();
        let action = default_action(&resolved).unwrap();
        assert_eq!(action.action.prompt, "overridden end to end");
        assert!(action.action.rate_difficulty);
        assert_eq!(action.origin, Origin::User);
        cleanup(&path);
    }

    #[test]
    fn disabled_groups_in_the_user_file_hide_the_default_action() {
        let path = scratch_path("disabled-group");
        write(&path, "disabled_groups = [\"Study\"]\n");
        let resolved = load_actions_from(&path).unwrap();
        assert!(default_action(&resolved).is_none());
        cleanup(&path);
    }

    #[test]
    fn path_lives_under_wingman_not_copilot_ask() {
        // Doesn't touch the filesystem -- just checks the computed path
        // shape, mirroring `Config::path`'s own test intent without
        // reading real %APPDATA% (rule 1/rule 9 in spirit: never assert
        // against a real user file, only the path shape).
        let p = path().expect("SHGetKnownFolderPath should succeed");
        assert!(p.ends_with("Wingman\\actions.toml") || p.ends_with("Wingman/actions.toml"));
    }
}
