//! Pure model for the Quick Ask palette (#25): fuzzy scoring, grouped vs.
//! flat ranking (#199, #132), the key-handling state machine, and the
//! action-id dispatch table. No `windows` crate dependency anywhere in this
//! file (AGENTS.md rule 8) -- see `ui::palette` for the real window that
//! wraps this, and
//! `docs/superpowers/specs/2026-09-17-palette-design.md` for the rules this
//! file implements and why.

/// One selectable action in the palette's catalogue, built fresh on every
/// `show` from `actions::load_actions()` plus the two tray-only utility
/// entries (see [`CALCULATE_SELECTION_ACTION_ID`] / [`COPY_REGION_ACTION_ID`]
/// below) -- never persisted, never mutated in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteAction {
    pub id: String,
    pub name: String,
    pub group: Option<String>,
    /// Whether this action needs a configured, ready model to run at all.
    /// `false` for the offline utilities (#132): "Copy text from screen",
    /// "Calculate selection", "Copy region to clipboard".
    pub requires_model: bool,
}

/// One row the palette shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A group name header (#199), shown only in the grouped (empty-query)
    /// view.
    Header(String),
    /// #132: "Add a model in Settings to unlock more actions." Shown once,
    /// only in the grouped view, only when no provider is configured.
    Hint(String),
    /// A runnable action. `score` is 0 in the grouped view (ranking is by
    /// catalogue order there, not fuzzy score) and the real fuzzy score in
    /// the flat, ranked (non-empty query) view.
    Action {
        id: String,
        name: String,
        score: i32,
    },
}

/// The tray-only "Calculate selection" utility's palette id. Not a built-in
/// `Action` (it has no proposal/executor/prompt -- `calc::run_on_selection`
/// runs entirely offline, no model involved), so it needs its own id here
/// rather than reusing anything from `actions::`.
pub const CALCULATE_SELECTION_ACTION_ID: &str = "calculate-selection";
/// The tray-only "Copy region to clipboard" utility's palette id. Same
/// reasoning as [`CALCULATE_SELECTION_ACTION_ID`].
pub const COPY_REGION_ACTION_ID: &str = "copy-region";

/// #132's exact hint text. No em dash (rule 11).
pub const NO_MODEL_HINT: &str = "Add a model in Settings to unlock more actions.";

/// #358's exact hint text, shown in place of a blank list when a non-empty
/// query matches no action at all. No em dash (rule 11).
pub const NO_MATCHES_HINT: &str = "No matching actions. Press Esc to clear.";

// ---------------------------------------------------------------------------
// Fuzzy scoring
// ---------------------------------------------------------------------------

/// Subsequence match of `query` (case-insensitive) against `candidate`.
/// `None` when `query` is not a subsequence of `candidate` at all (never a
/// negative score standing in for "no match" -- callers filter on `None`).
///
/// Scoring, higher is a better match:
/// - `+2` for a matched character that starts a word (position 0, or
///   immediately after a space);
/// - `+1` for any other matched character;
/// - `+3` extra when the very first matched character is `candidate`'s own
///   first character (the whole query matches as a prefix-anchored run).
///
/// An empty `query` matches everything with score `0` (used by the grouped,
/// empty-query view, which never calls this for ranking but does for the
/// `requires_model` filter's "does this row exist at all" check).
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    if query.trim().is_empty() {
        return Some(0);
    }

    let q: Vec<char> = query.to_lowercase().chars().collect();
    let c: Vec<char> = candidate.to_lowercase().chars().collect();

    let mut qi = 0;
    let mut score = 0;
    let mut first_match_pos: Option<usize> = None;
    for (ci, ch) in c.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            if first_match_pos.is_none() {
                first_match_pos = Some(ci);
            }
            let word_start = ci == 0 || c[ci - 1] == ' ';
            score += if word_start { 2 } else { 1 };
            qi += 1;
        }
    }

    if qi < q.len() {
        return None; // query was not a full subsequence of candidate
    }
    if first_match_pos == Some(0) {
        score += 3;
    }
    Some(score)
}

// ---------------------------------------------------------------------------
// Grouping / ranking (#199, #132)
// ---------------------------------------------------------------------------

/// Builds the palette's rows for `actions` given the current `query` and
/// whether a provider is configured and ready (`model_configured`). See the
/// design spec's "Grouping and ranking" section.
pub fn build_rows(actions: &[PaletteAction], query: &str, model_configured: bool) -> Vec<Row> {
    if query.trim().is_empty() {
        grouped_rows(actions, model_configured)
    } else {
        let rows = flat_ranked_rows(actions, query);
        if rows.is_empty() {
            // #358: a non-empty query that matches nothing must not just
            // leave the list blank -- that reads as broken, not empty.
            vec![Row::Hint(NO_MATCHES_HINT.to_string())]
        } else {
            rows
        }
    }
}

fn grouped_rows(actions: &[PaletteAction], model_configured: bool) -> Vec<Row> {
    let mut rows = Vec::new();

    let (free, gated): (Vec<&PaletteAction>, Vec<&PaletteAction>) = if model_configured {
        (Vec::new(), actions.iter().collect())
    } else {
        (
            actions.iter().filter(|a| !a.requires_model).collect(),
            actions.iter().filter(|a| a.requires_model).collect(),
        )
    };

    for a in &free {
        rows.push(Row::Action {
            id: a.id.clone(),
            name: a.name.clone(),
            score: 0,
        });
    }
    if !model_configured {
        // Shown right after the free actions (even when there are none), so
        // it always sits before any grouped/gated section.
        rows.push(Row::Hint(NO_MODEL_HINT.to_string()));
    }

    // Ungrouped actions among `gated` (or, when model_configured, among
    // every action) go first, no header, preserving catalogue order.
    let mut seen_groups: Vec<String> = Vec::new();
    for a in gated.iter().filter(|a| a.group.is_none()) {
        rows.push(Row::Action {
            id: a.id.clone(),
            name: a.name.clone(),
            score: 0,
        });
    }
    for a in &gated {
        if let Some(group) = &a.group {
            if !seen_groups.contains(group) {
                seen_groups.push(group.clone());
                rows.push(Row::Header(group.clone()));
                for member in gated.iter().filter(|m| m.group.as_ref() == Some(group)) {
                    rows.push(Row::Action {
                        id: member.id.clone(),
                        name: member.name.clone(),
                        score: 0,
                    });
                }
            }
        }
    }

    rows
}

fn flat_ranked_rows(actions: &[PaletteAction], query: &str) -> Vec<Row> {
    let mut scored: Vec<(i32, &PaletteAction)> = actions
        .iter()
        .filter_map(|a| fuzzy_score(query, &a.name).map(|s| (s, a)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored
        .into_iter()
        .map(|(score, a)| Row::Action {
            id: a.id.clone(),
            name: a.name.clone(),
            score,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Key handling state machine
// ---------------------------------------------------------------------------

/// Virtual-key codes this module cares about, spelled out as plain
/// constants (not imported from `windows`) so this file stays free of a
/// Win32 dependency. Values match `VK_UP`/`VK_DOWN`/`VK_RETURN`/`VK_ESCAPE`
/// exactly.
const VK_UP: u16 = 0x26;
const VK_DOWN: u16 = 0x28;
const VK_PRIOR: u16 = 0x21; // PageUp
const VK_NEXT: u16 = 0x22; // PageDown
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;

/// #217: how many rows the palette paints at once. Used both for the
/// window's fixed height (`ui::palette`'s `window_height`/`on_paint`) and as
/// the page size for [`PaletteKey::PageUp`]/[`PaletteKey::PageDown`] and the
/// viewport math below -- one number, so the page a PageDown press jumps
/// always matches what's actually on screen.
pub const MAX_VISIBLE_ROWS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteKey {
    Up,
    Down,
    PageUp,
    PageDown,
    Enter,
    Escape,
}

/// Maps a raw virtual-key code to the palette command it means, or `None`
/// for anything the palette does not handle as a command (ordinary typing
/// keys fall through to the edit control unchanged).
pub fn palette_key_from_vk(vk: u16) -> Option<PaletteKey> {
    match vk {
        VK_UP => Some(PaletteKey::Up),
        VK_DOWN => Some(PaletteKey::Down),
        VK_PRIOR => Some(PaletteKey::PageUp),
        VK_NEXT => Some(PaletteKey::PageDown),
        VK_RETURN => Some(PaletteKey::Enter),
        VK_ESCAPE => Some(PaletteKey::Escape),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Viewport math (#217): the palette shows only MAX_VISIBLE_ROWS rows at a
// time once the catalogue grows past that; `PaletteState::offset` is the
// index of the first row currently painted. Both scrolling paths in
// `ui::palette` go through one of these two pure functions rather than each
// doing its own clamping arithmetic.
// ---------------------------------------------------------------------------

/// Adjusts `offset` so row `selected` is inside the visible window
/// `[offset, offset + visible_rows)`, moving it the minimum amount needed
/// (never re-centers). The keyboard-scrolling half of #217:
/// [`PaletteState::move_selection`] calls this after moving the selection,
/// so Up/Down/PageUp/PageDown drag the viewport along only when the new
/// selection actually fell outside it. Always clamped to
/// `[0, total_rows.saturating_sub(visible_rows)]`, so the list never scrolls
/// past its own last page; returns `0` when everything already fits
/// (`total_rows <= visible_rows`) or `visible_rows == 0`.
pub fn clamp_offset_to_selection(
    offset: usize,
    selected: usize,
    total_rows: usize,
    visible_rows: usize,
) -> usize {
    if visible_rows == 0 || total_rows <= visible_rows {
        return 0;
    }
    let max_offset = total_rows - visible_rows;
    let mut offset = offset.min(max_offset);
    if selected < offset {
        offset = selected;
    } else if selected >= offset + visible_rows {
        offset = selected + 1 - visible_rows;
    }
    offset.min(max_offset)
}

/// Moves the viewport by `delta_rows` (negative towards the top), clamped to
/// `[0, total_rows.saturating_sub(visible_rows)]`. The mouse-wheel half of
/// #217: unlike [`clamp_offset_to_selection`], this never touches
/// `PaletteState::selected` -- wheeling the list does not change which row
/// Enter would run.
pub fn scroll_by(offset: usize, delta_rows: i32, total_rows: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 || total_rows <= visible_rows {
        return 0;
    }
    let max_offset = (total_rows - visible_rows) as i32;
    (offset as i32 + delta_rows).clamp(0, max_offset) as usize
}

/// The palette's selection state over a built row list. Rebuilt (via
/// [`PaletteState::new`]) every time the query changes; selection resets to
/// the first selectable (non-header, non-hint) row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteState {
    pub rows: Vec<Row>,
    pub selected: usize,
    /// #217: index of the first row currently painted. Kept in lockstep
    /// with `selected` by [`PaletteState::move_selection`]; the mouse wheel
    /// (`ui::palette`'s `WM_MOUSEWHEEL` handler) moves it directly via
    /// [`scroll_by`] instead, without touching `selected`.
    pub offset: usize,
}

fn is_selectable(row: &Row) -> bool {
    matches!(row, Row::Action { .. })
}

impl PaletteState {
    pub fn new(rows: Vec<Row>) -> Self {
        let selected = rows.iter().position(is_selectable).unwrap_or(0);
        let offset = clamp_offset_to_selection(0, selected, rows.len(), MAX_VISIBLE_ROWS);
        Self {
            rows,
            selected,
            offset,
        }
    }

    /// Moves the selection by `delta` (+1 down, -1 up), skipping over
    /// headers/hints, and clamping (not wrapping) at the first/last
    /// selectable row. `delta` may be larger than 1 (PageUp/PageDown use
    /// `MAX_VISIBLE_ROWS`) -- the clamp already handles any magnitude.
    ///
    /// #217: also drags `offset` along via [`clamp_offset_to_selection`], so
    /// every caller (Up, Down, PageUp, PageDown) gets keyboard scrolling for
    /// free instead of having to remember to clamp the viewport itself.
    pub fn move_selection(&mut self, delta: i32) {
        let selectable: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| is_selectable(r))
            .map(|(i, _)| i)
            .collect();
        if selectable.is_empty() {
            return;
        }
        let current_pos = selectable
            .iter()
            .position(|&i| i == self.selected)
            .unwrap_or(0);
        let new_pos = (current_pos as i32 + delta).clamp(0, selectable.len() as i32 - 1);
        self.selected = selectable[new_pos as usize];
        self.offset = clamp_offset_to_selection(
            self.offset,
            self.selected,
            self.rows.len(),
            MAX_VISIBLE_ROWS,
        );
    }

    pub fn selected_action_id(&self) -> Option<&str> {
        match self.rows.get(self.selected) {
            Some(Row::Action { id, .. }) => Some(id.as_str()),
            _ => None,
        }
    }
}

/// #24: pre-selects the row whose action id is `action_id`, for the intent
/// router's suggestion. `false` (a no-op, never a panic -- rule 7) when
/// `action_id` isn't among today's ROWS at all -- e.g. the row only shows up
/// once a provider is configured, and the grouped view currently has none,
/// or the query has since filtered it out. Whether this should even be
/// attempted (confidence vs. threshold, whether the user already
/// typed/moved) is [`crate::router::should_apply`]'s job, not this
/// function's -- this only performs the mechanical "does this id exist as a
/// row right now, and if so select it" step, which is why it lives here
/// (state mutation) rather than in `router.rs` (decision).
pub fn preselect_action(state: &mut PaletteState, action_id: &str) -> bool {
    match state
        .rows
        .iter()
        .position(|r| matches!(r, Row::Action { id, .. } if id == action_id))
    {
        Some(idx) => {
            state.selected = idx;
            true
        }
        None => false,
    }
}

/// What handling a key should cause the caller (the Win32 layer) to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteOutcome {
    /// Selection moved (or tried to); repaint the list.
    None,
    /// Enter on a runnable row: dispatch this action id, then hide.
    Run(String),
    /// Esc: hide, no dispatch.
    Hide,
}

/// The whole key-handling state machine in one pure function (AGENTS.md
/// rule 8): given the current state and a recognized key, what happens.
pub fn handle_key(state: &mut PaletteState, key: PaletteKey) -> PaletteOutcome {
    match key {
        PaletteKey::Up => {
            state.move_selection(-1);
            PaletteOutcome::None
        }
        PaletteKey::Down => {
            state.move_selection(1);
            PaletteOutcome::None
        }
        // #217: a page is `MAX_VISIBLE_ROWS` selectable positions -- an
        // approximation (headers/hints in between mean a literal page of
        // ROWS is not always exactly `MAX_VISIBLE_ROWS` selectable actions),
        // but `move_selection`'s existing clamp already makes this safe at
        // any magnitude, and it means PageDown always covers at least one
        // full screen's worth of rows.
        PaletteKey::PageUp => {
            state.move_selection(-(MAX_VISIBLE_ROWS as i32));
            PaletteOutcome::None
        }
        PaletteKey::PageDown => {
            state.move_selection(MAX_VISIBLE_ROWS as i32);
            PaletteOutcome::None
        }
        PaletteKey::Enter => match state.selected_action_id() {
            Some(id) => PaletteOutcome::Run(id.to_string()),
            None => PaletteOutcome::None,
        },
        PaletteKey::Escape => PaletteOutcome::Hide,
    }
}

// ---------------------------------------------------------------------------
// Dispatch table: action id -> the same entry point the tray item uses
// ---------------------------------------------------------------------------

/// Closed set of everywhere Enter can route to. `ui::palette`'s Win32 layer
/// matches this to call the exact `App` method the tray already calls for
/// that id -- never a second, palette-only code path. `Generic` (#242) is
/// the one open-ended arm: any action id that is not one of the six fixed
/// built-ins or two tray-only utilities still routes somewhere runnable,
/// through `App::run_generic_action`'s own generic Look/Propose/Confirm/Do
/// plumbing (schema + executor resolved from the action's own record at
/// dispatch time, not baked into this enum).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchTarget {
    CheckMyWork,
    ExtractText,
    AddToCalendar,
    ReviewEmail,
    FillForm,
    CalculateSelection,
    CopyRegion,
    /// Any other action id (#242): a user-authored `actions.toml` entry,
    /// carried by value so `App::dispatch_palette_action` can look the
    /// action back up by id without a second table.
    Generic(String),
}

/// The one lookup from a palette row's action id to what running it means.
/// Every built-in id and the two tray-only utilities route to their own
/// named arm; everything else (#242) routes to `Generic`, never `None` --
/// the palette only ever shows ids `actions::load_actions` actually
/// resolved (see `catalogue`), so there is no id left that has nowhere to
/// go.
///
/// Every named arm above is asserted to produce its own exact,
/// non-`Generic` `DispatchTarget` by `dispatch_covers_every_built_in_action`
/// below, so a built-in added to `actions::builtin_actions()` (or a new
/// tray-only utility) without a matching arm here fails a test instead of
/// silently falling through to the generic path and losing its dedicated
/// dispatch (the "wired to nothing" shape AGENTS.md rule 8 calls out).
pub fn dispatch_target_for(action_id: &str) -> Option<DispatchTarget> {
    match action_id {
        crate::actions::DEFAULT_ACTION_ID => Some(DispatchTarget::CheckMyWork),
        crate::actions::EXTRACT_TEXT_ACTION_ID => Some(DispatchTarget::ExtractText),
        crate::actions::calendar::ACTION_ID => Some(DispatchTarget::AddToCalendar),
        crate::actions::review_email::ACTION_ID => Some(DispatchTarget::ReviewEmail),
        crate::actions::fill_form::ACTION_ID => Some(DispatchTarget::FillForm),
        CALCULATE_SELECTION_ACTION_ID => Some(DispatchTarget::CalculateSelection),
        COPY_REGION_ACTION_ID => Some(DispatchTarget::CopyRegion),
        _ => Some(DispatchTarget::Generic(action_id.to_string())),
    }
}

/// Whether `action_id` needs a model to run, the single rule
/// [`catalogue`] uses to fill [`PaletteAction::requires_model`]. A small,
/// explicit id list rather than deriving it from `Action::proposal` (empty
/// vs. non-empty): the two tray-only utilities have no `Action` at all to
/// read a proposal from, so one list covering every id (built-in or
/// utility) is the only place this fact can live without splitting the rule
/// across two different lookups.
fn action_requires_model(action_id: &str) -> bool {
    let model_free = action_id == crate::actions::EXTRACT_TEXT_ACTION_ID
        || action_id == CALCULATE_SELECTION_ACTION_ID
        || action_id == COPY_REGION_ACTION_ID;
    !model_free
}

/// Builds the palette's catalogue from already-resolved, already-visible
/// actions (`actions::load_actions()`'s return value) plus the two
/// tray-only utilities, which have no `Action` entry to resolve from.
pub fn catalogue(resolved: &[crate::actions::Resolved]) -> Vec<PaletteAction> {
    let mut out: Vec<PaletteAction> = resolved
        .iter()
        .map(|r| PaletteAction {
            id: r.action.id.clone(),
            name: r.action.name.clone(),
            group: r.action.group.clone(),
            requires_model: action_requires_model(&r.action.id),
        })
        .collect();
    out.push(PaletteAction {
        id: CALCULATE_SELECTION_ACTION_ID.to_string(),
        name: "Calculate selection".to_string(),
        group: None,
        requires_model: false,
    });
    out.push(PaletteAction {
        id: COPY_REGION_ACTION_ID.to_string(),
        name: "Copy region to clipboard".to_string(),
        group: None,
        requires_model: false,
    });
    out
}

// ---------------------------------------------------------------------------
// Footer
// ---------------------------------------------------------------------------

/// `"mode: Auto - openai:gpt-5"`, or just `"mode: Auto"` when
/// `provider_model` is `None` (nothing configured / ready). No em dash
/// (rule 11) -- a hyphen, matching the rest of this crate's footer-style
/// strings.

fn friendly_model_name(provider_model: &str) -> String {
    let model = provider_model
        .split_once(':')
        .map(|(_, model)| model)
        .unwrap_or(provider_model);

    if model.is_empty() {
        return provider_model.to_string();
    }

    if let Some(rest) = model.strip_prefix("gpt-") {
        return format!("GPT-{rest}");
    }

    if let Some(rest) = model.strip_prefix("claude-") {
        let parts: Vec<&str> = rest.split('-').collect();

        let (words, version) = if parts.len() >= 3
            && parts[parts.len() - 1].chars().all(|c| c.is_ascii_digit())
            && parts[parts.len() - 2].chars().all(|c| c.is_ascii_digit())
        {
            (
                &parts[..parts.len() - 2],
                Some(format!(
                    "{}.{}",
                    parts[parts.len() - 2],
                    parts[parts.len() - 1]
                )),
            )
        } else if parts.len() >= 2 && parts[parts.len() - 1].chars().all(|c| c.is_ascii_digit()) {
            (
                &parts[..parts.len() - 1],
                Some(parts[parts.len() - 1].to_string()),
            )
        } else {
            (&parts[..], None)
        };

        let name = words
            .iter()
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");

        return match version {
            Some(version) => format!("Claude {name} {version}"),
            None => format!("Claude {name}"),
        };
    }

    if let Some(rest) = model.strip_prefix("gemini-") {
        return rest
            .split('-')
            .fold("Gemini".to_string(), |mut result, word| {
                result.push(' ');
                let mut chars = word.chars();
                if let Some(first) = chars.next() {
                    result.push_str(&first.to_uppercase().collect::<String>());
                    result.push_str(chars.as_str());
                }
                result
            });
    }

    model.to_string()
}

pub fn footer_line(mode_label: &str, provider_model: Option<&str>) -> String {
    match provider_model {
        Some(pm) => {
            let friendly_name = friendly_model_name(pm);
            format!("{mode_label} · {friendly_name}")
        }
        None => mode_label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(id: &str, name: &str, group: Option<&str>, requires_model: bool) -> PaletteAction {
        PaletteAction {
            id: id.to_string(),
            name: name.to_string(),
            group: group.map(|g| g.to_string()),
            requires_model,
        }
    }

    // -- fuzzy_score ---------------------------------------------------

    #[test]
    fn empty_query_matches_everything_with_zero_score() {
        assert_eq!(fuzzy_score("", "Check my work"), Some(0));
        assert_eq!(fuzzy_score("   ", "Check my work"), Some(0));
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert_eq!(fuzzy_score("xyz", "Check my work"), None);
        assert_eq!(fuzzy_score("workcheck", "Check my work"), None);
    }

    #[test]
    fn subsequence_out_of_order_characters_do_not_match() {
        // 'k' before 'c' -- not a subsequence of "Check".
        assert_eq!(fuzzy_score("kc", "Check"), None);
    }

    #[test]
    fn case_insensitive_match() {
        assert!(fuzzy_score("CHECK", "check my work").is_some());
        assert!(fuzzy_score("check", "CHECK MY WORK").is_some());
    }

    #[test]
    fn prefix_match_scores_higher_than_mid_string_match() {
        let prefix = fuzzy_score("che", "Check my work").unwrap();
        let mid = fuzzy_score("y wo", "Check my work").unwrap();
        assert!(prefix > mid, "prefix={prefix} should beat mid-string={mid}");
    }

    #[test]
    fn word_start_bonus_beats_a_match_with_no_word_starts() {
        // "cw": 'C' at position 0 (word start, prefix) + 'w' at the start of
        // "work" (word start) -- both bonuses.
        let word_starts = fuzzy_score("cw", "Check work").unwrap();
        // "cw" against "acbw": 'c' matches mid-word (not position 0, not
        // after a space), 'w' matches mid-word too -- no word-start bonus,
        // no prefix bonus.
        let no_word_starts = fuzzy_score("cw", "acbw").unwrap();
        assert!(
            word_starts > no_word_starts,
            "word_starts={word_starts} should beat no_word_starts={no_word_starts}"
        );
    }

    #[test]
    fn exact_prefix_beats_non_prefix_subsequence_of_equal_length() {
        let exact_prefix = fuzzy_score("add", "Add to calendar").unwrap();
        let non_prefix = fuzzy_score("add", "Copy add region").unwrap();
        assert!(exact_prefix > non_prefix);
    }

    // -- build_rows: grouping / ordering (#199) -------------------------

    #[test]
    fn empty_query_groups_by_group_with_headers_in_first_seen_order() {
        let actions = vec![
            action("a", "Check my work", Some("Study"), true),
            action("b", "Add to calendar", Some("Work"), true),
            action("c", "Define word", Some("Study"), true),
        ];
        let rows = build_rows(&actions, "", true);
        assert_eq!(
            rows,
            vec![
                Row::Header("Study".to_string()),
                Row::Action {
                    id: "a".into(),
                    name: "Check my work".into(),
                    score: 0
                },
                Row::Action {
                    id: "c".into(),
                    name: "Define word".into(),
                    score: 0
                },
                Row::Header("Work".to_string()),
                Row::Action {
                    id: "b".into(),
                    name: "Add to calendar".into(),
                    score: 0
                },
            ]
        );
    }

    #[test]
    fn empty_query_ungrouped_actions_have_no_header_and_come_first() {
        let actions = vec![
            action("a", "Check my work", Some("Study"), true),
            action("b", "Calculate selection", None, false),
        ];
        let rows = build_rows(&actions, "", true);
        assert_eq!(
            rows[0],
            Row::Action {
                id: "b".into(),
                name: "Calculate selection".into(),
                score: 0
            }
        );
        assert!(rows.contains(&Row::Header("Study".to_string())));
    }

    #[test]
    fn typing_a_query_produces_a_flat_list_with_no_headers() {
        let actions = vec![
            action("a", "Check my work", Some("Study"), true),
            action("b", "Add to calendar", Some("Work"), true),
        ];
        let rows = build_rows(&actions, "check", true);
        assert!(!rows.iter().any(|r| matches!(r, Row::Header(_))));
        assert!(!rows.iter().any(|r| matches!(r, Row::Hint(_))));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn typing_a_query_ranks_by_score_descending() {
        let actions = vec![
            action("a", "Add to calendar", None, true),
            action("b", "Calculate selection", None, false),
        ];
        // "ca" is a prefix of "Calculate selection" but not of "Add to
        // calendar" (it matches mid-string there via "...calendar" 'c','a').
        let rows = build_rows(&actions, "ca", true);
        let ids: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Action { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids[0], "b", "prefix match must rank first: {rows:?}");
    }

    #[test]
    fn typing_a_query_that_matches_nothing_shows_the_no_matches_hint() {
        let actions = vec![
            action("a", "Check my work", Some("Study"), true),
            action("b", "Add to calendar", Some("Work"), true),
        ];
        let rows = build_rows(&actions, "zzz", true);
        assert_eq!(rows, vec![Row::Hint(NO_MATCHES_HINT.to_string())]);
    }

    #[test]
    fn typing_a_query_that_matches_something_shows_no_hint() {
        let actions = vec![action("a", "Check my work", Some("Study"), true)];
        let rows = build_rows(&actions, "check", true);
        assert!(!rows.iter().any(|r| matches!(r, Row::Hint(_))));
    }

    // -- build_rows: no-model ordering (#132) ----------------------------

    #[test]
    fn no_model_configured_lists_model_free_actions_first_then_a_hint() {
        let actions = vec![
            action("a", "Check my work", Some("Study"), true),
            action("b", "Copy text from screen", Some("Work"), false),
            action("c", "Calculate selection", None, false),
        ];
        let rows = build_rows(&actions, "", false);
        // First two rows: the model-free actions, in catalogue order, no
        // header.
        assert_eq!(
            rows[0],
            Row::Action {
                id: "b".into(),
                name: "Copy text from screen".into(),
                score: 0
            }
        );
        assert_eq!(
            rows[1],
            Row::Action {
                id: "c".into(),
                name: "Calculate selection".into(),
                score: 0
            }
        );
        assert_eq!(rows[2], Row::Hint(NO_MODEL_HINT.to_string()));
        // The gated action still appears, grouped, after the hint.
        assert!(rows.contains(&Row::Header("Study".to_string())));
        assert!(rows.contains(&Row::Action {
            id: "a".into(),
            name: "Check my work".into(),
            score: 0
        }));
    }

    #[test]
    fn model_configured_has_no_hint_and_no_free_gated_split() {
        let actions = vec![
            action("a", "Check my work", Some("Study"), true),
            action("b", "Copy text from screen", Some("Work"), false),
        ];
        let rows = build_rows(&actions, "", true);
        assert!(!rows.iter().any(|r| matches!(r, Row::Hint(_))));
    }

    #[test]
    fn hint_appears_even_when_there_are_no_free_actions() {
        let actions = vec![action("a", "Check my work", Some("Study"), true)];
        let rows = build_rows(&actions, "", false);
        assert!(rows.iter().any(|r| matches!(r, Row::Hint(_))));
    }

    // -- key handling state machine --------------------------------------

    #[test]
    fn palette_key_from_vk_recognizes_the_six_keys() {
        assert_eq!(palette_key_from_vk(0x26), Some(PaletteKey::Up));
        assert_eq!(palette_key_from_vk(0x28), Some(PaletteKey::Down));
        assert_eq!(palette_key_from_vk(0x21), Some(PaletteKey::PageUp));
        assert_eq!(palette_key_from_vk(0x22), Some(PaletteKey::PageDown));
        assert_eq!(palette_key_from_vk(0x0D), Some(PaletteKey::Enter));
        assert_eq!(palette_key_from_vk(0x1B), Some(PaletteKey::Escape));
        assert_eq!(palette_key_from_vk(0x41), None); // 'A', ordinary typing
    }

    fn three_action_rows() -> Vec<Row> {
        vec![
            Row::Header("Study".to_string()),
            Row::Action {
                id: "a".into(),
                name: "A".into(),
                score: 0,
            },
            Row::Action {
                id: "b".into(),
                name: "B".into(),
                score: 0,
            },
            Row::Header("Work".to_string()),
            Row::Action {
                id: "c".into(),
                name: "C".into(),
                score: 0,
            },
        ]
    }

    #[test]
    fn new_state_selects_the_first_selectable_row_not_a_header() {
        let state = PaletteState::new(three_action_rows());
        assert_eq!(state.selected_action_id(), Some("a"));
    }

    #[test]
    fn down_moves_past_headers_to_the_next_action() {
        let mut state = PaletteState::new(three_action_rows());
        let outcome = handle_key(&mut state, PaletteKey::Down);
        assert_eq!(outcome, PaletteOutcome::None);
        assert_eq!(state.selected_action_id(), Some("b"));
        handle_key(&mut state, PaletteKey::Down);
        assert_eq!(
            state.selected_action_id(),
            Some("c"),
            "must skip the 'Work' header"
        );
    }

    #[test]
    fn down_clamps_at_the_last_action_no_wraparound() {
        let mut state = PaletteState::new(three_action_rows());
        for _ in 0..10 {
            handle_key(&mut state, PaletteKey::Down);
        }
        assert_eq!(state.selected_action_id(), Some("c"));
    }

    #[test]
    fn up_clamps_at_the_first_action_no_wraparound() {
        let mut state = PaletteState::new(three_action_rows());
        for _ in 0..10 {
            handle_key(&mut state, PaletteKey::Up);
        }
        assert_eq!(state.selected_action_id(), Some("a"));
    }

    #[test]
    fn enter_on_a_selected_action_returns_run_with_its_id() {
        let mut state = PaletteState::new(three_action_rows());
        handle_key(&mut state, PaletteKey::Down);
        let outcome = handle_key(&mut state, PaletteKey::Enter);
        assert_eq!(outcome, PaletteOutcome::Run("b".to_string()));
    }

    #[test]
    fn escape_returns_hide() {
        let mut state = PaletteState::new(three_action_rows());
        assert_eq!(
            handle_key(&mut state, PaletteKey::Escape),
            PaletteOutcome::Hide
        );
    }

    #[test]
    fn enter_with_no_selectable_rows_is_a_no_op_not_a_panic() {
        let mut state = PaletteState::new(vec![Row::Header("Empty".to_string())]);
        assert_eq!(
            handle_key(&mut state, PaletteKey::Enter),
            PaletteOutcome::None
        );
        assert_eq!(
            handle_key(&mut state, PaletteKey::Down),
            PaletteOutcome::None
        );
    }

    // -- viewport math (#217) -----------------------------------------------

    #[test]
    fn clamp_offset_to_selection_is_zero_when_everything_fits() {
        assert_eq!(clamp_offset_to_selection(0, 5, 10, 12), 0);
        assert_eq!(clamp_offset_to_selection(3, 5, 10, 10), 0);
    }

    #[test]
    fn clamp_offset_to_selection_scrolls_down_to_reveal_a_selection_below_the_window() {
        // 20 rows, 5 visible, selection at row 10: offset must move so row
        // 10 is the LAST visible row (minimum movement, no re-centering).
        assert_eq!(clamp_offset_to_selection(0, 10, 20, 5), 6);
    }

    #[test]
    fn clamp_offset_to_selection_scrolls_up_to_reveal_a_selection_above_the_window() {
        // Viewport currently at rows 10..15; selection jumps to row 2.
        assert_eq!(clamp_offset_to_selection(10, 2, 20, 5), 2);
    }

    #[test]
    fn clamp_offset_to_selection_leaves_the_offset_alone_when_selection_is_already_visible() {
        assert_eq!(clamp_offset_to_selection(4, 6, 20, 5), 4);
    }

    #[test]
    fn clamp_offset_to_selection_never_scrolls_past_the_last_page() {
        // 20 rows, 5 visible: max_offset is 15. A selection at the very
        // last row (19) must not push offset past 15.
        assert_eq!(clamp_offset_to_selection(0, 19, 20, 5), 15);
    }

    #[test]
    fn clamp_offset_to_selection_zero_visible_rows_never_divides_by_zero() {
        assert_eq!(clamp_offset_to_selection(3, 1, 20, 0), 0);
    }

    #[test]
    fn scroll_by_moves_the_viewport_without_touching_selection() {
        assert_eq!(scroll_by(5, 1, 20, 5), 6);
        assert_eq!(scroll_by(5, -1, 20, 5), 4);
    }

    #[test]
    fn scroll_by_clamps_to_the_first_and_last_page() {
        assert_eq!(scroll_by(0, -1, 20, 5), 0);
        assert_eq!(scroll_by(15, 1, 20, 5), 15);
    }

    #[test]
    fn scroll_by_is_zero_when_everything_fits() {
        assert_eq!(scroll_by(0, 5, 10, 12), 0);
    }

    /// #217's Done-when: a catalogue of 20+ visible actions is fully
    /// reachable by keyboard, and the selection stays inside the viewport
    /// as it moves past row `MAX_VISIBLE_ROWS`.
    fn twenty_ungrouped_rows() -> Vec<Row> {
        (0..20)
            .map(|i| Row::Action {
                id: format!("action-{i}"),
                name: format!("Action {i}"),
                score: 0,
            })
            .collect()
    }

    #[test]
    fn down_past_max_visible_rows_scrolls_the_viewport_into_view() {
        let mut state = PaletteState::new(twenty_ungrouped_rows());
        assert_eq!(state.offset, 0);
        for _ in 0..15 {
            handle_key(&mut state, PaletteKey::Down);
        }
        assert_eq!(state.selected, 15);
        // Row 15 must be inside [offset, offset + MAX_VISIBLE_ROWS).
        assert!(state.offset <= 15 && 15 < state.offset + MAX_VISIBLE_ROWS);
        assert!(state.offset > 0, "viewport must have scrolled");
    }

    #[test]
    fn page_down_then_page_up_returns_to_the_top_of_the_viewport() {
        let mut state = PaletteState::new(twenty_ungrouped_rows());
        handle_key(&mut state, PaletteKey::PageDown);
        assert_eq!(state.selected, MAX_VISIBLE_ROWS);
        assert!(state.offset > 0);
        handle_key(&mut state, PaletteKey::PageUp);
        assert_eq!(state.selected, 0);
        assert_eq!(state.offset, 0);
    }

    #[test]
    fn page_down_clamps_at_the_last_row_no_panic() {
        let mut state = PaletteState::new(twenty_ungrouped_rows());
        for _ in 0..5 {
            handle_key(&mut state, PaletteKey::PageDown);
        }
        assert_eq!(state.selected, 19);
        assert_eq!(state.offset, 20 - MAX_VISIBLE_ROWS);
    }

    // -- preselect_action (#24: the intent router's palette-side half) -----

    #[test]
    fn preselect_action_selects_the_matching_row() {
        let mut state = PaletteState::new(three_action_rows());
        assert_eq!(state.selected_action_id(), Some("a"));
        assert!(preselect_action(&mut state, "c"));
        assert_eq!(state.selected_action_id(), Some("c"));
    }

    #[test]
    fn preselect_action_false_when_the_id_is_not_a_row_right_now() {
        let mut state = PaletteState::new(three_action_rows());
        assert!(!preselect_action(&mut state, "not-a-row"));
        // Selection is untouched by a failed attempt.
        assert_eq!(state.selected_action_id(), Some("a"));
    }

    // -- dispatch table: every built-in, tested (rule 8) ------------------

    /// Every built-in action id (from `actions::builtin_actions()`) and
    /// both tray-only utility ids must route to their OWN named
    /// `DispatchTarget` arm, never to `Generic` -- `Generic` is only for an
    /// id nothing above special-cases (#242). `is_some()` alone stopped
    /// being a meaningful assertion once `dispatch_target_for`'s `_` arm
    /// started returning `Some(Generic(..))` for everything: this asserts
    /// the exact expected value per id instead, so a built-in that
    /// regresses to the generic path (silently losing its dedicated
    /// dispatch, e.g. its own preview headline or a fixed-name executor)
    /// fails this test instead of passing it vacuously.
    #[test]
    fn dispatch_covers_every_built_in_action() {
        for a in crate::actions::builtin_actions() {
            assert!(
                !matches!(dispatch_target_for(&a.id), Some(DispatchTarget::Generic(_))),
                "built-in action {:?} dispatches generically -- it lost its own named \
                 DispatchTarget arm",
                a.id
            );
        }
        assert_eq!(
            dispatch_target_for(crate::actions::DEFAULT_ACTION_ID),
            Some(DispatchTarget::CheckMyWork)
        );
        assert_eq!(
            dispatch_target_for(crate::actions::EXTRACT_TEXT_ACTION_ID),
            Some(DispatchTarget::ExtractText)
        );
        assert_eq!(
            dispatch_target_for(crate::actions::calendar::ACTION_ID),
            Some(DispatchTarget::AddToCalendar)
        );
        assert_eq!(
            dispatch_target_for(crate::actions::review_email::ACTION_ID),
            Some(DispatchTarget::ReviewEmail)
        );
        assert_eq!(
            dispatch_target_for(crate::actions::fill_form::ACTION_ID),
            Some(DispatchTarget::FillForm)
        );
        assert_eq!(
            dispatch_target_for(CALCULATE_SELECTION_ACTION_ID),
            Some(DispatchTarget::CalculateSelection)
        );
        assert_eq!(
            dispatch_target_for(COPY_REGION_ACTION_ID),
            Some(DispatchTarget::CopyRegion)
        );
    }

    #[test]
    fn dispatch_targets_are_the_expected_distinct_values() {
        assert_eq!(
            dispatch_target_for(crate::actions::DEFAULT_ACTION_ID),
            Some(DispatchTarget::CheckMyWork)
        );
        assert_eq!(
            dispatch_target_for(crate::actions::EXTRACT_TEXT_ACTION_ID),
            Some(DispatchTarget::ExtractText)
        );
        assert_eq!(
            dispatch_target_for(crate::actions::calendar::ACTION_ID),
            Some(DispatchTarget::AddToCalendar)
        );
    }

    /// #242: an id `dispatch_target_for` does not special-case is no longer
    /// a dead end -- it routes to `Generic`, carrying the id along so
    /// `App::dispatch_palette_action` can look the action back up and run
    /// it through the generic Look/Propose/Confirm/Do path. This test
    /// replaces `unknown_action_id_dispatches_to_nothing_not_a_panic`,
    /// which asserted the bug this issue reports (a user-authored
    /// `actions.toml` id parsing, displaying and then silently no-opping on
    /// Enter).
    #[test]
    fn unknown_action_id_dispatches_generically_instead_of_to_nothing() {
        assert_eq!(
            dispatch_target_for("translate-selection"),
            Some(DispatchTarget::Generic("translate-selection".to_string()))
        );
        assert_eq!(
            dispatch_target_for("not-a-real-action"),
            Some(DispatchTarget::Generic("not-a-real-action".to_string()))
        );
    }

    // -- catalogue ---------------------------------------------------------

    #[test]
    fn catalogue_includes_every_resolved_action_plus_the_two_utilities() {
        let resolved = crate::actions::merge_actions(crate::actions::builtin_actions(), vec![]);
        let cat = catalogue(&resolved);
        assert!(cat
            .iter()
            .any(|a| a.id == crate::actions::DEFAULT_ACTION_ID));
        assert!(cat.iter().any(|a| a.id == CALCULATE_SELECTION_ACTION_ID));
        assert!(cat.iter().any(|a| a.id == COPY_REGION_ACTION_ID));
        assert_eq!(cat.len(), resolved.len() + 2);
    }

    #[test]
    fn catalogue_marks_model_free_actions_correctly() {
        let resolved = crate::actions::merge_actions(crate::actions::builtin_actions(), vec![]);
        let cat = catalogue(&resolved);
        let check_my_work = cat
            .iter()
            .find(|a| a.id == crate::actions::DEFAULT_ACTION_ID)
            .unwrap();
        assert!(check_my_work.requires_model);
        let extract_text = cat
            .iter()
            .find(|a| a.id == crate::actions::EXTRACT_TEXT_ACTION_ID)
            .unwrap();
        assert!(!extract_text.requires_model);
        let calc = cat
            .iter()
            .find(|a| a.id == CALCULATE_SELECTION_ACTION_ID)
            .unwrap();
        assert!(!calc.requires_model);
    }

    // -- footer_line ---------------------------------------------------------

    #[test]
    fn footer_line_formats_gpt_models() {
        assert_eq!(
            footer_line("Auto", Some("openai:gpt-5.5")),
            "Auto · GPT-5.5"
        );
        assert_eq!(footer_line("Auto", Some("openai:gpt-4o")), "Auto · GPT-4o");
    }

    #[test]
    fn footer_line_formats_claude_models() {
        assert_eq!(
            footer_line("Auto", Some("anthropic:claude-sonnet-5")),
            "Auto · Claude Sonnet 5"
        );
        assert_eq!(
            footer_line("Auto", Some("anthropic:claude-opus-4-8")),
            "Auto · Claude Opus 4.8"
        );
    }

    #[test]
    fn footer_line_formats_gemini_models() {
        assert_eq!(
            footer_line("Auto", Some("gemini:gemini-3.8-flash")),
            "Auto · Gemini 3.8 Flash"
        );
    }

    #[test]
    fn footer_line_falls_back_to_raw_model_name() {
        assert_eq!(
            footer_line("Auto", Some("ollama:gemma3:12b")),
            "Auto · gemma3:12b"
        );
        assert_eq!(footer_line("Auto", Some("ollama")), "Auto · ollama");
    }

    #[test]
    fn footer_line_with_no_provider() {
        assert_eq!(footer_line("Offline", None), "Offline");
    }

    #[test]
    fn footer_line_has_no_em_dash() {
        // AGENTS.md rule 11.
        let a = footer_line("Auto", Some("ollama:gemma3"));
        let b = footer_line("Auto", None);
        assert!(!a.contains('\u{2014}'));
        assert!(!b.contains('\u{2014}'));
    }
}
