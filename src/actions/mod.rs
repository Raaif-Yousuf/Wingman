//! The `Action` model (#23): built-in actions merged with a user
//! `%APPDATA%\Wingman\actions.toml`, the group-disable rule (#199), and the
//! precedence rules that let the one action Wingman runs today ("Check my
//! work") route through this model with no observable behaviour change.
//! See `docs/superpowers/specs/2026-09-17-action-model-design.md` for the
//! rules this file implements and why.

pub mod action_items;
pub mod calendar;
pub mod chart_table;
pub mod code_actions;
pub mod contact;
pub mod convert_times;
pub mod dates;
pub mod define_word;
pub mod describe_image;
pub mod explain_error;
pub mod explain_screen;
pub mod extract_text;
pub mod fill_form;
pub mod generic_answer;
pub mod generic_inputs;
pub mod generic_replace;
pub mod jargon;
pub mod link_summary;
pub mod page_summary;
pub mod read_qr;
pub mod receipt;
pub mod relative_dates;
pub mod review_email;
pub mod rewrite_tone;
pub mod scam_check;
pub mod schema;
pub mod share;
pub mod table;
pub mod time_zones;
pub mod translate;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The one built-in action's id, and the id `app.rs`'s worker looks up to
/// run the hotkey's default action. A constant (not a magic string
/// scattered across `mod.rs` and `app.rs`) so the two can never drift.
pub const DEFAULT_ACTION_ID: &str = "check-my-work";

/// What an action gathers before the model is asked. A generic action's
/// worker gathers `Screen`, `Selection`, `Clipboard`, `Uia`, `PageText` and
/// `LinkedPage` (`actions::generic_inputs`, `actions::link_summary`); `Window` and `Region` are treated as
/// `Screen`, and `Text` and `Context` are accepted but gathered by nothing
/// yet (expansion plan §7 "Inputs"). A closed enum, not a
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
    /// The foreground browser or reader's document text, read through UI
    /// Automation (#104).
    PageText,
    /// The text of the web page behind a link found in the selection, the
    /// clipboard or on screen. Fetched only after the user confirms the
    /// link (#127).
    LinkedPage,
    Context,
}

/// `prefer = { mode = "auto" }` in an action's TOML block. Inert today:
/// nothing reads it (#535), the same status as `Action::hotkey`. Only `mode`
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
    /// Executor id, or `"none"` for a read-only action. Resolved by
    /// [`resolve_executor`] through `executors::registry::resolve`, which
    /// knows `none`, `clipboard`, `calendar_add`, `image_clipboard`,
    /// `replace_text` and `fill_form`; any other name is an error at
    /// dispatch time. A hand-written `actions.toml` entry runs through the
    /// generic path (`App::run_generic_action`), which reads this field.
    /// The built-in "Add event from screen", "Fill this form" and "Review
    /// this email" flows in `app.rs` currently resolve their executor by a
    /// hardcoded name that matches this field instead of reading it back.
    pub executor: String,
    pub confirm: bool,
    pub prompt: String,
    #[serde(default)]
    pub prefer: Prefer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<ActionHotkey>,
    /// #197: ask for and render the 1-10/Ultra difficulty badge for this
    /// action. Only meaningful for the `"verdict"` proposal kind today.
    /// Defaults to `false`. This is the only switch for the rating: the old
    /// global `ui.show_difficulty` setting was retired.
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
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ActionsFile {
    pub actions: Vec<Action>,
    pub disabled_groups: Vec<String>,
}

/// One `[[actions]]` entry of a USER `actions.toml`, as written: every field
/// but `id` is optional so the loader can tell "left out" from "set to the
/// default" (#559). An entry whose `id` is a built-in overrides only the
/// fields it carries, unless `replace = true`; any other entry must carry
/// every required field. `replace` exists only here, never on [`Action`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionOverride {
    id: String,
    name: Option<String>,
    group: Option<String>,
    inputs: Option<Vec<InputKind>>,
    proposal: Option<String>,
    executor: Option<String>,
    confirm: Option<bool>,
    prompt: Option<String>,
    prefer: Option<Prefer>,
    hotkey: Option<ActionHotkey>,
    rate_difficulty: Option<bool>,
    enabled: Option<bool>,
    replace: Option<bool>,
}

/// The root of a user `actions.toml` as the loader reads it.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct UserActionsFile {
    actions: Vec<ActionOverride>,
    disabled_groups: Vec<String>,
}

impl ActionOverride {
    /// A complete action from this entry alone: a new id, or `replace = true`.
    fn into_action(self) -> Result<Action> {
        let id = self.id;
        let need = |what: &str| {
            anyhow::anyhow!(
                "action \"{id}\" is missing the field \"{what}\".                  If this was meant to change a built-in action, check the id spelling"
            )
        };
        Ok(Action {
            name: self.name.ok_or_else(|| need("name"))?,
            group: self.group,
            inputs: self.inputs.ok_or_else(|| need("inputs"))?,
            proposal: self.proposal.ok_or_else(|| need("proposal"))?,
            executor: self.executor.ok_or_else(|| need("executor"))?,
            confirm: self.confirm.ok_or_else(|| need("confirm"))?,
            prompt: self.prompt.ok_or_else(|| need("prompt"))?,
            prefer: self.prefer.unwrap_or_default(),
            hotkey: self.hotkey,
            rate_difficulty: self.rate_difficulty.unwrap_or(false),
            enabled: self.enabled.unwrap_or(true),
            id,
        })
    }

    /// The built-in with only this entry's present fields laid over it. The
    /// origin stays `Builtin` unless a `prompt` was given, so the Settings
    /// prompt keeps applying to an override that never touched it.
    fn apply_to(self, base: &Action) -> (Action, Origin) {
        // Exhaustive on purpose: a new field on `ActionOverride` must fail
        // to compile here until it is merged below.
        let ActionOverride {
            id: _,
            name,
            group,
            inputs,
            proposal,
            executor,
            confirm,
            prompt,
            prefer,
            hotkey,
            rate_difficulty,
            enabled,
            replace: _,
        } = self;
        let origin = if prompt.is_some() {
            Origin::User
        } else {
            Origin::Builtin
        };
        let mut a = base.clone();
        if let Some(v) = name {
            a.name = v;
        }
        if group.is_some() {
            a.group = group;
        }
        if let Some(v) = inputs {
            a.inputs = v;
        }
        if let Some(v) = proposal {
            a.proposal = v;
        }
        if let Some(v) = executor {
            a.executor = v;
        }
        if let Some(v) = confirm {
            a.confirm = v;
        }
        if let Some(v) = prompt {
            a.prompt = v;
        }
        if let Some(v) = prefer {
            a.prefer = v;
        }
        if hotkey.is_some() {
            a.hotkey = hotkey;
        }
        if let Some(v) = rate_difficulty {
            a.rate_difficulty = v;
        }
        if let Some(v) = enabled {
            a.enabled = v;
        }
        (a, origin)
    }

    /// Resolves this entry against the built-ins: field by field over a
    /// built-in with the same id, wholesale for `replace = true` or a new id.
    fn resolve(self, builtins: &[Action]) -> Result<(Action, Origin)> {
        let base = builtins.iter().find(|b| b.id == self.id);
        match base {
            Some(base) if self.replace != Some(true) => Ok(self.apply_to(base)),
            _ => Ok((self.into_action()?, Origin::User)),
        }
    }
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

/// The id of the "Copy text from screen" built-in action (#41). Not a
/// second `DEFAULT_ACTION_ID`: nothing looks this id up today (the hotkey's
/// default action is still, and only, [`DEFAULT_ACTION_ID`]) -- this path is
/// reached from its own tray item (`ui::tray::cmd::EXTRACT_TEXT`), calling
/// `actions::extract_text::capture_screen`/`recognize_and_copy` directly,
/// never `worker`/`load_actions`. See `actions::extract_text`'s module doc
/// for why.
pub const EXTRACT_TEXT_ACTION_ID: &str = "extract-text-to-clipboard";

/// The id of the "Read QR code from screen" built-in action (#313): decode
/// every QR code on the active monitor with no model and no network, copy
/// the text. See `actions::read_qr`.
pub const READ_QR_ACTION_ID: &str = "read-qr-code";

/// The built-in actions Wingman ships.
///
/// - `"check-my-work"`: today's physics/statistics check, unchanged in
///   behaviour (expansion plan §6, "Check my work (exists)").
/// - `"extract-text-to-clipboard"` (#41): OCR the screen, copy the text,
///   offline and model-free. `group: "Work"`, chosen over "Writing" --
///   reading text off whatever is on screen (a dialog, a PDF, a chat
///   window) is a general capture utility, not a composition aid the way
///   "Review this email" or "Rewrite for tone" (both squarely "Writing" in
///   the expansion plan's catalogue) are. `proposal: "ocr_text"` is a
///   deliberately UNREGISTERED schema name (`actions::schema::schema_for`
///   has no `"ocr_text"` arm and never will need one): this action never
///   asks a model for a proposal at all, so there is no JSON Schema to
///   register -- the field exists only because `Action` requires it, the
///   same "present but not resolved" status the design doc already gives
///   an executor's `executor` field before its resolution exists. `prompt`
///   is empty for the same reason: no model call, nothing to prompt.
///   `confirm: false` matches the real behaviour: the `"clipboard"`
///   executor is `Effect::ReadOnly`, so `actions::extract_text::run_pipeline`
///   auto-confirms it, exactly like `"check-my-work"`'s `"none"` executor.
///   The Quick Ask palette (#25) reads this entry to list the action, and
///   picking it dispatches through `DispatchTarget::ExtractText`
///   (`ui/palette_model.rs`) to `App::extract_text`, the same function the
///   tray's `cmd::EXTRACT_TEXT` calls. The execution path itself
///   (`App::extract_text` ->
///   `actions::extract_text::{capture_screen, recognize_and_copy}`) still
///   does not read this `Action` value's `proposal`/`executor` fields back.
/// - `"read-qr-code"` (#313): decode every QR code on the screen locally and
///   copy the text, model-free like `extract-text-to-clipboard` (same
///   deliberately unregistered proposal name, empty prompt, read-only
///   clipboard executor). A link in a code is shown, never opened.
/// - `"summarize-page"` (#67, `page_summary::builtin_action`): proposal
///   `page_summary`, executor `none`. Two summary lines on the card, bullets on
///   expand. Runs through the generic dispatch path.
/// - `"add-to-calendar"` (#39, `calendar::builtin_action`): the first action
///   that runs the full Look/Propose/Confirm/Do loop end to end.
/// - `"review-this-email"` (#38, `review_email::builtin_action`): the
///   second Look/Propose/Confirm/Do action, and the first whose "Do" step
///   writes text into an arbitrary UIA-editable control (`replace_text`)
///   instead of opening a generated file.
/// - `"fill-this-form"` (#40, `fill_form::builtin_action`): proposal
///   `form_fill`, executor `fill_form` (#33, already shipped). See
///   `actions::fill_form`'s own module doc comment for the two-stage
///   (local-then-model) proposal it builds.
/// - `"action-items-checklist"` (#314, `action_items::builtin_action`):
///   proposal `action_items`, executor `clipboard`. Runs through the generic
///   dispatch path (`App::run_generic_action`), like an `actions.toml` entry.
/// - `"explain-this-error"` (#66, `explain_error::builtin_action`): proposal
///   `error_explanation`, executor `none`, so the answer is the card (#441).
/// - `"define-this-word"` (#75, `define_word::builtin_action`): proposal
///   `word_definition`, executor `none`, selection only, so a text-only
///   local model can answer it.
/// - `"translate-selection"` (#68, `translate::builtin_action`): proposal
///   `text_answer`, executor `replace_text`. Confirms on a card, then writes
///   the translation over the selection (`actions::generic_replace`).
/// - `"describe-image-alt-text"` (#113, `describe_image::builtin_action`):
///   proposal `text_answer`, executor `clipboard`. One sentence of alt text for
///   the image on screen, confirmed on a card, then copied.
/// - `"explain-this-screen"` (#131, `explain_screen::builtin_action`): proposal
///   `screen_explanation`, executor `none`. A one-sentence headline on the
///   collapsed card, a short explanation on expand. Works on any screen.
/// - `"summarize-link"` (#127, `link_summary::builtin_action`): proposal
///   `page_summary`, executor `none`. Finds the link in the selection (else on
///   screen), asks the user to confirm the address, then fetches the page and
///   summarizes it.
/// - `"convert-times-to-my-zone"` (#315, `convert_times::builtin_action`):
///   proposal `time_list`, executor `none`. The model reads each time and its
///   zone as written; Rust converts them to the local zone (`time_zones`).
///   Ambiguous zones list every reading.
/// - `"is-this-a-scam"` (#312, `scam_check::builtin_action`): proposal
///   `scam_check`, executor `none`. A calibrated verdict (likely a scam,
///   suspicious, no clear signs) with the signs behind it, for the message or
///   page on screen.
/// - `"explain-this-code"`, `"fix-this-code"`, `"write-tests-for-code"` (#73,
///   `code_actions::builtin_actions`): group Code, the selection only. Explain
///   is a read-only card, Fix confirms then replaces the selection, Tests
///   confirms then copies.
/// - `"rewrite-shorter"`, `"rewrite-friendlier"`, `"rewrite-formal"` (#72,
///   `rewrite_tone::builtin_actions`): proposal `text_answer`, executor
///   `replace_text`. Rewrite the selection in a fixed tone, confirmed on a
///   card, then written over the selection like "Translate selection".
/// - `"resolve-relative-dates"` (#336, `relative_dates::builtin_action`):
///   model-free. Finds phrases like "next Tuesday" or "in 3 days" in the
///   selection (else on screen) and lists the date each works out to
///   (`dates`), from today's local date.
/// - `"contact-to-address-book"` (#71, `contact::builtin_action`): proposal
///   `contact_card`, executor `contact_add`. Reads the signature or contact
///   block on screen into a contact, confirms it on a card, then writes a
///   `.vcf` (vCard 3.0) and opens it so Windows offers to add it.
/// - `"explain-jargon"` (#335, `jargon::builtin_action`): proposal
///   `jargon_terms`, executor `none`. Lists the technical terms and acronyms in
///   the selection (else on screen) with a short plain meaning each, on the
///   card.
pub fn builtin_actions() -> Vec<Action> {
    let mut actions = vec![
        Action {
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
        },
        Action {
            id: EXTRACT_TEXT_ACTION_ID.to_string(),
            name: "Copy text from screen".to_string(),
            group: Some("Work".to_string()),
            inputs: vec![InputKind::Screen],
            proposal: "ocr_text".to_string(),
            executor: "clipboard".to_string(),
            confirm: false,
            prompt: String::new(),
            prefer: Prefer::default(),
            hotkey: None,
            rate_difficulty: false,
            enabled: true,
        },
        Action {
            id: READ_QR_ACTION_ID.to_string(),
            name: "Read QR code from screen".to_string(),
            group: Some("Work".to_string()),
            inputs: vec![InputKind::Screen],
            proposal: "qr_text".to_string(),
            executor: "clipboard".to_string(),
            confirm: false,
            prompt: String::new(),
            prefer: Prefer::default(),
            hotkey: None,
            rate_difficulty: false,
            enabled: true,
        },
        page_summary::builtin_action(),
        calendar::builtin_action(),
        review_email::builtin_action(),
        fill_form::builtin_action(),
        action_items::builtin_action(),
        table::builtin_action(),
        chart_table::builtin_action(),
        receipt::builtin_action(),
        explain_error::builtin_action(),
        define_word::builtin_action(),
        translate::builtin_action(),
        describe_image::builtin_action(),
        explain_screen::builtin_action(),
        link_summary::builtin_action(),
        convert_times::builtin_action(),
        scam_check::builtin_action(),
        relative_dates::builtin_action(),
        jargon::builtin_action(),
        contact::builtin_action(),
    ];
    actions.extend(code_actions::builtin_actions());
    actions.extend(rewrite_tone::builtin_actions());
    actions
}

/// Replaces each built-in whose `id` a user action matches (wholesale -- the
/// field-by-field override is resolved earlier, by the loader, see #559),
/// appends any user action with a new `id`, and records which is which via
/// `Origin`. Pure: no filesystem, no filtering by `enabled`/`disabled_groups`
/// (that is [`visible`]'s job) -- kept separate so a future actions-editor UI
/// can merge without also deciding visibility.
#[cfg(test)]
pub fn merge_actions(builtins: Vec<Action>, user_actions: Vec<Action>) -> Vec<Resolved> {
    merge_with_origins(
        builtins,
        user_actions
            .into_iter()
            .map(|a| (a, Origin::User))
            .collect(),
    )
}

/// [`merge_actions`] where each entry brings its own origin: a field-level
/// override that left `prompt` alone stays `Builtin` (#559).
fn merge_with_origins(builtins: Vec<Action>, entries: Vec<(Action, Origin)>) -> Vec<Resolved> {
    let mut merged: Vec<Resolved> = builtins
        .into_iter()
        .map(|action| Resolved {
            action,
            origin: Origin::Builtin,
        })
        .collect();

    for (action, origin) in entries {
        if let Some(existing) = merged.iter_mut().find(|r| r.action.id == action.id) {
            existing.action = action;
            existing.origin = origin;
        } else {
            merged.push(Resolved { action, origin });
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

/// Resolves an action's `executor` field to a real
/// [`crate::executors::Executor`], per
/// `docs/superpowers/specs/2026-09-17-executor-design.md` ("Registry",
/// "Minimal wiring into `actions/`"). Called from `app.rs`'s
/// `on_calendar_result`, `on_generic_action_result` and
/// `run_confirmed_generic_action` after a real confirm click.
pub fn resolve_executor(action: &Action) -> anyhow::Result<Box<dyn crate::executors::Executor>> {
    crate::executors::registry::resolve(&action.executor)
}

/// #242: parses a generic action's completion text into a `serde_json::
/// Value`, the shape `App::run_generic_action`'s worker needs for any
/// non-built-in action -- unlike `calendar::parse_calendar_proposal`, this
/// has no fixed field list to check (a generic action's proposal shape is
/// whatever its own registered schema says), so the only requirement is
/// "valid JSON, and a JSON object" -- the same minimum every proposal
/// schema in `actions::schema` already guarantees via `"type": "object"`.
pub fn parse_generic_proposal(text: &str) -> anyhow::Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("provider: completion text is not valid JSON: {e}"))?;
    if !value.is_object() {
        anyhow::bail!("provider: completion is not a JSON object");
    }
    Ok(value)
}

#[cfg(test)]
mod generic_proposal_tests {
    use super::parse_generic_proposal;

    #[test]
    fn parse_generic_proposal_parses_a_json_object() {
        let value = parse_generic_proposal(r#"{"headline":"ok","detail":"fine"}"#)
            .expect("a JSON object must parse");
        assert_eq!(value["headline"], "ok");
    }

    #[test]
    fn parse_generic_proposal_rejects_invalid_json() {
        let err = parse_generic_proposal("not json").unwrap_err();
        assert!(err.to_string().contains("JSON"));
    }

    #[test]
    fn parse_generic_proposal_rejects_a_non_object() {
        let err = parse_generic_proposal("[1,2,3]").unwrap_err();
        assert!(err.to_string().contains("object"));
    }
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
#[cfg(test)]
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
    load_actions_report_from(user_path).map(|report| report.actions)
}

/// What a load produced: the actions that can run, and one plain sentence for
/// each action that was left out because its own fields are wrong (#620).
/// `skipped_ids` holds the ids, in the same order, so a run of a skipped id
/// can say why it is missing.
#[derive(Debug, Default)]
pub struct LoadReport {
    pub actions: Vec<Resolved>,
    pub skipped: Vec<String>,
    pub skipped_ids: Vec<String>,
}

impl LoadReport {
    /// Why the action with this id was skipped, if it was.
    pub fn skipped_reason(&self, id: &str) -> Option<&str> {
        let i = self.skipped_ids.iter().position(|s| s == id)?;
        self.skipped.get(i).map(String::as_str)
    }
}

/// [`load_actions_from`] plus the reasons any action was skipped. A file that
/// cannot be read or parsed is still an `Err` for the whole file; a single
/// action with a bad executor, proposal or input list is skipped on its own
/// so one typo never hides the built-ins. User entries are checked BEFORE
/// the merge: a bad override of a built-in is skipped and the built-in stays.
pub fn load_actions_report_from(user_path: &Path) -> Result<LoadReport> {
    let user_file = if user_path.exists() {
        let contents = fs::read_to_string(user_path)
            .with_context(|| format!("failed to read {}", user_path.display()))?;
        toml::from_str::<UserActionsFile>(&contents)
            .context("actions.toml did not parse")
            .with_context(|| format!("{} is not a valid actions file", user_path.display()))?
    } else {
        UserActionsFile::default()
    };

    let builtins = builtin_actions();
    let mut report = LoadReport::default();
    let mut entries = Vec::new();
    let mut seen_ids: Vec<&str> = Vec::new();
    for entry in &user_file.actions {
        if seen_ids.contains(&entry.id.as_str()) {
            anyhow::bail!(
                "{} is not a valid actions file: the id \"{}\" appears more than once",
                user_path.display(),
                entry.id
            );
        }
        seen_ids.push(entry.id.as_str());
    }
    for entry in user_file.actions {
        let (action, origin) = entry
            .resolve(&builtins)
            .with_context(|| format!("{} is not a valid actions file", user_path.display()))?;
        // A hidden action never runs, so it is not checked.
        let hidden = !action.enabled
            || action
                .group
                .as_ref()
                .is_some_and(|g| user_file.disabled_groups.contains(g));
        match validate_action(&action) {
            Err(e) if !hidden => {
                let mut message = format!("{e:#}.");
                if builtins.iter().any(|b| b.id == action.id) {
                    message.push_str(" Your version was skipped and the built-in was kept.");
                } else {
                    message.push_str(" It was skipped.");
                }
                report.skipped_ids.push(action.id.clone());
                report.skipped.push(message);
            }
            _ => entries.push((action, origin)),
        }
    }

    let merged = merge_with_origins(builtins, entries);
    report.actions = visible(merged, &user_file.disabled_groups);
    Ok(report)
}

/// Rejects a combination that could only fail (or quietly do something else)
/// after the user had spent a model call (#536, #620). A named error, never
/// a panic (rule 7).
fn validate_action(action: &Action) -> Result<()> {
    let executor = match crate::executors::registry::resolve(&action.executor) {
        Ok(executor) => executor,
        Err(_) => anyhow::bail!(
            "action \"{}\" names executor \"{}\", which does not exist",
            action.id,
            action.executor
        ),
    };
    // No model call (an empty prompt) and nothing written (a read-only
    // executor): the proposal name (`"ocr_text"`) is deliberately not a
    // registered schema. Anything else needs a real schema.
    let no_model =
        action.prompt.trim().is_empty() && executor.effect() == crate::executors::Effect::ReadOnly;
    if !no_model && schema::schema_for(&action.proposal, action.rate_difficulty).is_none() {
        anyhow::bail!(
            "action \"{}\" names proposal \"{}\", which does not exist",
            action.id,
            action.proposal
        );
    }
    if action.executor == "replace_text" && !action.inputs.contains(&InputKind::Selection) {
        anyhow::bail!(
            "action \"{}\" uses executor \"replace_text\", {}",
            action.id,
            "which writes over the selected text, so its inputs must include \"selection\""
        );
    }
    // `confirm = false` on a writing executor is refused by
    // `auto_confirm_read_only` only after the model has answered (#732).
    if !action.confirm && executor.effect() == crate::executors::Effect::Writes {
        anyhow::bail!(
            "action \"{}\" uses executor \"{}\", which writes, so it needs confirm = true",
            action.id,
            action.executor
        );
    }
    Ok(())
}

/// Remembers which skipped-actions notice the user has already been shown, so
/// the same problem is said once and again only when the set changes (#620).
#[derive(Debug, Default)]
pub struct SkipNotice {
    notified: Option<String>,
    pending: Option<String>,
}

impl SkipNotice {
    /// The notice for this load if it differs from the last one shown. An
    /// empty list (or a failed load, pass `&[]`) forgets the last notice so a
    /// later break is reported again.
    pub fn observe(&mut self, skipped: &[String]) -> Option<String> {
        let text = skipped.join(" ");
        if text.is_empty() {
            self.notified = None;
            return None;
        }
        if self.notified.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.notified = Some(text.clone());
        Some(text)
    }

    /// [`Self::observe`], then holds a new notice for the next result card.
    pub fn queue(&mut self, skipped: &[String]) {
        if let Some(text) = self.observe(skipped) {
            self.pending = Some(text);
        }
    }

    pub fn take(&mut self) -> Option<String> {
        self.pending.take()
    }
}

static SKIP_NOTICE: std::sync::Mutex<SkipNotice> = std::sync::Mutex::new(SkipNotice {
    notified: None,
    pending: None,
});

fn skip_notice() -> std::sync::MutexGuard<'static, SkipNotice> {
    SKIP_NOTICE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The notice to show now (the palette's own card), once per change.
pub fn observe_skipped(skipped: &[String]) -> Option<String> {
    skip_notice().observe(skipped)
}

/// Holds the notice for the next result card (`app.rs`'s
/// `with_pending_notices` takes it).
pub fn queue_skipped(skipped: &[String]) {
    skip_notice().queue(skipped);
}

/// Takes the queued notice, if any.
pub fn take_skipped_notice() -> Option<String> {
    skip_notice().take()
}

/// Loads the report and queues its notice; a failed load forgets the last
/// notice (the run itself reports the load error).
pub fn queue_skipped_from_disk() {
    match load_actions_report() {
        Ok(report) => queue_skipped(&report.skipped),
        Err(_) => queue_skipped(&[]),
    }
}

/// [`load_actions_from`] against the real `%APPDATA%\Wingman\actions.toml`.
/// The only entry point `app.rs` calls.
pub fn load_actions() -> Result<Vec<Resolved>> {
    load_actions_from(&path()?)
}

/// [`load_actions_report_from`] against the real `actions.toml`. The palette
/// uses it so it can tell the user about a skipped action.
pub fn load_actions_report() -> Result<LoadReport> {
    load_actions_report_from(&path()?)
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
        for id in [
            "check-my-work",
            "summarize-page",
            "read-qr-code",
            "summarize-page",
            "add-to-calendar",
            "review-this-email",
            "fill-this-form",
            "action-items-checklist",
        ] {
            assert!(
                builtins.iter().any(|a| a.id == id),
                "built-in {id} is missing"
            );
        }
        // Found by id, not by position, so this stays valid regardless of
        // what order the built-ins are in.
        let a = builtins
            .iter()
            .find(|a| a.id == DEFAULT_ACTION_ID)
            .expect("Check my work must be a built-in action");
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

    // -- builtin extract-text-to-clipboard (#41) ----------------------------

    #[test]
    fn builtin_extract_text_matches_the_design() {
        let builtins = builtin_actions();
        let a = builtins
            .iter()
            .find(|a| a.id == EXTRACT_TEXT_ACTION_ID)
            .expect("extract-text-to-clipboard is built in");
        assert_eq!(a.name, "Copy text from screen");
        assert_eq!(a.group.as_deref(), Some("Work"));
        assert_eq!(a.inputs, vec![InputKind::Screen]);
        assert_eq!(a.executor, "clipboard");
        assert!(!a.confirm, "read-only action, no confirm step");
        assert!(!a.rate_difficulty);
        assert!(a.enabled);
        // Deliberately unregistered -- see builtin_actions()'s doc comment.
        assert_eq!(schema::schema_for(&a.proposal, a.rate_difficulty), None);
    }

    #[test]
    fn extract_text_action_stays_visible_by_default() {
        let resolved = load_actions_from(&scratch_path("extract-text-default"));
        let resolved = resolved.expect("a missing user file is not an error");
        assert!(
            resolved
                .iter()
                .any(|r| r.action.id == EXTRACT_TEXT_ACTION_ID),
            "extract-text-to-clipboard must be visible with no actions.toml"
        );
    }

    #[test]
    fn builtin_read_qr_matches_the_design() {
        let builtins = builtin_actions();
        let a = builtins
            .iter()
            .find(|a| a.id == READ_QR_ACTION_ID)
            .expect("read-qr-code is built in");
        assert_eq!(a.name, "Read QR code from screen");
        assert_eq!(a.group.as_deref(), Some("Work"));
        assert_eq!(a.inputs, vec![InputKind::Screen]);
        assert_eq!(a.executor, "clipboard");
        assert!(!a.confirm && a.prompt.is_empty());
        assert_eq!(schema::schema_for(&a.proposal, a.rate_difficulty), None);
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
id       = "translate-to-clipboard"
name     = "Translate to clipboard"
inputs   = ["selection", "screen"]
proposal = "text_answer"
executor = "clipboard"
confirm  = false
prompt   = "Translate the selected text."

[actions.prefer]
mode = "auto"
"#;
        let file = parse_actions_file(toml_str).expect("CONTRIBUTING's example must parse");
        assert_eq!(file.actions.len(), 1);
        let a = &file.actions[0];
        assert_eq!(a.id, "translate-to-clipboard");
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

    /// #242's own "Done when": a non-built-in `actions.toml` action -- here
    /// CONTRIBUTING.md's own worked example, loaded from a scratch file
    /// exactly the way a real user's override would be -- is no longer a
    /// dead end. This exercises every non-Win32, non-network piece of the
    /// pipeline `App::run_generic_action`/`on_generic_action_result` use:
    /// the palette still lists it, `dispatch_target_for` routes it
    /// (instead of `None`), its proposal schema resolves, and its executor
    /// resolves and actually runs against a synthetic confirmed value
    /// shaped like that schema.
    #[test]
    fn contributing_md_translate_selection_example_dispatches_and_runs_end_to_end() {
        let path = scratch_path("translate-to-clipboard-e2e");
        write(
            &path,
            r#"
[[actions]]
id       = "translate-to-clipboard"
name     = "Translate to clipboard"
inputs   = ["selection", "screen"]
proposal = "text_answer"
executor = "clipboard"
confirm  = false
prompt   = "Translate the selected text."

[actions.prefer]
mode = "auto"
"#,
        );
        let resolved = load_actions_from(&path).unwrap();
        let action = resolved
            .iter()
            .find(|r| r.action.id == "translate-to-clipboard")
            .map(|r| r.action.clone())
            .expect("the new action must be present after merge");

        // The palette still shows it, and Enter on it no longer dispatches
        // to nothing (the bug this issue reports).
        let catalogue = crate::ui::palette_model::catalogue(&resolved);
        assert!(
            catalogue
                .iter()
                .any(|row| row.id == "translate-to-clipboard"),
            "a non-built-in action must still appear in the palette"
        );
        assert_eq!(
            crate::ui::palette_model::dispatch_target_for("translate-to-clipboard"),
            Some(crate::ui::palette_model::DispatchTarget::Generic(
                "translate-to-clipboard".to_string()
            )),
            "a non-built-in action id must dispatch generically, not to nothing"
        );

        // Its proposal schema resolves (the model-call half of the generic
        // path App::run_generic_action builds a request from).
        let schema = schema::schema_for(&action.proposal, action.rate_difficulty)
            .expect("\"text_answer\" must be a registered proposal schema (#242)");
        assert_eq!(schema["required"], serde_json::json!(["text"]));

        // Its executor resolves and actually runs a synthetic confirmed
        // value shaped like that schema, exactly what
        // App::run_confirmed_generic_action does after a real model call
        // and (for confirm = false) auto-confirm.
        let executor = resolve_executor(&action).expect("\"clipboard\" must resolve");
        assert_eq!(executor.effect(), crate::executors::Effect::ReadOnly);
        let proposal = crate::ui::confirm::Proposal::new(serde_json::json!({"text": "bonjour"}));
        let confirmed = crate::ui::confirm::auto_confirm_read_only(executor.as_ref(), proposal)
            .expect("a read-only executor with confirm = false must auto-confirm");
        let undo = executor
            .execute(confirmed)
            .expect("the clipboard executor must run against a text_answer-shaped value");
        assert!(
            undo.summary.contains("bonjour"),
            "the executor must actually have acted on the model's value: {}",
            undo.summary
        );

        cleanup(&path);
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
        new_action.id = "translate-to-clipboard".to_string();
        new_action.name = "Translate to clipboard".to_string();
        let builtin_count = builtin_actions().len();
        let merged = merge_actions(builtin_actions(), vec![new_action]);
        assert_eq!(merged.len(), builtin_count + 1);
        assert_eq!(
            merged[0].action.id, DEFAULT_ACTION_ID,
            "builtin stays first"
        );
        let appended = merged
            .iter()
            .find(|r| r.action.id == "translate-to-clipboard")
            .expect("the new user action must be appended, not merged into a builtin");
        assert_eq!(appended.origin, Origin::User);
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
        assert_eq!(resolved.len(), builtin_actions().len());
        let default = resolved
            .iter()
            .find(|r| r.action.id == DEFAULT_ACTION_ID)
            .expect("check-my-work is present");
        assert_eq!(default.origin, Origin::Builtin);
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

    // -- replace_text needs a selection input (#536) -------------------------

    const REPLACE_NO_SELECTION: &str = r#"
[[actions]]
id = "shout"
name = "Shout"
inputs = ["clipboard"]
proposal = "text_answer"
executor = "replace_text"
confirm = true
prompt = "Make it loud."
"#;

    #[test]
    fn replace_text_without_a_selection_input_is_skipped_with_a_named_reason() {
        let path = scratch_path("replace-no-selection");
        write(&path, REPLACE_NO_SELECTION);
        let report = load_actions_report_from(&path).unwrap();
        cleanup(&path);
        assert!(!report.actions.iter().any(|r| r.action.id == "shout"));
        assert_eq!(report.skipped.len(), 1);
        let msg = &report.skipped[0];
        assert!(msg.contains("shout"), "names the action: {msg}");
        assert!(msg.contains("replace_text"), "names the executor: {msg}");
        assert!(msg.contains("selection"), "names the fix: {msg}");
        assert!(!msg.contains('\u{2014}'), "no em dash: {msg}");
    }

    // -- confirm = false on a writing executor (#732) -------------------------

    #[test]
    fn confirm_false_on_a_writing_executor_is_skipped_at_load_naming_the_field() {
        for executor in ["replace_text", "calendar_add", "fill_form"] {
            let path = scratch_path("confirm-false-writes");
            write(
                &path,
                &one_action("shout", "text_answer", executor, "Make it loud."),
            );
            let report = load_actions_report_from(&path).unwrap();
            cleanup(&path);
            assert!(
                !report.actions.iter().any(|r| r.action.id == "shout"),
                "{executor}"
            );
            assert_eq!(report.skipped.len(), 1, "{executor}");
            let msg = &report.skipped[0];
            assert!(msg.contains("shout"), "names the action: {msg}");
            assert!(msg.contains("confirm"), "names the field: {msg}");
            assert!(msg.contains(executor), "names the executor: {msg}");
            assert!(!msg.contains('\u{2014}'), "no em dash: {msg}");
        }
    }

    #[test]
    fn confirm_true_on_a_writing_executor_and_confirm_false_on_a_read_only_one_load() {
        let path = scratch_path("confirm-ok");
        let text = format!(
            "{}{}",
            one_action("w", "text_answer", "replace_text", "Go.")
                .replace("confirm = false", "confirm = true"),
            one_action("r", "text_answer", "clipboard", "Go.")
        );
        write(&path, &text);
        let report = load_actions_report_from(&path).unwrap();
        cleanup(&path);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(report.actions.iter().any(|r| r.action.id == "w"));
        assert!(report.actions.iter().any(|r| r.action.id == "r"));
    }

    // -- unknown executor / proposal and skip-not-fail (#620) -----------------

    fn one_action(id: &str, proposal: &str, executor: &str, prompt: &str) -> String {
        format!(
            "[[actions]]\nid = \"{id}\"\nname = \"{id}\"\ninputs = [\"selection\"]\n\
             proposal = \"{proposal}\"\nexecutor = \"{executor}\"\nconfirm = false\n\
             prompt = \"{prompt}\"\n"
        )
    }

    #[test]
    fn an_unknown_executor_is_skipped_and_named() {
        let path = scratch_path("unknown-executor");
        write(
            &path,
            &one_action("typo", "text_answer", "clipbaord", "Do it."),
        );
        let report = load_actions_report_from(&path).unwrap();
        cleanup(&path);
        assert!(!report.actions.iter().any(|r| r.action.id == "typo"));
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].contains("typo"));
        assert!(report.skipped[0].contains("clipbaord"));
    }

    #[test]
    fn an_unknown_proposal_is_skipped_and_named() {
        let path = scratch_path("unknown-proposal");
        write(
            &path,
            &one_action("odd", "no_such_schema", "none", "Answer."),
        );
        let report = load_actions_report_from(&path).unwrap();
        cleanup(&path);
        assert!(!report.actions.iter().any(|r| r.action.id == "odd"));
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].contains("odd"));
        assert!(report.skipped[0].contains("no_such_schema"));
    }

    #[test]
    fn an_unregistered_proposal_is_fine_when_the_action_never_calls_a_model() {
        let path = scratch_path("no-model");
        write(&path, &one_action("ocr-copy", "ocr_text", "clipboard", ""));
        let report = load_actions_report_from(&path).unwrap();
        cleanup(&path);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(report.actions.iter().any(|r| r.action.id == "ocr-copy"));
    }

    #[test]
    fn a_bad_action_does_not_hide_the_good_ones_or_the_builtins() {
        let path = scratch_path("mixed");
        let text = format!(
            "{}\n{}",
            one_action("good", "text_answer", "clipboard", "Fine."),
            one_action("bad", "text_answer", "nope", "Broken.")
        );
        write(&path, &text);
        let report = load_actions_report_from(&path).unwrap();
        let plain = load_actions_from(&path).unwrap();
        cleanup(&path);
        assert!(report.actions.iter().any(|r| r.action.id == "good"));
        assert!(!report.actions.iter().any(|r| r.action.id == "bad"));
        assert!(report
            .actions
            .iter()
            .any(|r| r.action.id == DEFAULT_ACTION_ID));
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(plain.len(), report.actions.len());
    }

    #[test]
    fn a_bad_override_of_a_builtin_is_skipped_and_the_builtin_kept() {
        let path = scratch_path("bad-override");
        write(
            &path,
            &one_action(DEFAULT_ACTION_ID, "verdict", "nope", "Mine."),
        );
        let report = load_actions_report_from(&path).unwrap();
        cleanup(&path);
        let kept = default_action(&report.actions).expect("the built-in default stays");
        assert_eq!(kept.origin, Origin::Builtin);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].contains("built-in was kept"));
        assert!(report.skipped_reason(DEFAULT_ACTION_ID).is_some());
        assert!(report.skipped_reason("something-else").is_none());
    }

    // -- field-level override of a built-in (#559) --------------------------

    fn load_one(tag: &str, toml_text: &str) -> LoadReport {
        let path = scratch_path(tag);
        write(&path, toml_text);
        let report = load_actions_report_from(&path).unwrap();
        cleanup(&path);
        report
    }

    fn load_err(tag: &str, toml_text: &str) -> String {
        let path = scratch_path(tag);
        write(&path, toml_text);
        let err = load_actions_report_from(&path).unwrap_err();
        cleanup(&path);
        format!("{err:#}")
    }

    #[test]
    fn flipping_one_option_keeps_the_builtin_prompt() {
        let report = load_one(
            "flip-one",
            &format!("[[actions]]\nid = \"{DEFAULT_ACTION_ID}\"\nrate_difficulty = true\n"),
        );
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let r = default_action(&report.actions).unwrap();
        let builtin = &builtin_actions()[0];
        assert!(r.action.rate_difficulty);
        assert_eq!(r.action.prompt, builtin.prompt);
        assert_eq!(r.action.name, builtin.name);
        assert_eq!(r.action.group, builtin.group);
        assert_eq!(r.action.inputs, builtin.inputs);
        assert_eq!(
            r.origin,
            Origin::Builtin,
            "config.ui.prompt must still apply"
        );
    }

    #[test]
    fn an_explicit_prompt_override_still_replaces_the_builtin_prompt() {
        let report = load_one(
            "explicit-prompt",
            &format!("[[actions]]\nid = \"{DEFAULT_ACTION_ID}\"\nprompt = \"Mine.\"\n"),
        );
        let r = default_action(&report.actions).unwrap();
        assert_eq!(r.action.prompt, "Mine.");
        assert_eq!(r.action.name, builtin_actions()[0].name);
        assert_eq!(r.origin, Origin::User);
    }

    #[test]
    fn an_invalid_merged_result_keeps_the_builtin_with_a_notice() {
        let report = load_one(
            "bad-merge",
            &format!("[[actions]]\nid = \"{DEFAULT_ACTION_ID}\"\nexecutor = \"nope\"\n"),
        );
        let r = default_action(&report.actions).unwrap();
        assert_eq!(r.action, builtin_actions()[0]);
        assert_eq!(r.origin, Origin::Builtin);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].contains("built-in was kept"));
    }

    #[test]
    fn a_new_id_still_needs_every_required_field() {
        let err = load_err(
            "new-id-partial",
            "[[actions]]\nid = \"brand-new\"\nname = \"New\"\n",
        );
        assert!(err.contains("inputs"), "{err}");
    }

    #[test]
    fn replace_true_restores_wholesale_replacement() {
        let text = format!(
            "{}replace = true\n",
            one_action(DEFAULT_ACTION_ID, "verdict", "none", "Mine.")
        );
        let report = load_one("replace", &text);
        let r = default_action(&report.actions).unwrap();
        assert_eq!(r.action.prompt, "Mine.");
        assert!(
            r.action.group.is_none(),
            "replace must drop the built-in group"
        );
        assert_eq!(r.origin, Origin::User);
    }

    #[test]
    fn replace_true_still_needs_every_required_field() {
        let err = load_err(
            "replace-partial",
            &format!("[[actions]]\nid = \"{DEFAULT_ACTION_ID}\"\nreplace = true\n"),
        );
        assert!(err.contains("name"), "{err}");
    }

    #[test]
    fn a_typo_in_a_partial_override_is_still_a_named_error() {
        let err = load_err(
            "partial-typo",
            &format!("[[actions]]\nid = \"{DEFAULT_ACTION_ID}\"\nrate_dificulty = true\n"),
        );
        assert!(err.contains("rate_dificulty"), "{err}");
    }

    #[test]
    fn a_duplicate_id_in_one_file_is_a_named_error() {
        let text = format!(
            "[[actions]]
id = \"{DEFAULT_ACTION_ID}\"
enabled = true

             [[actions]]
id = \"{DEFAULT_ACTION_ID}\"
confirm = false
"
        );
        let err = load_err("dup-id", &text);
        assert!(err.contains(DEFAULT_ACTION_ID) && err.contains("more than once"));
    }

    #[test]
    fn a_missing_field_error_hints_at_the_id_spelling() {
        let err = load_err(
            "hint",
            "[[actions]]
id = \"check-my-wrok\"
name = \"X\"
",
        );
        assert!(err.contains("check the id spelling"), "{err}");
    }

    #[test]
    fn every_builtin_survives_being_written_out_and_applied_as_an_override() {
        // Writes each built-in as a full override entry, parses it with the
        // strict user-file parser and applies it onto itself: a field the
        // override merge forgets would come back different.
        let builtins = builtin_actions();
        assert!(!builtins.is_empty());
        for builtin in &builtins {
            let file = ActionsFile {
                actions: vec![builtin.clone()],
                disabled_groups: Vec::new(),
            };
            let text = toml::to_string(&file).unwrap();
            let user: UserActionsFile = toml::from_str(&text).unwrap();
            let entry = user.actions.into_iter().next().unwrap();
            let (applied, _) = entry.resolve(&builtins).unwrap();
            assert_eq!(&applied, builtin, "{}", builtin.id);
        }
    }

    #[test]
    fn an_unregistered_proposal_with_a_writing_executor_is_skipped() {
        let path = scratch_path("empty-prompt-writes");
        write(
            &path,
            &one_action("quiet", "no_such_schema", "replace_text", ""),
        );
        let report = load_actions_report_from(&path).unwrap();
        cleanup(&path);
        assert!(!report.actions.iter().any(|r| r.action.id == "quiet"));
        assert_eq!(report.skipped.len(), 1);
    }

    #[test]
    fn skip_messages_have_no_double_spaces() {
        for (executor, proposal, prompt, inputs) in [
            ("clipbaord", "text_answer", "x", "selection"),
            ("none", "no_such_schema", "x", "selection"),
            ("replace_text", "text_answer", "x", "clipboard"),
        ] {
            let path = scratch_path("spaces");
            let text = one_action("m", proposal, executor, prompt)
                .replace("\"selection\"", &format!("\"{inputs}\""));
            write(&path, &text);
            let report = load_actions_report_from(&path).unwrap();
            cleanup(&path);
            assert_eq!(report.skipped.len(), 1, "{executor}");
            assert!(!report.skipped[0].contains("  "), "{}", report.skipped[0]);
        }
    }

    #[test]
    fn a_skip_notice_is_taken_once_until_the_skipped_set_changes() {
        let mut n = SkipNotice::default();
        let a = vec!["action \"a\" is bad.".to_string()];
        n.queue(&a);
        assert_eq!(n.take().as_deref(), Some("action \"a\" is bad."));
        assert_eq!(n.take(), None, "taken once");
        n.queue(&a);
        assert_eq!(n.take(), None, "same problem is not repeated");
        let b = vec![a[0].clone(), "action \"b\" is bad.".to_string()];
        n.queue(&b);
        assert!(n.take().unwrap().contains("\"b\""), "a changed set is new");
        n.queue(&[]);
        n.queue(&b);
        assert!(
            n.take().is_some(),
            "fixed then broken again is reported again"
        );
    }

    #[test]
    fn observe_and_queue_share_what_was_already_shown() {
        let mut n = SkipNotice::default();
        let a = vec!["x".to_string()];
        assert!(n.observe(&a).is_some());
        n.queue(&a);
        assert_eq!(n.take(), None, "the palette already showed it");
    }

    #[test]
    fn a_toml_syntax_error_is_still_a_whole_file_error() {
        let path = scratch_path("syntax");
        write(&path, "[[actions]\nid = ");
        let result = load_actions_report_from(&path);
        cleanup(&path);
        assert!(result.is_err());
    }

    #[test]
    fn every_builtin_passes_validation() {
        let builtins = builtin_actions();
        assert!(!builtins.is_empty());
        for a in builtins {
            validate_action(&a).unwrap_or_else(|e| panic!("{}: {e:#}", a.id));
        }
    }

    #[test]
    fn replace_text_with_a_selection_input_loads() {
        let path = scratch_path("replace-with-selection");
        write(
            &path,
            &REPLACE_NO_SELECTION.replace(r#"["clipboard"]"#, r#"["selection", "clipboard"]"#),
        );
        let resolved = load_actions_from(&path);
        cleanup(&path);
        assert!(resolved.unwrap().iter().any(|r| r.action.id == "shout"));
    }

    #[test]
    fn a_disabled_replace_text_action_without_a_selection_still_loads() {
        let path = scratch_path("replace-disabled");
        write(&path, &format!("{REPLACE_NO_SELECTION}enabled = false\n"));
        let report = load_actions_report_from(&path).unwrap();
        cleanup(&path);
        assert!(report.skipped.is_empty(), "a hidden action is not checked");
    }

    #[test]
    fn every_builtin_replace_text_action_lists_selection() {
        let replacers: Vec<_> = builtin_actions()
            .into_iter()
            .filter(|a| a.executor == "replace_text")
            .collect();
        assert!(!replacers.is_empty());
        for a in replacers {
            assert!(a.inputs.contains(&InputKind::Selection), "{}", a.id);
        }
    }
}
