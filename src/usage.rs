//! Token accounting and spend (#21): a local price table, a pure cost
//! function, and per-day totals that the tray tooltip reads.
//!
//! # Where it is called from
//!
//! Every provider's `complete` calls [`record`] with the usage block it just
//! parsed, so a new provider that calls it gets counted for free. The tray
//! tooltip (`App::update_tooltip`) reads [`tooltip_part`].
//!
//! # The price table
//!
//! [`PRICES`] is the one place prices live. Each row names where the number
//! came from and the date it was checked (AGENTS.md rule 10). A model with
//! no row is never guessed: it is counted as "unpriced" so the tooltip says
//! the total is a floor. To update a price, edit its row and the `CHECKED`
//! date. When a vendor announces a future change, add a second row for the
//! same model with a later `from` date: the newest row whose `from` is on or
//! before the day of the request wins.
//!
//! Known gap, which makes the estimate an upper bound rather than a guess:
//! long-prompt surcharges (over 200k or 272k tokens) are ignored because a
//! screenshot press is nowhere near them. Cached input is priced at each
//! provider's cached rate (OpenAI `cached_tokens`, Gemini
//! `cachedContentTokenCount`, Anthropic cache read).
//!
//! Totals are a floor: only successful, parsed responses are counted, so a
//! refused or failed call that the vendor still billed is not included. The
//! tooltip refreshes only after a request finishes (no timer, AGENTS.md
//! rule 5), so it can show yesterday's total until the first press of a day.
//!
//! # Where the totals live
//!
//! The `usage_day` table of the local store (`crate::store`, issue #46),
//! one row per local date, provider and model. Only dollars, counts and
//! token totals are stored, never prompts or answers. The old
//! `spend.json` is imported once and renamed `spend.json.migrated`.
//!
//! # The daily budget (#101)
//!
//! `[budget] daily_usd` in config.toml is mirrored here by
//! [`set_daily_budget`] (the request path has no `Config` in reach, same as
//! the Offline mode and egress preview mirrors). `provider::common` calls
//! [`check_budget`] before every request that leaves this machine, so a
//! provider added later cannot skip it. Past the limit, billed requests are
//! refused with a card-ready message and the chain falls through to a local
//! provider if it has one. Crossing the warning line (default 80%) or the
//! limit queues one notice per day for the next card ([`take_budget_notice`]).
//! The check reads the same `usage_day` totals the tooltip shows, so unpriced
//! requests are not counted: the budget is a floor, like the total.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::provider::{Completion, Usage};

/// US dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    /// Anthropic prompt-cache write (5 minute) rate. Others: the input rate.
    pub cache_write: f64,
    /// Prompt-cache read rate (Anthropic cache read, OpenAI cached input,
    /// Gemini context caching). The input rate when the page lists none.
    pub cache_read: f64,
}

// `source` and `checked` are provenance for the person editing the table;
// only the unit test reads them.
#[cfg_attr(not(test), allow(dead_code))]
struct PriceRow {
    provider: &'static str,
    model: &'static str,
    /// First day (`YYYY-MM-DD`, local) this row applies.
    from: &'static str,
    price: Price,
    /// Where the number came from.
    source: &'static str,
    /// When it was last compared with that source.
    checked: &'static str,
}

const fn p(input: f64, output: f64, cache_write: f64, cache_read: f64) -> Price {
    Price {
        input,
        output,
        cache_write,
        cache_read,
    }
}

const fn row(
    provider: &'static str,
    model: &'static str,
    from: &'static str,
    price: Price,
    source: &'static str,
) -> PriceRow {
    PriceRow {
        provider,
        model,
        from,
        price,
        source,
        checked: CHECKED,
    }
}

const ANTHROPIC_SRC: &str = "https://platform.claude.com/docs/en/about-claude/pricing";
const OPENAI_SRC: &str = "https://developers.openai.com/api/docs/pricing";
const GEMINI_SRC: &str = "https://ai.google.dev/gemini-api/docs/pricing";
/// MEASURED 2026-10-15: every row below was read from its `source` page on
/// this date. Never edit a number without updating this.
const CHECKED: &str = "2026-10-15";
const ALWAYS: &str = "0000-01-01";

const PRICES: &[PriceRow] = &[
    // Anthropic. Cache write is the 5 minute rate.
    row(
        "anthropic",
        "claude-opus-5",
        ALWAYS,
        p(5.0, 25.0, 6.25, 0.50),
        ANTHROPIC_SRC,
    ),
    row(
        "anthropic",
        "claude-sonnet-5",
        ALWAYS,
        p(2.0, 10.0, 2.50, 0.20),
        ANTHROPIC_SRC,
    ),
    row(
        "anthropic",
        "claude-opus-4-8",
        ALWAYS,
        p(5.0, 25.0, 6.25, 0.50),
        ANTHROPIC_SRC,
    ),
    row(
        "anthropic",
        "claude-haiku-4-5",
        ALWAYS,
        p(1.0, 5.0, 1.25, 0.10),
        ANTHROPIC_SRC,
    ),
    // Fable 5.1 cache reads are 0.025x input (the page's own footnote 1), not
    // the 0.1x of the older rows: 10.0 * 0.025 = 0.25. Re-read 2026-10-15.
    row(
        "anthropic",
        "claude-fable-5-1",
        ALWAYS,
        p(10.0, 50.0, 12.50, 0.25),
        ANTHROPIC_SRC,
    ),
    // OpenAI (standard tier, up to 272k input tokens).
    row(
        "openai",
        "gpt-5.5",
        ALWAYS,
        p(5.0, 30.0, 5.0, 0.50),
        OPENAI_SRC,
    ),
    // No cached rate is listed for the Pro model: cached reads bill as input.
    row(
        "openai",
        "gpt-5.5-pro",
        ALWAYS,
        p(30.0, 180.0, 30.0, 30.0),
        OPENAI_SRC,
    ),
    row(
        "openai",
        "gpt-5.4",
        ALWAYS,
        p(2.5, 15.0, 2.5, 0.25),
        OPENAI_SRC,
    ),
    row(
        "openai",
        "gpt-5.4-mini",
        ALWAYS,
        p(0.75, 4.5, 0.75, 0.075),
        OPENAI_SRC,
    ),
    row(
        "openai",
        "gpt-5.4-nano",
        ALWAYS,
        p(0.20, 1.25, 0.20, 0.02),
        OPENAI_SRC,
    ),
    row(
        "openai",
        "gpt-5.2",
        ALWAYS,
        p(1.75, 14.0, 1.75, 0.175),
        OPENAI_SRC,
    ),
    row(
        "openai",
        "gpt-5.1",
        ALWAYS,
        p(1.25, 10.0, 1.25, 0.125),
        OPENAI_SRC,
    ),
    row(
        "openai",
        "gpt-5",
        ALWAYS,
        p(1.25, 10.0, 1.25, 0.125),
        OPENAI_SRC,
    ),
    row(
        "openai",
        "gpt-5-mini",
        ALWAYS,
        p(0.25, 2.0, 0.25, 0.025),
        OPENAI_SRC,
    ),
    row(
        "openai",
        "gpt-4.1",
        ALWAYS,
        p(2.0, 8.0, 2.0, 0.50),
        OPENAI_SRC,
    ),
    // Gemini context-caching rates (the last number in each row) are the
    // "Context caching" column of the pricing page, MEASURED 2026-10-15:
    // 3.8 Flash $0.075 (then $0.15 from 2027-01-01), 3.1 Pro $0.20, 2.5 Pro
    // $0.125, 2.5 Flash $0.03. Storage fees are not counted.
    // Gemini (output includes thinking tokens). The 3.8 Flash page lists an
    // introductory rate through 2026-12-31 and double from 2027-01-01
    // (MEASURED 2026-10-15: the page says "$0.75 through December 31, 2026"
    // and "$1.50 starting January 1, 2027"; output $3.75 then $7.50).
    row(
        "gemini",
        "gemini-3.8-flash",
        ALWAYS,
        p(0.75, 3.75, 0.75, 0.075),
        GEMINI_SRC,
    ),
    row(
        "gemini",
        "gemini-3.8-flash",
        "2027-01-01",
        p(1.50, 7.50, 1.50, 0.15),
        GEMINI_SRC,
    ),
    row(
        "gemini",
        "gemini-3.1-pro-preview",
        ALWAYS,
        p(2.0, 12.0, 2.0, 0.20),
        GEMINI_SRC,
    ),
    row(
        "gemini",
        "gemini-2.5-pro",
        ALWAYS,
        p(1.25, 10.0, 1.25, 0.125),
        GEMINI_SRC,
    ),
    row(
        "gemini",
        "gemini-2.5-flash",
        ALWAYS,
        p(0.30, 2.5, 0.30, 0.03),
        GEMINI_SRC,
    ),
    // THEORY (unverified): "gemini-3.5-flash" is in the default model list
    // but the Gemini pricing page lists no such model on 2026-10-15 (it
    // lists 3.6 Flash). Left unpriced on purpose, so it shows as unpriced.
];

/// `rest` is a dated snapshot suffix such as `-20251001`.
fn is_date_suffix(rest: &str) -> bool {
    rest.strip_prefix('-')
        .is_some_and(|d| d.len() >= 6 && d.bytes().all(|b| b.is_ascii_digit()))
}

/// The price for `model` on `date`, or `None` when the table has no row.
pub fn price_for(provider: &str, model: &str, date: &str) -> Option<Price> {
    PRICES
        .iter()
        .filter(|r| r.provider == provider && r.from <= date)
        .filter(|r| model == r.model || model.strip_prefix(r.model).is_some_and(is_date_suffix))
        .max_by_key(|r| (r.model.len(), r.from))
        .map(|r| r.price)
}

/// Dollars for one response, or `None` when the model has no price.
/// Ollama runs on the user's own machine: always free.
pub fn cost_usd(provider: &str, model: &str, date: &str, usage: &Usage) -> Option<f64> {
    if provider.starts_with("ollama") {
        return Some(0.0);
    }
    let price = price_for(provider, model, date)?;
    let per_million = |tokens: u32, rate: f64| f64::from(tokens) * rate / 1_000_000.0;
    Some(
        per_million(usage.input_tokens, price.input)
            + per_million(usage.output_tokens, price.output)
            + per_million(usage.cache_write_tokens, price.cache_write)
            + per_million(usage.cache_read_tokens, price.cache_read),
    )
}

/// Whether `base_url` points at this machine (a loopback host). A
/// compat endpoint on loopback (LM Studio, llama.cpp) costs nothing.
pub fn is_loopback_url(base_url: &str) -> bool {
    let rest = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(inner) = host_port.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    host == "localhost"
        || host == "::1"
        || host.strip_prefix("127.").is_some_and(|r| {
            r.split('.').count() == 3 && r.split('.').all(|o| o.parse::<u8>().is_ok())
        })
}

/// The cost of one request. A request that ran on this machine (`local`) is
/// free whether or not the response carried a usage block; anything else with
/// no usage block or no price row is `None` (unpriced).
pub fn request_cost(
    provider: &str,
    model: &str,
    date: &str,
    local: bool,
    usage: Option<&Usage>,
) -> Option<f64> {
    if local || provider.starts_with("ollama") {
        return Some(0.0);
    }
    cost_usd(provider, model, date, usage?)
}

/// The cost of a successful response, read at the HTTP boundary for the
/// egress log (#487). Same price table and rules as [`record`], so the log
/// row, the tooltip and the budget cannot disagree. `tag` is the provider's
/// `id()` and `url` the endpoint (a loopback URL is free).
pub fn response_cost(tag: &str, url: &str, model: Option<&str>, body: &str) -> Option<f64> {
    let usage = crate::provider::usage_from_body(tag, body);
    request_cost(
        tag,
        model.unwrap_or(""),
        &local_date_today(),
        is_loopback_url(url),
        usage.as_ref(),
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DayTotal {
    pub usd: f64,
    pub requests: u32,
    /// Requests whose model had no price row, or that carried no usage block.
    pub unpriced: u32,
}

/// The tooltip fragment for `day`, or `None` when there is nothing to show
/// (no requests, or only free local ones).
pub fn format_tooltip_part(day: &DayTotal) -> Option<String> {
    if day.usd <= 0.0 && day.unpriced == 0 {
        return None;
    }
    let plus = if day.unpriced > 0 { "+" } else { "" };
    let amount = if day.usd > 0.0 && day.usd < 0.005 {
        "<$0.01".to_string()
    } else {
        format!("${:.2}{plus}", day.usd)
    };
    let tail = if day.unpriced > 0 {
        format!(" ({} unpriced)", day.unpriced)
    } else {
        String::new()
    };
    Some(format!("{amount} today{tail}"))
}

/// Today's date in local time, `YYYY-MM-DD`.
fn local_date_today() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay)
}

/// The configured daily budget (#101).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    pub limit_usd: f64,
    /// Percent of the limit at which the warning notice appears (1 to 100).
    pub warn_percent: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BudgetState {
    Under,
    Warn,
    Exhausted,
}

static BUDGET: Mutex<Option<Budget>> = Mutex::new(None);
static BUDGET_NOTICE: Mutex<Option<String>> = Mutex::new(None);
/// The local date and the highest state a notice was already queued for.
static NOTIFIED: Mutex<Option<(String, BudgetState)>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Publishes `[budget]` for the request path. A limit that is not a positive
/// number turns the budget off. Called at startup and on every config reload.
pub fn set_daily_budget(daily_usd: f64, warn_percent: u8) {
    *lock(&BUDGET) = budget_from(daily_usd, warn_percent);
}

fn budget_from(daily_usd: f64, warn_percent: u8) -> Option<Budget> {
    (daily_usd.is_finite() && daily_usd > 0.0).then_some(Budget {
        limit_usd: daily_usd,
        warn_percent: warn_percent.clamp(1, 100),
    })
}

pub fn budget_state(spent: f64, b: &Budget) -> BudgetState {
    // A hair of slack so 0.05 of 0.05 is not lost to float error.
    let spent = spent + 1e-9;
    if spent >= b.limit_usd {
        BudgetState::Exhausted
    } else if spent >= b.limit_usd * f64::from(b.warn_percent) / 100.0 {
        BudgetState::Warn
    } else {
        BudgetState::Under
    }
}

fn exhausted_message(spent: f64, b: &Budget) -> String {
    format!(
        "Daily budget of ${:.2} reached (${spent:.2} spent today). Cloud requests are paused until tomorrow, and local models still work. Raise daily_usd under [budget] in config.toml, or set it to 0, to continue.",
        b.limit_usd
    )
}

fn warning_message(spent: f64, b: &Budget) -> String {
    format!(
        "You have used ${spent:.2} of your ${:.2} daily budget. Cloud requests stop at the limit.",
        b.limit_usd
    )
}

/// Ok while billed requests are allowed, else the card text.
pub fn check_budget_with(b: &Budget, spent: f64) -> Result<(), String> {
    match budget_state(spent, b) {
        BudgetState::Exhausted => Err(exhausted_message(spent, b)),
        _ => Ok(()),
    }
}

/// What `provider::common` asks before a request leaves this machine. Fails
/// open when the store cannot be read: accounting is never why an answer fails.
pub fn check_budget() -> Result<(), String> {
    let Some(b) = *lock(&BUDGET) else {
        return Ok(());
    };
    match spent_today() {
        Some(spent) => check_budget_with(&b, spent),
        None => Ok(()),
    }
}

#[cfg(test)]
pub(crate) static SPENT_OVERRIDE: Mutex<Option<f64>> = Mutex::new(None);

fn spent_today() -> Option<f64> {
    #[cfg(test)]
    if let Some(v) = *lock(&SPENT_OVERRIDE) {
        return Some(v);
    }
    let day = crate::store::global()
        .and_then(|s| s.day_total(&local_date_today()))
        .ok()?;
    Some(day.map_or(0.0, |d| d.usd))
}

/// The notice to queue now, if the day's spend just reached a level no notice
/// was queued for yet. Pure apart from `notified`.
fn budget_notice_for(
    spent: f64,
    b: &Budget,
    date: &str,
    notified: &mut Option<(String, BudgetState)>,
) -> Option<String> {
    let state = budget_state(spent, b);
    if state == BudgetState::Under {
        return None;
    }
    let seen = match notified {
        Some((d, s)) if d == date => *s,
        _ => BudgetState::Under,
    };
    if state <= seen {
        return None;
    }
    *notified = Some((date.to_string(), state));
    Some(match state {
        BudgetState::Exhausted => exhausted_message(spent, b),
        _ => warning_message(spent, b),
    })
}

/// Takes the pending budget notice, if any. `app.rs` adds it to the next card.
pub fn take_budget_notice() -> Option<String> {
    lock(&BUDGET_NOTICE).take()
}

fn queue_budget_notice(date: &str) {
    let Some(b) = *lock(&BUDGET) else { return };
    let Some(spent) = spent_today() else { return };
    if let Some(text) = budget_notice_for(spent, &b, date, &mut lock(&NOTIFIED)) {
        *lock(&BUDGET_NOTICE) = Some(text);
    }
}

/// Counts one provider response toward today's total. Best effort: a failure
/// to write is swallowed (rule 7: accounting must never be why an answer
/// fails). A response with no usage block counts as unpriced.
pub fn record(provider: &str, model: &str, local: bool, usage: Option<Usage>) {
    let date = local_date_today();
    let cost = request_cost(provider, model, &date, local, usage.as_ref());
    let saved = crate::store::global()
        .and_then(|s| s.record_usage(&date, provider, model, cost, usage.as_ref()));
    if let Err(e) = saved {
        // Never fails the request; the next card says so, once per run.
        crate::store::report(&e);
    }
    queue_budget_notice(&date);
}

/// Counts the usage of a successful completion and passes the result through
/// unchanged. What every provider's `complete` ends with.
///
/// `local` is true when the request ran on this machine and so costs nothing.
pub fn tally(
    provider: &str,
    model: &str,
    local: bool,
    result: anyhow::Result<Completion>,
) -> anyhow::Result<Completion> {
    if let Ok(c) = &result {
        record(provider, model, local, c.usage);
    }
    result
}

/// The tooltip fragment for today, read from the store.
pub fn tooltip_part() -> Option<String> {
    let day = crate::store::global()
        .and_then(|s| s.day_total(&local_date_today()))
        .ok()??;
    format_tooltip_part(&day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u32, output: u32) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn cost_from_the_anthropic_fixture_usage_block() {
        // tests/fixtures/anthropic_response.json: 900 in, 60 out, claude-opus-5.
        let body = include_str!("../tests/fixtures/anthropic_response.json");
        let u = crate::provider::anthropic::Anthropic::parse_completion(body)
            .unwrap()
            .usage
            .expect("usage parsed");
        let c = cost_usd("anthropic", "claude-opus-5", "2026-10-15", &u).unwrap();
        // 900 * 5 / 1e6 + 60 * 25 / 1e6
        assert!(close(c, 0.0045 + 0.0015), "{c}");
    }

    #[test]
    fn anthropic_cache_tokens_use_their_own_rates() {
        let u = Usage {
            input_tokens: 100,
            output_tokens: 0,
            cache_write_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
        };
        let c = cost_usd("anthropic", "claude-opus-5", "2026-10-15", &u).unwrap();
        assert!(close(c, 100.0 * 5.0 / 1e6 + 6.25 + 0.50), "{c}");
    }

    #[test]
    fn cached_reads_are_priced_at_the_provider_cached_rate() {
        let u = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_write_tokens: 0,
            cache_read_tokens: 1_000_000,
        };
        let c = |p: &str, m: &str| cost_usd(p, m, "2026-10-15", &u).unwrap();
        assert!(close(c("openai", "gpt-5.4"), 0.25));
        assert!(close(c("gemini", "gemini-2.5-pro"), 0.125));
        assert!(close(c("gemini", "gemini-2.5-flash"), 0.03));
        assert!(close(c("gemini", "gemini-3.1-pro-preview"), 0.20));
        assert!(close(c("gemini", "gemini-3.8-flash"), 0.075));
        let later = cost_usd("gemini", "gemini-3.8-flash", "2027-01-01", &u).unwrap();
        assert!(close(later, 0.15), "{later}");
    }

    #[test]
    fn response_cost_is_free_on_loopback_and_unknown_for_an_unknown_tag() {
        let body = r#"{"choices":[{"finish_reason":"stop","message":{"content":"{}"}}],"usage":{"prompt_tokens":5,"completion_tokens":5}}"#;
        let local = response_cost("openai-compat", "http://127.0.0.1:1234/v1", Some("m"), body);
        assert_eq!(local, Some(0.0));
        assert_eq!(
            response_cost("elsewhere", "https://x.invalid/", Some("m"), body),
            None
        );
    }

    #[test]
    fn unknown_model_is_unpriced_not_zero() {
        assert_eq!(
            cost_usd("openai", "gpt-9", "2026-10-15", &usage(10, 10)),
            None
        );
    }

    #[test]
    fn ollama_is_free_for_any_model() {
        let c = cost_usd("ollama", "gemma3:4b", "2026-10-15", &usage(500, 50));
        assert_eq!(c, Some(0.0));
    }

    #[test]
    fn dated_snapshot_suffix_matches_but_a_different_model_does_not() {
        assert!(price_for("anthropic", "claude-haiku-4-5-20251001", "2026-10-15").is_some());
        assert!(price_for("openai", "gpt-5-nano", "2026-10-15").is_none());
        assert!(price_for("openai", "gpt-5-mini", "2026-10-15").is_some());
    }

    #[test]
    fn newest_row_on_or_before_the_day_wins() {
        let before = price_for("gemini", "gemini-3.8-flash", "2026-12-31").unwrap();
        let after = price_for("gemini", "gemini-3.8-flash", "2027-01-01").unwrap();
        assert!(close(before.input, 0.75));
        assert!(close(after.input, 1.50));
    }

    #[test]
    fn every_row_names_a_source_and_a_check_date() {
        assert!(!PRICES.is_empty());
        for r in PRICES {
            assert!(r.source.starts_with("https://"), "{}", r.model);
            assert_eq!(r.checked.len(), 10, "{}", r.model);
            assert_eq!(r.from.len(), 10, "{}", r.model);
        }
    }

    #[test]
    fn tooltip_part_shapes() {
        assert_eq!(format_tooltip_part(&DayTotal::default()), None);
        let local_only = DayTotal {
            usd: 0.0,
            requests: 4,
            unpriced: 0,
        };
        assert_eq!(format_tooltip_part(&local_only), None);
        let spend = DayTotal {
            usd: 0.0432,
            requests: 3,
            unpriced: 0,
        };
        assert_eq!(format_tooltip_part(&spend).unwrap(), "$0.04 today");
        let tiny = DayTotal {
            usd: 0.004,
            requests: 1,
            unpriced: 0,
        };
        assert_eq!(format_tooltip_part(&tiny).unwrap(), "<$0.01 today");
        let floor = DayTotal {
            usd: 0.05,
            requests: 2,
            unpriced: 1,
        };
        assert_eq!(
            format_tooltip_part(&floor).unwrap(),
            "$0.05+ today (1 unpriced)"
        );
    }

    #[test]
    fn tooltip_text_has_no_em_dash() {
        let d = DayTotal {
            usd: 1.0,
            requests: 1,
            unpriced: 1,
        };
        assert!(!format_tooltip_part(&d).unwrap().contains('\u{2014}'));
    }

    /// The wiring check: a completion passed through `tally` (what every
    /// provider's `complete` ends with) shows up in what the tooltip reads.
    #[test]
    fn tally_puts_a_completion_into_the_tooltip_text() {
        // Other tests may also record today through the shared test store,
        // so compare against what is there before.
        let today = local_date_today();
        let before = crate::store::global()
            .unwrap()
            .day_total(&today)
            .unwrap()
            .unwrap_or_default();
        let completion = Completion {
            text: String::new(),
            usage: Some(usage(1_000_000, 0)),
            stop: crate::provider::StopReason::Complete,
        };
        let out = tally("anthropic", "claude-opus-5", false, Ok(completion)).unwrap();
        assert_eq!(out.usage.unwrap().input_tokens, 1_000_000);
        let after = crate::store::global()
            .unwrap()
            .day_total(&today)
            .unwrap()
            .unwrap();
        assert_eq!(after.requests, before.requests + 1);
        assert!(close(after.usd - before.usd, 5.0), "{after:?}");
        assert!(tooltip_part().unwrap().contains("today"));
    }

    #[test]
    fn a_free_provider_is_never_unpriced_even_without_a_usage_block() {
        assert_eq!(
            request_cost("ollama", "gemma3:4b", "2026-10-15", true, None),
            Some(0.0)
        );
        assert_eq!(
            request_cost("openai-compat", "m", "2026-10-15", true, None),
            Some(0.0)
        );
        // A metered provider with no usage block is unpriced.
        assert_eq!(
            request_cost("openai", "gpt-5.5", "2026-10-15", false, None),
            None
        );
        // A non-loopback compat endpoint has no price rows: unpriced.
        assert_eq!(
            request_cost(
                "openai-compat",
                "m",
                "2026-10-15",
                false,
                Some(&usage(1, 1))
            ),
            None
        );
    }

    #[test]
    fn loopback_urls_are_recognised() {
        for url in [
            "http://127.0.0.1:1234/v1",
            "http://localhost:8080",
            "https://LOCALHOST/v1",
            "http://[::1]:8080/v1",
            "http://user:pw@127.0.0.5/v1",
        ] {
            assert!(is_loopback_url(url), "{url}");
        }
        for url in [
            "https://api.example.com/v1",
            "http://192.168.1.5:1234/v1",
            "http://127.0.0.1.evil.com/v1",
            "https://localhost.example.com",
            "http://10.0.0.1",
            "",
        ] {
            assert!(!is_loopback_url(url), "{url}");
        }
    }

    /// Default model ids that are priced on purpose as "unknown".
    const UNPRICED_ON_PURPOSE: &[(&str, &str)] = &[("gemini", "gemini-3.5-flash")];

    #[test]
    fn every_default_model_has_a_price_or_is_listed_as_exempt() {
        let providers = crate::config::Config::default().providers;
        let mut checked = 0;
        for (id, cfg) in [
            ("openai", &providers.openai),
            ("anthropic", &providers.anthropic),
            ("gemini", &providers.gemini),
        ] {
            for model in cfg.models.iter().chain(std::iter::once(&cfg.model)) {
                checked += 1;
                let exempt = UNPRICED_ON_PURPOSE.contains(&(id, model.as_str()));
                let priced = price_for(id, model, "2026-10-15").is_some();
                assert!(
                    priced != exempt,
                    "{id}/{model}: priced={priced} exempt={exempt}; add a PRICES row or an exemption (and drop a stale exemption)"
                );
            }
        }
        assert!(checked > 10);
    }

    // -- daily budget (#101) -------------------------------------------------

    fn budget(limit: f64) -> Budget {
        Budget {
            limit_usd: limit,
            warn_percent: 80,
        }
    }

    #[test]
    fn budget_state_has_three_levels() {
        let b = budget(0.05);
        assert_eq!(budget_state(0.0, &b), BudgetState::Under);
        assert_eq!(budget_state(0.039, &b), BudgetState::Under);
        assert_eq!(budget_state(0.04, &b), BudgetState::Warn);
        assert_eq!(budget_state(0.049, &b), BudgetState::Warn);
        // Float error must not let 5 one-cent requests slip under 5 cents.
        assert_eq!(budget_state(0.01 * 5.0, &b), BudgetState::Exhausted);
        assert_eq!(budget_state(0.05 + 0.01, &b), BudgetState::Exhausted);
    }

    #[test]
    fn check_budget_with_refuses_only_at_the_limit_and_says_so() {
        let b = budget(0.05);
        assert!(check_budget_with(&b, 0.04).is_ok());
        let err = check_budget_with(&b, 0.05).unwrap_err();
        assert!(err.contains("Daily budget of $0.05 reached"), "{err}");
        assert!(err.contains("local models still work"), "{err}");
        assert!(!err.contains('\u{2014}'), "no em dash in card text");
    }

    #[test]
    fn a_notice_is_queued_once_per_level_per_day() {
        let b = budget(1.0);
        let mut seen = None;
        assert_eq!(budget_notice_for(0.5, &b, "d1", &mut seen), None);
        let warn = budget_notice_for(0.8, &b, "d1", &mut seen).unwrap();
        assert!(warn.contains("$0.80 of your $1.00"), "{warn}");
        assert!(!warn.contains('\u{2014}'), "no em dash in card text");
        assert_eq!(budget_notice_for(0.9, &b, "d1", &mut seen), None);
        let stop = budget_notice_for(1.0, &b, "d1", &mut seen).unwrap();
        assert!(stop.contains("reached"), "{stop}");
        assert_eq!(budget_notice_for(1.5, &b, "d1", &mut seen), None);
        // A new day starts again.
        assert!(budget_notice_for(0.8, &b, "d2", &mut seen).is_some());
    }

    #[test]
    fn a_limit_that_is_not_positive_turns_the_budget_off() {
        for off in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(budget_from(off, 80), None, "{off}");
        }
        assert_eq!(
            budget_from(2.0, 0),
            Some(Budget {
                limit_usd: 2.0,
                warn_percent: 1
            })
        );
    }
}
