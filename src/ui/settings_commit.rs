//! Instant apply for the Settings window (#875, slice 5 of the Settings pages
//! spec): the parts that need no window, so they can be tested without one.
//!
//! A change in Settings is one [`Field`]. [`plan`] works out what that one field
//! changed (the keys of `config.toml` it owns, whole values made conditional on
//! the file still holding what the window saw), and what undoes it. Nothing here
//! touches the disk, the registry or a window: `settings.rs` sends the plan to the
//! writer thread and shows the answer, and [`Tracker`] remembers what is in
//! flight, what failed and what Undo would do.

use crate::config::Config;
use crate::config_writer::{self, FieldChange};
use crate::keypress::{DOUBLE_PRESS_MAX_MS, DOUBLE_PRESS_MIN_MS};

/// Everything in the window that edits one setting. The label is what the
/// status line calls it ("Saved: Card corner") and what the writer's answer is
/// matched by, so each is unique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    ActiveProvider,
    OpenAiKey,
    OpenAiModel,
    OpenAiEffort,
    AnthropicKey,
    AnthropicModel,
    AnthropicEffort,
    GeminiKey,
    GeminiModel,
    GeminiEffort,
    OllamaModel,
    PrimaryKey,
    SecondaryKey,
    PauseKey,
    KeyMode,
    DoublePressMs,
    ClickAway,
    Autostart,
    CardSeconds,
    TextScale,
    CardCorner,
    CardMonitor,
    MaxEdge,
    Monitor,
    Redact,
    RequireTickFor,
    Prompt,
}

impl Field {
    pub(crate) const ALL: [Field; 27] = [
        Field::ActiveProvider,
        Field::OpenAiKey,
        Field::OpenAiModel,
        Field::OpenAiEffort,
        Field::AnthropicKey,
        Field::AnthropicModel,
        Field::AnthropicEffort,
        Field::GeminiKey,
        Field::GeminiModel,
        Field::GeminiEffort,
        Field::OllamaModel,
        Field::PrimaryKey,
        Field::SecondaryKey,
        Field::PauseKey,
        Field::KeyMode,
        Field::DoublePressMs,
        Field::ClickAway,
        Field::Autostart,
        Field::CardSeconds,
        Field::TextScale,
        Field::CardCorner,
        Field::CardMonitor,
        Field::MaxEdge,
        Field::Monitor,
        Field::Redact,
        Field::RequireTickFor,
        Field::Prompt,
    ];

    /// The words the status line uses. No em dash (rule 11).
    pub(crate) fn label(self) -> &'static str {
        match self {
            Field::ActiveProvider => "Active provider",
            Field::OpenAiKey => "OpenAI key",
            Field::OpenAiModel => "OpenAI model",
            Field::OpenAiEffort => "OpenAI thinking time",
            Field::AnthropicKey => "Anthropic key",
            Field::AnthropicModel => "Anthropic model",
            Field::AnthropicEffort => "Anthropic thinking time",
            Field::GeminiKey => "Gemini key",
            Field::GeminiModel => "Gemini model",
            Field::GeminiEffort => "Gemini thinking time",
            Field::OllamaModel => "Ollama model",
            Field::PrimaryKey => "Copilot key",
            Field::SecondaryKey => "Quick Ask shortcut",
            Field::PauseKey => "Pause shortcut",
            Field::KeyMode => "Wingman key on a card",
            Field::DoublePressMs => "Double press time",
            Field::ClickAway => "Click away closes the card",
            Field::Autostart => "Start with Windows",
            Field::CardSeconds => "Hide card after",
            Field::TextScale => "Text size",
            Field::CardCorner => "Card corner",
            Field::CardMonitor => "Card screen",
            Field::MaxEdge => "Screenshot detail",
            Field::Monitor => "Screen to capture",
            Field::Redact => "Hide sensitive text",
            Field::RequireTickFor => "Review before filling",
            Field::Prompt => "Check my work prompt",
        }
    }

    /// The field a writer answer is about (its label).
    pub(crate) fn from_label(label: &str) -> Option<Field> {
        Field::ALL.into_iter().find(|f| f.label() == label)
    }

    /// The keys of `config.toml` this field owns, as table paths: a change is
    /// this field's when its path starts with one of these. Empty for a field
    /// that is not in the file (a key lives in Credential Manager, Start with
    /// Windows in the registry).
    pub(crate) fn paths(self) -> &'static [&'static [&'static str]] {
        match self {
            Field::ActiveProvider => &[&["providers", "order"]],
            Field::OpenAiModel => &[&["providers", "openai", "model"]],
            Field::OpenAiEffort => &[&["providers", "openai", "effort"]],
            Field::AnthropicModel => &[&["providers", "anthropic", "model"]],
            Field::AnthropicEffort => &[&["providers", "anthropic", "effort"]],
            Field::GeminiModel => &[&["providers", "gemini", "model"]],
            Field::GeminiEffort => &[&["providers", "gemini", "effort"]],
            Field::OllamaModel => &[&["providers", "ollama", "model"]],
            Field::PrimaryKey => &[&["hotkeys", "primary"]],
            Field::SecondaryKey => &[&["hotkeys", "secondary"]],
            Field::PauseKey => &[&["hotkeys", "pause"]],
            Field::KeyMode => &[&["hotkeys", "key_mode"]],
            Field::DoublePressMs => &[&["hotkeys", "double_press_ms"]],
            Field::ClickAway => &[&["options", "click_away_dismiss"]],
            Field::CardSeconds => &[&["ui", "card_seconds"]],
            Field::TextScale => &[&["ui", "text_scale"]],
            Field::CardCorner => &[&["ui", "card_corner"]],
            Field::CardMonitor => &[&["ui", "card_monitor"]],
            Field::MaxEdge => &[&["capture", "max_edge"]],
            Field::Monitor => &[&["capture", "monitor"]],
            Field::Redact => &[&["capture", "redact"]],
            Field::RequireTickFor => &[&["forms", "require_tick_for"]],
            Field::Prompt => &[&["ui", "prompt"]],
            Field::OpenAiKey | Field::AnthropicKey | Field::GeminiKey | Field::Autostart => &[],
        }
    }

    /// Which cloud vendor's key this is: 0 OpenAI, 1 Anthropic, 2 Gemini.
    pub(crate) fn key_vendor(self) -> Option<usize> {
        match self {
            Field::OpenAiKey => Some(0),
            Field::AnthropicKey => Some(1),
            Field::GeminiKey => Some(2),
            _ => None,
        }
    }

    /// A box the user types in: a change is held until Enter or leaving it, and
    /// Esc puts the applied text back.
    pub(crate) fn is_text_box(self) -> bool {
        matches!(
            self,
            Field::OpenAiKey
                | Field::AnthropicKey
                | Field::GeminiKey
                | Field::CardSeconds
                | Field::DoublePressMs
                | Field::Prompt
        )
    }

    fn owns(self, change: &FieldChange) -> bool {
        let path = change.path();
        self.paths().iter().any(|own| {
            path.len() >= own.len() && own.iter().zip(path).all(|(a, b)| *a == b.as_str())
        })
    }
}

/// What one commit sends and remembers.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    /// The keys to patch onto the file (whole values guarded).
    pub changes: Vec<FieldChange>,
    /// What puts the file back the way it was (guarded the same way).
    pub undo: Vec<FieldChange>,
    /// The window's picture of the config after this change.
    pub original: Config,
    /// The window's picture of the file after this change.
    pub baseline: Config,
    /// An API key changed: it is stored in Credential Manager, not the file.
    pub key_changed: bool,
}

/// What `field` changed between `original` (what the window believes is applied)
/// and `edited` (the form now). `baseline` is the file as the window last saw
/// it, which a whole value (the provider order) is checked against so a hand
/// edit of the same list is never overwritten. `Ok(None)`: nothing to do.
pub(crate) fn plan(
    field: Field,
    baseline: &Config,
    original: &Config,
    edited: &Config,
) -> Result<Option<Plan>, String> {
    let own = own_changes(field, config_writer::diff(original, edited).map_err(err)?);
    let key_changed = field
        .key_vendor()
        .is_some_and(|v| key_of(original, v) != key_of(edited, v));
    if own.is_empty() && !key_changed {
        return Ok(None);
    }
    let changes = config_writer::guard_whole_values(own.clone(), baseline).map_err(err)?;
    let mut after = config_writer::reapply(original, &own).map_err(err)?;
    if let Some(v) = field.key_vendor() {
        set_key(&mut after, v, key_of(edited, v));
    }
    let after_baseline = config_writer::reapply(baseline, &own).map_err(err)?;
    let back = own_changes(field, config_writer::diff(&after, original).map_err(err)?);
    // Undo writes back only what this change wrote: a hand edit since is refused.
    let undo = config_writer::guard_every_value(back, &after_baseline).map_err(err)?;
    Ok(Some(Plan {
        changes,
        undo,
        original: after,
        baseline: after_baseline,
        key_changed,
    }))
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

/// The changes in `all` that belong to `field`.
fn own_changes(field: Field, all: Vec<FieldChange>) -> Vec<FieldChange> {
    all.into_iter().filter(|c| field.owns(c)).collect()
}

fn key_of(config: &Config, vendor: usize) -> &str {
    let p = &config.providers;
    match vendor {
        0 => &p.openai.api_key,
        1 => &p.anthropic.api_key,
        _ => &p.gemini.api_key,
    }
}

fn set_key(config: &mut Config, vendor: usize, key: &str) {
    let p = &mut config.providers;
    let slot = match vendor {
        0 => &mut p.openai.api_key,
        1 => &mut p.anthropic.api_key,
        _ => &mut p.gemini.api_key,
    };
    *slot = key.to_string();
}

/// `to` with `field` taken from `from`: the one field, nothing else. For putting
/// a field back after a failed write and for adopting a hand edit.
pub(crate) fn copy_field(field: Field, from: &Config, to: &Config) -> Result<Config, String> {
    let own = own_changes(field, config_writer::diff(to, from).map_err(err)?);
    let mut out = config_writer::reapply(to, &own).map_err(err)?;
    if let Some(v) = field.key_vendor() {
        set_key(&mut out, v, key_of(from, v));
    }
    Ok(out)
}

/// Whether `field` holds a different value in `a` and in `b`. A key counts by its
/// text.
pub(crate) fn differs(field: Field, a: &Config, b: &Config) -> bool {
    if let Some(v) = field.key_vendor() {
        return key_of(a, v) != key_of(b, v);
    }
    config_writer::diff(a, b).is_ok_and(|all| all.iter().any(|c| field.owns(c)))
}

/// Why `text` cannot be applied to `field`, or `None`. A blank box is not an
/// error: it keeps the applied value.
pub(crate) fn invalid(field: Field, text: &str) -> Option<&'static str> {
    match field {
        Field::CardSeconds => {
            let text = text.trim();
            (!text.is_empty() && text.parse::<u32>().is_err())
                .then_some("Enter a whole number of seconds, such as 8.")
        }
        Field::DoublePressMs => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            match text.parse::<u32>() {
                Ok(0) => None,
                Ok(n) if (DOUBLE_PRESS_MIN_MS..=DOUBLE_PRESS_MAX_MS).contains(&n) => None,
                _ => Some("Enter 0 to turn it off, or a number from 150 to 600."),
            }
        }
        _ => None,
    }
}

/// "Saved: Card corner".
pub(crate) fn saved_line(label: &str) -> String {
    format!("Saved: {label}")
}

/// "Undone: Card corner".
pub(crate) fn undone_line(label: &str) -> String {
    format!("Undone: {label}")
}

/// A row that cannot be applied: never colour alone, so it starts with "Error".
pub(crate) fn invalid_line(label: &str, why: &str) -> String {
    format!("Error: {label}. {why}")
}

/// The reason shown for a write that failed. An Undo the file refused (it was
/// edited since) gets its own plain sentence instead of the writer's advice to
/// save again, which would not undo anything.
pub(crate) fn failure_reason(undoing: bool, reason: &str) -> String {
    if undoing && reason.contains("was changed in config.toml") {
        "The file changed since. Undo was not applied.".to_string()
    } else {
        reason.to_string()
    }
}

/// A write that did not happen.
pub(crate) fn failed_line(label: &str, reason: &str) -> String {
    format!("Not saved: {label}. {reason} Wingman is still using the old value.")
}

/// The card shown when Settings closes with `count` settings still not saved.
pub(crate) fn unsaved_card(count: usize) -> (String, String) {
    let headline = if count == 1 {
        "Couldn't save 1 setting".to_string()
    } else {
        format!("Couldn't save {count} settings")
    };
    (
        headline,
        "Open Settings to see which, then try again. Wingman is still using the old values."
            .to_string(),
    )
}

/// What the latest change was, for Undo.
#[derive(Debug, Clone)]
pub(crate) struct UndoSlot {
    pub field: Field,
    pub what: UndoWhat,
}

#[derive(Debug, Clone)]
pub(crate) enum UndoWhat {
    /// Send these changes to the file and show `before` for the field.
    Config { restore: Vec<FieldChange> },
    /// Start with Windows was `was`.
    Autostart { was: bool },
}

/// One write handed to the writer and not yet answered.
#[derive(Debug, Clone)]
pub(crate) struct InFlight {
    pub field: Field,
    pub undoing: bool,
    /// The window's pictures before this change, to put the field back from if
    /// the write fails.
    pub before_original: Config,
    pub before_baseline: Config,
}

/// What is in flight, which rows failed, and what Undo would do.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    in_flight: Vec<InFlight>,
    failed: Vec<(Field, String)>,
    pub undo: Option<UndoSlot>,
}

impl Tracker {
    pub(crate) fn begin(&mut self, entry: InFlight) {
        self.in_flight.push(entry);
    }

    /// The writer answered for `field`: the oldest write of it that was waiting.
    pub(crate) fn finish(&mut self, field: Field) -> Option<InFlight> {
        let at = self.in_flight.iter().position(|e| e.field == field)?;
        Some(self.in_flight.remove(at))
    }

    /// A write that `finish` returned did not save. Returns whether a newer write
    /// of the same field is still waiting: then the window keeps showing the newer
    /// value (it may still save) and must not roll back past it. The waiting
    /// writes now go back to what the file really holds, the value before this one.
    pub(crate) fn settle_failed(&mut self, failed: &InFlight) -> bool {
        let mut newer = false;
        for e in self
            .in_flight
            .iter_mut()
            .filter(|e| e.field == failed.field)
        {
            newer = true;
            if let Ok(back) = copy_field(failed.field, &failed.before_original, &e.before_original)
            {
                e.before_original = back;
            }
            if let Ok(back) = copy_field(failed.field, &failed.before_baseline, &e.before_baseline)
            {
                e.before_baseline = back;
            }
        }
        newer
    }

    pub(crate) fn writes_in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// `field` did not save. One row is one failure, whatever the reason said last.
    pub(crate) fn fail(&mut self, field: Field, reason: &str) {
        self.clear_failed(field);
        self.failed.push((field, reason.to_string()));
    }

    pub(crate) fn clear_failed(&mut self, field: Field) {
        self.failed.retain(|(f, _)| *f != field);
    }

    pub(crate) fn failed_count(&self) -> usize {
        self.failed.len()
    }

    pub(crate) fn failed(&self, field: Field) -> Option<&str> {
        self.failed
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, why)| why.as_str())
    }
}

// ---------------------------------------------------------------------------
// A typed key is checked before it is written (#876)
// ---------------------------------------------------------------------------

/// What the vendor said about a typed key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyVerdict {
    /// 2xx: the vendor knows the key.
    Accepted,
    /// 401 or 403: the vendor refused it. The only verdict that blocks the write.
    Rejected,
    /// Nothing is known: no network, a timeout, 5xx, a rate limit.
    Unchecked,
}

/// What to do with a changed key of the right shape, given the mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyStep {
    /// Ask the vendor, then write.
    Check,
    /// Make no request: write it, and say it was not checked.
    SaveWithNote(&'static str),
}

/// The status line while the vendor is asked.
pub(crate) const CHECKING_LINE: &str = "Checking key...";
/// Why a key still being checked when the window closed was not saved: only
/// when the close could not claim it (its answer was already on the way).
pub(crate) const KEY_UNFINISHED: &str = "The key was not checked before the window closed.";
/// A key whose check could not finish is still saved, with this said.
pub(crate) const UNCHECKED_NOTE: &str = "Saved. Could not check it right now.";
const OFFLINE_NOTE: &str = "Saved. Not checked in Offline mode.";
const LOCAL_NOTE: &str = "Saved. Not checked in Local mode.";

/// #1130: a key still being checked when the window closes is saved unchecked,
/// like one whose check could not finish. If its vendor then refuses it
/// (401 or 403), nothing is left to show that inline, so it is a card.
/// `closed_saved` is whether the close saved the very key this verdict is for
/// (the check's claim, see `close_claims`).
pub(crate) fn late_refusal(verdict: KeyVerdict, closed_saved: bool) -> bool {
    verdict == KeyVerdict::Rejected && closed_saved
}

/// Who owns the answer of one key check: the window (the worker posts it) or
/// the close (it saved the key unchecked). One shared cell per check, settled
/// by whichever side moves it off `CLAIM_PENDING` first, so the verdict is
/// never both posted to a window that is closing and shown as a late card, nor
/// neither.
pub(crate) type Claim = std::sync::Arc<std::sync::atomic::AtomicU8>;
const CLAIM_PENDING: u8 = 0;
const CLAIM_ANSWERED: u8 = 1;
const CLAIM_CLOSED_SAVED: u8 = 2;

pub(crate) fn new_claim() -> Claim {
    Claim::new(std::sync::atomic::AtomicU8::new(CLAIM_PENDING))
}

fn settle(claim: &Claim, to: u8) -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    claim
        .compare_exchange(CLAIM_PENDING, to, SeqCst, SeqCst)
        .is_ok()
}

/// The worker has its verdict: true when it may post it to the window.
pub(crate) fn worker_claims(claim: &Claim) -> bool {
    settle(claim, CLAIM_ANSWERED)
}

/// The window is closing with this check out: true when the close may save the
/// key unchecked (the worker has not answered yet).
pub(crate) fn close_claims(claim: &Claim) -> bool {
    settle(claim, CLAIM_CLOSED_SAVED)
}

/// Whether the close saved the key this check was asked about.
pub(crate) fn closed_saved(claim: &Claim) -> bool {
    claim.load(std::sync::atomic::Ordering::SeqCst) == CLAIM_CLOSED_SAVED
}

/// The card for a key that was saved on close and then refused by its vendor.
/// The key stays saved: undoing it would need the writer, and a refused key
/// only fails requests, which then end in their own card. `vendor`: 0 OpenAI,
/// 1 Anthropic, 2 Gemini.
pub(crate) fn late_refusal_card(vendor: usize) -> (String, String) {
    let name = match vendor {
        0 => "OpenAI",
        1 => "Anthropic",
        _ => "Gemini",
    };
    (
        format!("{name} did not accept the key"),
        format!(
            "The key you typed was saved when Settings closed, but {name} refused it. Open Settings and enter a working key."
        ),
    )
}

/// Offline and Local keep every request on this computer, so a key is
/// shape-checked and saved without asking its vendor.
pub(crate) fn key_step(mode: crate::mode::Mode) -> KeyStep {
    use crate::mode::Mode;
    match mode {
        Mode::Offline => KeyStep::SaveWithNote(OFFLINE_NOTE),
        Mode::Local => KeyStep::SaveWithNote(LOCAL_NOTE),
        _ => KeyStep::Check,
    }
}

/// What the status line says after a key was saved, when it is more than "Saved".
pub(crate) fn verdict_note(verdict: KeyVerdict) -> Option<&'static str> {
    (verdict == KeyVerdict::Unchecked).then_some(UNCHECKED_NOTE)
}

/// Whether the key must not be written.
pub(crate) fn verdict_blocks(verdict: KeyVerdict) -> bool {
    verdict == KeyVerdict::Rejected
}

/// The row's error for a refused key. `vendor`: 0 OpenAI, 1 Anthropic, 2 Gemini.
pub(crate) fn rejected_reason(vendor: usize) -> String {
    let name = match vendor {
        0 => "OpenAI",
        1 => "Anthropic",
        _ => "Gemini",
    };
    format!("{name} did not accept that key. Check it and try again.")
}

/// A verdict and the check it answers, as one message parameter. The generation
/// takes all but the low two bits.
pub(crate) fn encode_verdict(generation: u64, verdict: KeyVerdict) -> isize {
    let code = match verdict {
        KeyVerdict::Accepted => 0,
        KeyVerdict::Rejected => 1,
        KeyVerdict::Unchecked => 2,
    };
    ((generation << 2) | code) as isize
}

pub(crate) fn decode_verdict(param: isize) -> Option<(u64, KeyVerdict)> {
    let bits = param as u64;
    let verdict = match bits & 3 {
        0 => KeyVerdict::Accepted,
        1 => KeyVerdict::Rejected,
        2 => KeyVerdict::Unchecked,
        _ => return None,
    };
    Some((bits >> 2, verdict))
}

/// The key checks waiting on their vendor, one per vendor at most.
#[derive(Debug, Default)]
pub(crate) struct KeyChecks {
    last: u64,
    pending: [Option<PendingCheck>; 3],
}

#[derive(Debug)]
struct PendingCheck {
    generation: u64,
    key: String,
    claim: Claim,
}

impl KeyChecks {
    /// Starts a check of `key` for `vendor` and returns its number, or `None`
    /// when that same key is already being checked (Enter after leaving the box).
    /// A different key replaces the one waiting: the older answer is then stale.
    pub(crate) fn begin(&mut self, vendor: usize, key: &str) -> Option<u64> {
        let key = key.trim();
        let slot = self.pending.get_mut(vendor)?;
        if slot.as_ref().is_some_and(|p| p.key == key) {
            return None;
        }
        self.last += 1;
        *slot = Some(PendingCheck {
            generation: self.last,
            key: key.to_string(),
            claim: new_claim(),
        });
        Some(self.last)
    }

    /// The claim of the check now waiting for `vendor`.
    pub(crate) fn claim_of(&self, vendor: usize) -> Option<Claim> {
        self.pending.get(vendor)?.as_ref().map(|p| p.claim.clone())
    }

    /// The checks waiting, with the key each was asked about, left in place.
    pub(crate) fn snapshot(&self) -> Vec<(usize, String, Claim)> {
        self.pending
            .iter()
            .enumerate()
            .filter_map(|(v, p)| p.as_ref().map(|p| (v, p.key.clone(), p.claim.clone())))
            .collect()
    }

    /// Forgets the check waiting for `vendor` (the close has dealt with it).
    pub(crate) fn forget(&mut self, vendor: usize) {
        if let Some(slot) = self.pending.get_mut(vendor) {
            *slot = None;
        }
    }

    pub(crate) fn is_pending(&self, vendor: usize) -> bool {
        self.pending.get(vendor).is_some_and(Option::is_some)
    }

    /// An answer came for check `generation`. `typed_now` is what the box holds.
    /// True when it still counts: it is the check waiting and the user has not
    /// typed another key since. Otherwise it is ignored; if the text changed,
    /// nothing is left waiting for it.
    pub(crate) fn finish(&mut self, vendor: usize, generation: u64, typed_now: &str) -> bool {
        let Some(slot) = self.pending.get_mut(vendor) else {
            return false;
        };
        let Some(pending) = slot.as_ref().filter(|p| p.generation == generation) else {
            return false;
        };
        let same = pending.key == typed_now.trim();
        *slot = None;
        same
    }

    /// The vendors still being checked, now forgotten (the window is destroyed).
    pub(crate) fn take_pending(&mut self) -> Vec<usize> {
        let mut vendors = Vec::new();
        for (vendor, slot) in self.pending.iter_mut().enumerate() {
            if slot.take().is_some() {
                vendors.push(vendor);
            }
        }
        vendors
    }
}

/// What to say when each key write in flight is answered: one entry per write
/// sent, in order, so the answer for a field finds its own.
#[derive(Debug, Default)]
pub(crate) struct KeyNotes(Vec<(Field, Option<&'static str>)>);

impl KeyNotes {
    pub(crate) fn push(&mut self, field: Field, note: Option<&'static str>) {
        self.0.push((field, note));
    }

    /// The note of the oldest write of `field` still waiting, if it had one.
    pub(crate) fn take(&mut self, field: Field) -> Option<&'static str> {
        let at = self.0.iter().position(|(f, _)| *f == field)?;
        self.0.remove(at).1
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::{CardCorner, RequireTickFor};
    use crate::hotkey::Chord;

    /// A config with `field` changed to something else, for the field tests.
    pub(crate) fn changed(field: Field, base: &Config) -> Config {
        let mut c = base.clone();
        match field {
            Field::ActiveProvider => c.providers.order = vec!["gemini".into()],
            Field::OpenAiKey => c.providers.openai.api_key = "sk-new".into(),
            Field::OpenAiModel => c.providers.openai.model = "gpt-test".into(),
            Field::OpenAiEffort => c.providers.openai.effort = "high".into(),
            Field::AnthropicKey => c.providers.anthropic.api_key = "sk-ant-new".into(),
            Field::AnthropicModel => c.providers.anthropic.model = "claude-test".into(),
            Field::AnthropicEffort => c.providers.anthropic.effort = "high".into(),
            Field::GeminiKey => c.providers.gemini.api_key = "AIzaNew".into(),
            Field::GeminiModel => c.providers.gemini.model = "gemini-test".into(),
            Field::GeminiEffort => c.providers.gemini.effort = "high".into(),
            Field::OllamaModel => c.providers.ollama.model = "llama-test".into(),
            Field::PrimaryKey => c.hotkeys.primary = chord(0x70),
            Field::SecondaryKey => c.hotkeys.secondary = chord(0x71),
            Field::PauseKey => c.hotkeys.pause = Some(chord(0x72)),
            Field::KeyMode => c.hotkeys.key_mode = "close".into(),
            Field::DoublePressMs => c.hotkeys.double_press_ms = 450,
            Field::ClickAway => c.options.click_away_dismiss = !base.options.click_away_dismiss,
            Field::Autostart => {}
            Field::CardSeconds => c.ui.card_seconds = base.ui.card_seconds + 7,
            Field::TextScale => c.ui.text_scale = 1.75,
            Field::CardCorner => c.ui.card_corner = CardCorner::TopLeft,
            Field::CardMonitor => c.ui.card_monitor = "2".into(),
            Field::MaxEdge => c.capture.max_edge = 777,
            Field::Monitor => c.capture.monitor = "primary".into(),
            Field::Redact => c.capture.redact = !base.capture.redact,
            Field::RequireTickFor => c.forms.require_tick_for = RequireTickFor::None,
            Field::Prompt => c.ui.prompt = "Check it twice.".into(),
        }
        c
    }

    fn chord(vk: u32) -> Chord {
        Chord {
            vk,
            ctrl: true,
            shift: false,
            alt: false,
            win: false,
        }
    }

    fn paths_of(changes: &[FieldChange]) -> Vec<String> {
        changes.iter().map(|c| c.path().join(".")).collect()
    }

    #[test]
    fn labels_are_unique_plain_and_found_again() {
        let mut seen = std::collections::HashSet::new();
        for f in Field::ALL {
            assert!(seen.insert(f.label()), "{} is used twice", f.label());
            assert_eq!(Field::from_label(f.label()), Some(f));
            assert!(!f.label().contains('\u{2014}'), "{}", f.label());
        }
        assert_eq!(Field::from_label("Nothing like it"), None);
    }

    /// Every field owns exactly the keys its change touches, so a commit sends
    /// that field and no other.
    #[test]
    fn each_field_owns_the_keys_it_changes() {
        let base = Config::default();
        for f in Field::ALL {
            let after = changed(f, &base);
            let diff = config_writer::diff(&base, &after).unwrap();
            if f.paths().is_empty() {
                assert!(
                    diff.is_empty(),
                    "{f:?} is not in the file but changed {diff:?}"
                );
                continue;
            }
            assert!(!diff.is_empty(), "{f:?} changed nothing: the test is wrong");
            for change in &diff {
                assert!(
                    f.owns(change),
                    "{f:?} does not own {}",
                    change.path().join(".")
                );
            }
            for other in Field::ALL.into_iter().filter(|o| *o != f) {
                assert!(
                    !diff.iter().any(|c| other.owns(c)),
                    "{other:?} also owns what {f:?} changes"
                );
            }
        }
    }

    #[test]
    fn a_plan_carries_only_its_own_field() {
        let base = Config::default();
        // The form also holds a stale card corner the user has not committed.
        let mut edited = changed(Field::CardSeconds, &base);
        edited.ui.card_corner = CardCorner::TopLeft;
        let plan = plan(Field::CardSeconds, &base, &base, &edited)
            .unwrap()
            .expect("the seconds changed");
        assert_eq!(paths_of(&plan.changes), ["ui.card_seconds"]);
        assert_eq!(plan.original.ui.card_seconds, edited.ui.card_seconds);
        assert_eq!(plan.original.ui.card_corner, base.ui.card_corner);
        assert_eq!(paths_of(&plan.undo), ["ui.card_seconds"]);
        assert!(!plan.key_changed);
    }

    /// #393: the three key settings each own one key of the file, and a plan for
    /// one never carries another.
    #[test]
    fn the_key_settings_own_their_own_keys() {
        let base = Config::default();
        for (field, key) in [
            (Field::KeyMode, "hotkeys.key_mode"),
            (Field::DoublePressMs, "hotkeys.double_press_ms"),
            (Field::ClickAway, "options.click_away_dismiss"),
        ] {
            let mut edited = changed(field, &base);
            // The form holds an uncommitted change to the other two as well.
            for other in [Field::KeyMode, Field::DoublePressMs, Field::ClickAway] {
                if other != field {
                    edited = changed(other, &edited);
                }
            }
            let plan = plan(field, &base, &base, &edited)
                .unwrap()
                .expect("the setting changed");
            assert_eq!(paths_of(&plan.changes), [key]);
            assert_eq!(paths_of(&plan.undo), [key]);
        }
    }

    /// #393: a hand-edited key mode or press time the window cannot show is never
    /// rewritten by changing another row.
    #[test]
    fn a_hand_edited_key_mode_is_left_alone_by_another_row() {
        let mut base = Config::default();
        base.hotkeys.key_mode = "PICK ".into();
        base.hotkeys.double_press_ms = 5000;
        let edited = changed(Field::CardCorner, &base);
        let plan_for = |f| plan(f, &base, &base, &edited).unwrap();
        assert_eq!(
            paths_of(
                &plan_for(Field::CardCorner)
                    .expect("the corner changed")
                    .changes
            ),
            ["ui.card_corner"]
        );
        assert!(plan_for(Field::KeyMode).is_none());
        assert!(plan_for(Field::DoublePressMs).is_none());
    }

    #[test]
    fn the_press_time_box_accepts_off_and_the_honoured_range_only() {
        for ok in ["", "  ", "0", "150", "300", " 600 "] {
            assert_eq!(invalid(Field::DoublePressMs, ok), None, "{ok:?}");
        }
        for bad in [
            "1",
            "149",
            "601",
            "99999",
            "-5",
            "3.5",
            "abc",
            "99999999999",
        ] {
            assert!(invalid(Field::DoublePressMs, bad).is_some(), "{bad:?}");
        }
        let why = invalid(Field::DoublePressMs, "x").unwrap();
        assert!(!why.contains('\u{2014}'));
        assert!(Field::DoublePressMs.is_text_box());
        assert!(!Field::KeyMode.is_text_box() && !Field::ClickAway.is_text_box());
    }

    #[test]
    fn a_plan_for_an_unchanged_field_is_none() {
        let base = Config::default();
        let edited = changed(Field::CardCorner, &base);
        assert!(plan(Field::CardSeconds, &base, &base, &edited)
            .unwrap()
            .is_none());
        assert!(plan(Field::CardCorner, &base, &base, &base)
            .unwrap()
            .is_none());
    }

    #[test]
    fn every_field_plans_its_own_change() {
        let base = Config::default();
        for f in Field::ALL.into_iter().filter(|f| *f != Field::Autostart) {
            let edited = changed(f, &base);
            let plan = plan(f, &base, &base, &edited)
                .unwrap()
                .unwrap_or_else(|| panic!("{f:?} planned nothing"));
            assert_eq!(plan.key_changed, f.key_vendor().is_some(), "{f:?}");
            assert_eq!(plan.changes.is_empty(), f.key_vendor().is_some(), "{f:?}");
            // After the plan the window agrees with the form for this field.
            let again = super::plan(f, &plan.baseline, &plan.original, &edited).unwrap();
            assert!(again.is_none(), "{f:?} would send itself twice");
        }
    }

    /// A key never goes into the file, but its change is still a plan, so the
    /// keys are stored.
    #[test]
    fn a_key_plan_has_no_file_change_and_marks_the_key() {
        let base = Config::default();
        let edited = changed(Field::OpenAiKey, &base);
        let plan = plan(Field::OpenAiKey, &base, &base, &edited)
            .unwrap()
            .expect("the key changed");
        assert!(plan.changes.is_empty());
        assert!(plan.key_changed);
        assert_eq!(plan.original.providers.openai.api_key, "sk-new");
        assert_eq!(
            plan.original.providers.anthropic.api_key,
            base.providers.anthropic.api_key
        );
    }

    /// The provider order is a whole list: it is written only if the file still
    /// holds the list the window saw.
    #[test]
    fn the_provider_order_is_guarded_against_a_hand_edit() {
        let mut base = Config::default();
        base.providers.order = vec!["openai".into(), "anthropic".into()];
        let edited = changed(Field::ActiveProvider, &base);
        let plan = plan(Field::ActiveProvider, &base, &base, &edited)
            .unwrap()
            .expect("the order changed");
        match &plan.changes[..] {
            [FieldChange::SetIfUnchanged { expected, .. }] => {
                let expected = expected.as_ref().expect("the file's list is expected");
                assert_eq!(expected.as_array().map(Vec::len), Some(2));
            }
            other => panic!("the list was not guarded: {other:?}"),
        }
        match &plan.undo[..] {
            [FieldChange::SetIfUnchanged { expected, .. }] => {
                assert_eq!(
                    expected.as_ref().and_then(|v| v.as_array()).map(Vec::len),
                    Some(1)
                );
            }
            other => panic!("the undo was not guarded: {other:?}"),
        }
    }

    #[test]
    fn a_hand_edited_value_in_the_file_is_not_this_fields_to_rewrite() {
        // The file (baseline) differs from the window's picture in another key:
        // it is not in the plan, so it is never written over.
        let mut baseline = Config::default();
        baseline.ui.card_corner = CardCorner::TopRight;
        let base = Config::default();
        let edited = changed(Field::CardSeconds, &base);
        let plan = plan(Field::CardSeconds, &baseline, &base, &edited)
            .unwrap()
            .unwrap();
        assert_eq!(paths_of(&plan.changes), ["ui.card_seconds"]);
        assert_eq!(plan.baseline.ui.card_corner, CardCorner::TopRight);
    }

    #[test]
    fn undo_restores_the_old_value() {
        let base = Config::default();
        let edited = changed(Field::CardCorner, &base);
        let plan = plan(Field::CardCorner, &base, &base, &edited)
            .unwrap()
            .unwrap();
        let back = config_writer::reapply(&plan.original, &plan.undo).unwrap();
        assert_eq!(back.ui.card_corner, base.ui.card_corner);
    }

    #[test]
    fn copy_field_copies_one_field_and_nothing_else() {
        let base = Config::default();
        let mut from = changed(Field::CardCorner, &base);
        from.ui.card_seconds = 99;
        let to = copy_field(Field::CardCorner, &from, &base).unwrap();
        assert_eq!(to.ui.card_corner, from.ui.card_corner);
        assert_eq!(to.ui.card_seconds, base.ui.card_seconds);
        // A key is copied too, though it is not in the file.
        let from = changed(Field::GeminiKey, &base);
        let to = copy_field(Field::GeminiKey, &from, &base).unwrap();
        assert_eq!(to.providers.gemini.api_key, "AIzaNew");
        // A removed optional key (the pause shortcut) is removed.
        let mut with_pause = base.clone();
        with_pause.hotkeys.pause = Some(chord(0x72));
        let to = copy_field(Field::PauseKey, &base, &with_pause).unwrap();
        assert_eq!(to.hotkeys.pause, None);
    }

    #[test]
    fn differs_looks_at_one_field_only() {
        let base = Config::default();
        let mut other = changed(Field::CardCorner, &base);
        other.ui.card_seconds = 99;
        assert!(differs(Field::CardCorner, &base, &other));
        assert!(differs(Field::CardSeconds, &base, &other));
        assert!(!differs(Field::Redact, &base, &other));
        assert!(!differs(Field::CardCorner, &base, &base));
        let keyed = changed(Field::OpenAiKey, &base);
        assert!(differs(Field::OpenAiKey, &base, &keyed));
        assert!(!differs(Field::GeminiKey, &base, &keyed));
    }

    #[test]
    fn a_number_that_does_not_parse_is_an_error_and_a_blank_is_not() {
        assert!(invalid(Field::CardSeconds, "8").is_none());
        assert!(invalid(Field::CardSeconds, " 12 ").is_none());
        assert!(invalid(Field::CardSeconds, "").is_none());
        assert!(invalid(Field::CardSeconds, "2ooo").is_some());
        assert!(invalid(Field::CardSeconds, "-3").is_some());
        assert!(invalid(Field::CardSeconds, "4.5").is_some());
        assert!(invalid(Field::CardCorner, "anything").is_none());
        let why = invalid(Field::CardSeconds, "x").unwrap();
        assert!(!why.contains('\u{2014}'), "{why}");
    }

    #[test]
    fn a_refused_undo_says_so_in_plain_words() {
        let refusal = "`ui.card_seconds` was changed in config.toml while Settings was open, \
                       so Wingman did not overwrite it. Open Settings again and save once more";
        let shown = failure_reason(true, refusal);
        assert_eq!(shown, "The file changed since. Undo was not applied.");
        assert!(!shown.contains('\u{2014}') && !shown.contains("save once more"));
        // A normal write keeps the writer's words, and so does any other undo failure.
        assert_eq!(failure_reason(false, refusal), refusal);
        assert_eq!(failure_reason(true, "disk full"), "disk full");
    }

    #[test]
    fn status_lines_are_plain() {
        assert_eq!(saved_line("Card corner"), "Saved: Card corner");
        assert_eq!(undone_line("Card corner"), "Undone: Card corner");
        assert!(invalid_line("Hide card after", "Enter a number.").starts_with("Error"));
        let line = failed_line("Card corner", "The disk is full.");
        assert!(line.starts_with("Not saved: Card corner."), "{line}");
        assert!(line.contains("still using the old value"), "{line}");
        for text in [
            saved_line("x"),
            undone_line("x"),
            invalid_line("x", "y"),
            failed_line("x", "y"),
        ] {
            assert!(!text.contains('\u{2014}'), "{text}");
        }
    }

    #[test]
    fn the_close_card_counts_and_has_no_em_dash() {
        let (one, _) = unsaved_card(1);
        let (many, detail) = unsaved_card(3);
        assert_eq!(one, "Couldn't save 1 setting");
        assert_eq!(many, "Couldn't save 3 settings");
        assert!(detail.contains("Settings"), "{detail}");
        assert!(!detail.contains('\u{2014}') && !many.contains('\u{2014}'));
    }

    fn entry(field: Field, undoing: bool) -> InFlight {
        InFlight {
            field,
            undoing,
            before_original: Config::default(),
            before_baseline: Config::default(),
        }
    }

    #[test]
    fn the_tracker_matches_answers_to_writes_in_order() {
        let mut t = Tracker::default();
        t.begin(entry(Field::CardCorner, false));
        t.begin(entry(Field::CardSeconds, false));
        t.begin(entry(Field::CardCorner, true));
        assert_eq!(t.writes_in_flight(), 3);
        let first = t.finish(Field::CardCorner).expect("a write was waiting");
        assert!(
            !first.undoing,
            "the oldest write of the field answers first"
        );
        assert!(t.finish(Field::CardCorner).unwrap().undoing);
        assert!(t.finish(Field::CardCorner).is_none());
        assert_eq!(t.writes_in_flight(), 1);
    }

    #[test]
    fn the_tracker_counts_failed_rows_once_each_and_clears_them() {
        let mut t = Tracker::default();
        t.fail(Field::CardCorner, "disk full");
        t.fail(Field::CardCorner, "file locked");
        t.fail(Field::Redact, "disk full");
        assert_eq!(t.failed_count(), 2);
        assert_eq!(t.failed(Field::CardCorner), Some("file locked"));
        t.clear_failed(Field::CardCorner);
        assert_eq!(t.failed_count(), 1);
        assert_eq!(t.failed(Field::CardCorner), None);
    }

    fn write_of(field: Field, before: &Config) -> InFlight {
        InFlight {
            field,
            undoing: false,
            before_original: before.clone(),
            before_baseline: before.clone(),
        }
    }

    /// Two writes of one field are in flight and the older one fails first: the
    /// picture must keep the newer value (which may still save), not go back past it.
    #[test]
    fn an_older_failed_write_does_not_roll_back_past_a_newer_one() {
        let first_value = Config::default();
        let second_value = changed(Field::CardSeconds, &first_value);
        let third_value = {
            let mut c = second_value.clone();
            c.ui.card_seconds += 1;
            c
        };
        let mut t = Tracker::default();
        t.begin(write_of(Field::CardSeconds, &first_value));
        t.begin(write_of(Field::CardSeconds, &second_value));
        let older = t.finish(Field::CardSeconds).unwrap();
        assert!(
            t.settle_failed(&older),
            "a newer write of the field is still waiting"
        );
        // The window shows the third value (the newer write's). Nothing restores.
        let newer = t.finish(Field::CardSeconds).unwrap();
        // If the newer one fails too, it goes back to what the file really holds:
        // the value before the older write, not the older write's value.
        assert_eq!(
            newer.before_original.ui.card_seconds,
            first_value.ui.card_seconds
        );
        assert_eq!(
            newer.before_baseline.ui.card_seconds,
            first_value.ui.card_seconds
        );
        assert!(!t.settle_failed(&newer), "nothing newer is left");
        let _ = third_value;
    }

    #[test]
    fn a_failed_write_of_another_field_is_not_superseded() {
        let base = Config::default();
        let mut t = Tracker::default();
        t.begin(write_of(Field::CardSeconds, &base));
        t.begin(write_of(Field::CardCorner, &base));
        let e = t.finish(Field::CardSeconds).unwrap();
        assert!(!t.settle_failed(&e));
    }

    /// Undo puts back only what it wrote over: a hand edit made since is refused,
    /// not overwritten with the stale old value.
    #[test]
    fn undo_is_refused_when_the_file_was_edited_by_hand_since() {
        let base = Config::default();
        let edited = changed(Field::CardSeconds, &base);
        let plan = plan(Field::CardSeconds, &base, &base, &edited)
            .unwrap()
            .unwrap();
        match &plan.undo[..] {
            [FieldChange::SetIfUnchanged { expected, .. }] => {
                assert_eq!(
                    expected.as_ref().and_then(|v| v.as_integer()),
                    Some(i64::from(edited.ui.card_seconds))
                );
            }
            other => panic!("the undo is not conditional: {other:?}"),
        }
        let path =
            std::env::temp_dir().join(format!("wingman-settings-undo-{}.toml", std::process::id()));
        // The change was written, then the user edited the same line by hand.
        std::fs::write(
            &path,
            "[ui]
card_seconds = 99 # hand edit
",
        )
        .unwrap();
        let refused = config_writer::write_changes(&path, &plan.undo);
        let after = std::fs::read_to_string(&path).unwrap();
        // And with the file as the change left it, the undo goes through.
        std::fs::write(
            &path,
            format!(
                "[ui]
card_seconds = {}
",
                edited.ui.card_seconds
            ),
        )
        .unwrap();
        let allowed = config_writer::write_changes(&path, &plan.undo);
        let undone = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(refused.is_err(), "the hand edit was overwritten");
        assert!(after.contains("card_seconds = 99 # hand edit"), "{after}");
        assert!(allowed.is_ok(), "{allowed:?}");
        assert!(
            undone.contains(&format!("card_seconds = {}", base.ui.card_seconds)),
            "{undone}"
        );
    }

    // -- typed key check (#876) ----------------------------------------------

    #[test]
    fn a_key_is_checked_unless_the_mode_forbids_cloud_requests() {
        use crate::mode::Mode;
        assert_eq!(key_step(Mode::Auto), KeyStep::Check);
        assert_eq!(key_step(Mode::Cloud), KeyStep::Check);
        assert_eq!(
            key_step(Mode::Offline),
            KeyStep::SaveWithNote("Saved. Not checked in Offline mode.")
        );
        assert_eq!(
            key_step(Mode::Local),
            KeyStep::SaveWithNote("Saved. Not checked in Local mode.")
        );
    }

    #[test]
    fn only_a_refusal_blocks_and_only_an_unanswered_check_adds_a_note() {
        assert_eq!(verdict_note(KeyVerdict::Accepted), None);
        assert_eq!(
            verdict_note(KeyVerdict::Unchecked),
            Some("Saved. Could not check it right now.")
        );
        assert!(verdict_blocks(KeyVerdict::Rejected));
        assert!(!verdict_blocks(KeyVerdict::Accepted));
        assert!(!verdict_blocks(KeyVerdict::Unchecked));
    }

    #[test]
    fn the_key_messages_are_plain_and_never_carry_a_dash() {
        for vendor in 0..3 {
            let why = rejected_reason(vendor);
            assert!(why.ends_with("Check it and try again."), "{why}");
            assert!(!why.contains('\u{2014}'), "{why}");
        }
        assert!(rejected_reason(0).starts_with("OpenAI did not accept"));
        assert!(rejected_reason(1).starts_with("Anthropic did not accept"));
        assert!(rejected_reason(2).starts_with("Gemini did not accept"));
        for text in [CHECKING_LINE, "Saved. Could not check it right now."] {
            assert!(!text.contains('\u{2014}'), "{text}");
        }
    }

    /// #1130: the worker and the close settle a check's answer exactly once.
    #[test]
    fn the_worker_and_the_close_cannot_both_claim_a_check() {
        let claim = new_claim();
        assert!(close_claims(&claim));
        assert!(!worker_claims(&claim), "the close got it first");
        assert!(closed_saved(&claim));
        let claim = new_claim();
        assert!(worker_claims(&claim));
        assert!(!close_claims(&claim), "the answer is already on its way");
        assert!(!closed_saved(&claim));
    }

    /// #1130: a replaced check has its own claim, so closing with the newer key
    /// out never makes the older key's refusal a late card.
    #[test]
    fn a_replaced_check_keeps_its_own_claim() {
        let mut checks = KeyChecks::default();
        checks.begin(0, "sk-a").unwrap();
        let first = checks.claim_of(0).unwrap();
        checks.begin(0, "sk-b").unwrap();
        let second = checks.claim_of(0).unwrap();
        let snap = checks.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].1, "sk-b");
        assert!(close_claims(&snap[0].2));
        assert!(closed_saved(&second));
        assert!(!closed_saved(&first), "A's refusal must not raise a card");
        checks.forget(0);
        assert!(!checks.is_pending(0));
    }

    /// #1130: a key saved unchecked on close, then refused, is told in a card.
    #[test]
    fn a_late_refusal_is_a_card_for_a_closed_window_only() {
        assert!(late_refusal(KeyVerdict::Rejected, true));
        assert!(
            !late_refusal(KeyVerdict::Rejected, false),
            "the window tells it, or the key was replaced"
        );
        assert!(!late_refusal(KeyVerdict::Accepted, true));
        assert!(!late_refusal(KeyVerdict::Unchecked, true));
        for (vendor, name) in [(0, "OpenAI"), (1, "Anthropic"), (2, "Gemini")] {
            let (headline, detail) = late_refusal_card(vendor);
            assert!(headline.contains(name), "{headline}");
            assert!(detail.contains("Settings"), "{detail}");
            assert!(!format!("{headline}{detail}").contains('\u{2014}'));
        }
    }

    #[test]
    fn a_verdict_survives_the_trip_through_a_message() {
        for v in [
            KeyVerdict::Accepted,
            KeyVerdict::Rejected,
            KeyVerdict::Unchecked,
        ] {
            for generation in [1u64, 2, 977, u64::MAX >> 3] {
                assert_eq!(
                    decode_verdict(encode_verdict(generation, v)),
                    Some((generation, v))
                );
            }
        }
        assert_eq!(decode_verdict(3), None, "an unknown code is ignored");
    }

    #[test]
    fn the_same_key_is_not_checked_twice_at_once() {
        let mut checks = KeyChecks::default();
        let first = checks.begin(0, "sk-one").expect("starts");
        assert!(checks.is_pending(0));
        assert_eq!(checks.begin(0, "sk-one"), None, "Enter after leaving");
        assert_eq!(checks.begin(0, " sk-one "), None, "spaces do not matter");
        // Another vendor is its own check.
        assert!(checks.begin(1, "sk-ant-x").is_some());
        // A different key replaces it: the first answer is then stale.
        let second = checks.begin(0, "sk-two").expect("restarts");
        assert_ne!(first, second);
        assert!(
            !checks.finish(0, first, "sk-two"),
            "the old answer is ignored"
        );
        assert!(checks.is_pending(0), "the new check is still waiting");
        assert!(checks.finish(0, second, "sk-two"));
        assert!(!checks.is_pending(0));
    }

    #[test]
    fn an_answer_for_text_the_user_has_since_changed_is_ignored() {
        let mut checks = KeyChecks::default();
        let generation = checks.begin(2, "AIzaOld").unwrap();
        assert!(!checks.finish(2, generation, "AIzaOldPlusMore"));
        assert!(!checks.is_pending(2), "nothing is left waiting for it");
        // And an answer nobody asked for is ignored too.
        assert!(!checks.finish(2, generation, "AIzaOld"));
    }

    #[test]
    fn closing_with_a_check_running_names_the_keys_left_unsaved() {
        let mut checks = KeyChecks::default();
        checks.begin(0, "sk-a").unwrap();
        checks.begin(2, "AIzaB").unwrap();
        assert_eq!(checks.take_pending(), vec![0, 2]);
        assert!(!checks.is_pending(0) && !checks.is_pending(2));
        assert!(checks.take_pending().is_empty());
    }

    #[test]
    fn a_note_is_taken_once_in_the_order_the_writes_were_sent() {
        let mut notes = KeyNotes::default();
        notes.push(Field::OpenAiKey, Some("first"));
        notes.push(Field::OpenAiKey, None);
        notes.push(Field::GeminiKey, Some("gemini"));
        assert_eq!(notes.take(Field::OpenAiKey), Some("first"));
        assert_eq!(notes.take(Field::OpenAiKey), None);
        assert_eq!(notes.take(Field::OpenAiKey), None);
        assert_eq!(notes.take(Field::GeminiKey), Some("gemini"));
    }
}
