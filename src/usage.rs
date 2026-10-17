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
//! Totals are a floor: a reply that arrived is counted even when it was not an
//! answer (a refusal, a truncated thinking reply, a blocked prompt: [`tally`]
//! reads the usage block from the raw body, #730), but a call that failed
//! before any reply (a timeout, a transport error) or a body with no usage
//! block adds nothing, so the total can still be short. The
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
    // 3.6 Flash (MEASURED 2026-10-15): $0.75 in / $3.75 out / $0.075 cached per
    // 1M tokens through 2026-12-31, doubled from 2027-01-01.
    row(
        "gemini",
        "gemini-3.6-flash",
        ALWAYS,
        p(0.75, 3.75, 0.75, 0.075),
        GEMINI_SRC,
    ),
    row(
        "gemini",
        "gemini-3.6-flash",
        "2027-01-01",
        p(1.50, 7.50, 1.50, 0.15),
        GEMINI_SRC,
    ),
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
    // One trailing dot is the same name ("localhost.").
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
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

/// What one answer cost so far, for the card (#486). A run is one action
/// worker, and every action starts a fresh worker thread, so this thread-local
/// is that run's total: every provider request on the thread reports here
/// from [`record`], and a run with several chain calls (each one hands over
/// the total so far) ends with the sum. It needs no lock and cannot bleed
/// into another run.
#[derive(Clone, Copy, Default)]
struct RunCost {
    usd: f64,
    /// A request in this run had no price: the sum would be a floor, so the
    /// card shows nothing rather than a number that might be wrong.
    unpriced: bool,
}

thread_local! {
    static RUN_COST: std::cell::Cell<RunCost> = std::cell::Cell::new(RunCost::default());
}

/// Starts counting a new run. Production needs no call (a new run is a new
/// thread); tests share a thread-local per test thread and reset explicitly.
#[cfg(test)]
pub fn begin_run() {
    RUN_COST.with(|c| c.set(RunCost::default()));
}

/// Adds one request's cost (`None`: unpriced) to the run in flight.
pub fn note_run_cost(cost: Option<f64>) {
    RUN_COST.with(|c| {
        let mut run = c.get();
        match cost {
            Some(usd) => run.usd += usd,
            None => run.unpriced = true,
        }
        c.set(run);
    });
}

/// What the run in flight cost, or `None` when nothing was billed (all
/// local) or any request was unpriced. Never `Some(0.0)`.
pub fn run_cost() -> Option<f64> {
    let run = RUN_COST.with(std::cell::Cell::get);
    (!run.unpriced && run.usd > 0.0).then_some(run.usd)
}

/// The card's cost text: `About $0.002`, or `Under $0.001` for a fraction
/// of a tenth of a cent. `None` for a free or nonsense amount. No em dash
/// (AGENTS.md rule 11).
pub fn format_cost(usd: f64) -> Option<String> {
    if !usd.is_finite() || usd <= 0.0 {
        return None;
    }
    Some(if usd < 0.0005 {
        "Under $0.001".to_string()
    } else if usd < 0.1 {
        format!("About ${usd:.3}")
    } else {
        format!("About ${usd:.2}")
    })
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

/// Held by every test that touches this module's process-wide state (the
/// budget, the notice, the in-memory totals, the store-down switches) or
/// records into the real store, for the whole test (#990).
///
/// Lock order, to avoid deadlock: `EGRESS_PREVIEW_TEST_LOCK` (what
/// `network_guard` in `provider::common` takes first), then this one. Never
/// take the egress lock while holding this.
#[cfg(test)]
static USAGE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn lock_usage_test() -> std::sync::MutexGuard<'static, ()> {
    USAGE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
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

/// What `provider::common` asks before a request leaves this machine. With no
/// budget set it always allows. With one set, the saved total for today is
/// used; when the store cannot be read, the total counted in memory for this
/// run is used instead (#713), so the limit never switches itself off.
pub fn check_budget() -> Result<(), String> {
    let Some(b) = *lock(&BUDGET) else {
        return Ok(());
    };
    let date = local_date_today();
    let (spent, approximate) = spent_today(&date);
    if approximate {
        queue_approximate_notice();
    }
    check_budget_with(&b, spent)
}

#[cfg(test)]
pub(crate) static SPENT_OVERRIDE: Mutex<Option<f64>> = Mutex::new(None);

/// What this run has spent today, counted in memory by [`record`]. It exists
/// only for the case where the store is down, and then it is all there is.
static MEM_SPEND: Mutex<Option<(String, f64)>> = Mutex::new(None);
/// What this run counted but could not save (a write failed, or the store was
/// down). Added to the saved total, so a store that reads but does not write
/// cannot leave the budget stuck at its old value (#794).
static MEM_UNSAVED: Mutex<Option<(String, f64)>> = Mutex::new(None);
static APPROX_NOTICED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The last known price of one request, per `provider/model`, for pricing a
/// later response that came back with no usage block.
static LAST_COST: Mutex<Vec<(String, f64)>> = Mutex::new(Vec::new());
/// What an unpriced cloud response with no known price counts as in the
/// fallback total: deliberately on the high side.
const UNPRICED_GUESS_USD: f64 = 0.01;
/// Test seam: pretend the store cannot be read or written.
#[cfg(test)]
static STORE_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Test seam: the store reads fine but every write fails.
#[cfg(test)]
static WRITES_FAIL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Adds `usd` to the in-memory total for `date`; a new date starts from zero.
fn mem_add(mem: &mut Option<(String, f64)>, date: &str, usd: f64) {
    match mem {
        Some((d, total)) if d == date => *total += usd,
        _ => *mem = Some((date.to_string(), usd)),
    }
}

fn mem_total(mem: &Option<(String, f64)>, date: &str) -> f64 {
    match mem {
        Some((d, total)) if d == date => *total,
        _ => 0.0,
    }
}

/// The saved total plus what this run counted but could not save; when the
/// saved total cannot be read, the in-memory total for this run. The flag says
/// the number is not exact (#713, #794).
fn pick_spent(stored: Option<f64>, in_memory: f64, unsaved: f64) -> (f64, bool) {
    match stored {
        Some(s) => (s + unsaved, unsaved > 0.0),
        None => (in_memory, true),
    }
}

fn spent_today(date: &str) -> (f64, bool) {
    #[cfg(test)]
    if let Some(v) = *lock(&SPENT_OVERRIDE) {
        return (v, false);
    }
    #[cfg(test)]
    let down = STORE_DOWN.load(std::sync::atomic::Ordering::SeqCst);
    #[cfg(not(test))]
    let down = false;
    let stored = if down {
        None
    } else {
        crate::store::global()
            .and_then(|s| s.day_total(date))
            .ok()
            .map(|day| day.map_or(0.0, |d| d.usd))
    };
    pick_spent(
        stored,
        mem_total(&lock(&MEM_SPEND), date),
        mem_total(&lock(&MEM_UNSAVED), date),
    )
}

const APPROXIMATE_NOTICE: &str = "Wingman cannot read or save today's spend, so the daily budget is approximate: it counts what this run spends. It is exact again once the saved data works.";

/// Once per run, tell the user the limit is running on the fallback count.
fn queue_approximate_notice() {
    if !APPROX_NOTICED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        push_notice(APPROXIMATE_NOTICE.to_string());
    }
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
    let (spent, _) = spent_today(date);
    if let Some(text) = budget_notice_for(spent, &b, date, &mut lock(&NOTIFIED)) {
        push_notice(text);
    }
}

/// Adds `text` to the pending budget notice, joined with any already waiting.
fn push_notice(text: String) {
    let mut slot = lock(&BUDGET_NOTICE);
    *slot = Some(match slot.take() {
        Some(old) => format!("{old}\n\n{text}"),
        None => text,
    });
}

/// Counts one provider response toward today's total. Best effort: a failure
/// to write is swallowed (rule 7: accounting must never be why an answer
/// fails). A response with no usage block counts as unpriced.
pub fn record(provider: &str, model: &str, local: bool, usage: Option<Usage>) {
    let date = local_date_today();
    let cost = request_cost(provider, model, &date, local, usage.as_ref());
    note_run_cost(cost);
    // Always counted, so the budget has a total when the store is down. An
    // unpriced cloud response still costs something: count the last known
    // price for this model, else a flat guess, so the fallback never undercounts to zero.
    let counted = {
        let mut last = lock(&LAST_COST);
        let key = format!("{provider}/{model}");
        match cost {
            Some(usd) => {
                if usd > 0.0 {
                    last.retain(|(k, _)| *k != key);
                    last.push((key, usd));
                }
                usd
            }
            None => last
                .iter()
                .find(|(k, _)| *k == key)
                .map_or(UNPRICED_GUESS_USD, |(_, v)| *v),
        }
    };
    mem_add(&mut lock(&MEM_SPEND), &date, counted);
    #[cfg(test)]
    let saved = if WRITES_FAIL.load(std::sync::atomic::Ordering::SeqCst) {
        Err(crate::store::StoreError::Other("test: writes fail".into()))
    } else if STORE_DOWN.load(std::sync::atomic::Ordering::SeqCst) {
        Ok(())
    } else {
        crate::store::global()
            .and_then(|s| s.record_usage(&date, provider, model, cost, usage.as_ref()))
    };
    #[cfg(not(test))]
    let saved = crate::store::global()
        .and_then(|s| s.record_usage(&date, provider, model, cost, usage.as_ref()));
    if let Err(e) = saved {
        // Never fails the request; the next card says so, once per run.
        crate::store::report(&e);
        // The saved total is now short by this response (#794).
        mem_add(&mut lock(&MEM_UNSAVED), &date, counted);
    }
    queue_budget_notice(&date);
}

/// Counts the usage of a completion and passes the result through unchanged.
/// What every provider's `complete` ends with.
///
/// A reply that did not become an answer (a refusal, a truncated thinking
/// reply, a blocked prompt) was still billed, so `error_usage` reads the
/// usage block out of the raw body for that case (#730). It runs only on an
/// `Err`, and when it finds nothing (a body that is not JSON) nothing is
/// counted: there is no usage to count.
///
/// `local` is true when the request ran on this machine and so costs nothing.
pub fn tally(
    provider: &str,
    model: &str,
    local: bool,
    result: anyhow::Result<Completion>,
    error_usage: impl FnOnce() -> Option<Usage>,
) -> anyhow::Result<Completion> {
    match &result {
        Ok(c) => record(provider, model, local, c.usage),
        Err(_) => {
            if let Some(usage) = error_usage() {
                record(provider, model, local, Some(usage));
            }
        }
    }
    result
}

/// The tooltip fragment for today, read from the store. Runs on a worker
/// thread only (#1181), never the UI thread: it may wait for the start-up open
/// and may touch the file (#1010), so the result is never `None` just because
/// the store was still opening.
pub fn tooltip_part() -> Option<String> {
    let store = crate::store::global().ok()?;
    let day = store.day_total(&local_date_today()).ok()??;
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
        assert!(close(c("gemini", "gemini-3.6-flash"), 0.075));
        let later = cost_usd("gemini", "gemini-3.8-flash", "2027-01-01", &u).unwrap();
        assert!(close(later, 0.15), "{later}");
    }

    #[test]
    fn card_cost_text_rounds_to_a_readable_amount() {
        assert_eq!(format_cost(0.002).as_deref(), Some("About $0.002"));
        assert_eq!(format_cost(0.0123).as_deref(), Some("About $0.012"));
        assert_eq!(format_cost(0.0004).as_deref(), Some("Under $0.001"));
        assert_eq!(format_cost(0.5).as_deref(), Some("About $0.50"));
        assert_eq!(format_cost(1.234).as_deref(), Some("About $1.23"));
    }

    #[test]
    fn card_cost_text_is_absent_for_free_or_nonsense_amounts() {
        assert_eq!(format_cost(0.0), None);
        assert_eq!(format_cost(-1.0), None);
        assert_eq!(format_cost(f64::NAN), None);
    }

    #[test]
    fn a_run_sums_its_priced_requests() {
        begin_run();
        note_run_cost(Some(0.002));
        note_run_cost(Some(0.003));
        assert!(close(run_cost().unwrap(), 0.005));
    }

    #[test]
    fn one_unpriced_request_makes_the_run_cost_unknown() {
        begin_run();
        note_run_cost(Some(0.002));
        note_run_cost(None);
        assert_eq!(run_cost(), None);
    }

    #[test]
    fn a_new_run_forgets_the_last_one() {
        begin_run();
        note_run_cost(Some(0.002));
        begin_run();
        assert_eq!(run_cost(), None);
    }

    #[test]
    fn a_run_that_only_ran_locally_has_no_cost_to_show() {
        begin_run();
        note_run_cost(Some(0.0));
        assert_eq!(run_cost(), None);
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

    /// Held by every test that records into the real store (or flips the
    /// store-down switch), so a before/after difference is exact.
    fn tally_lock() -> std::sync::MutexGuard<'static, ()> {
        lock_usage_test()
    }

    fn day_total_today(today: &str) -> DayTotal {
        crate::store::global()
            .unwrap()
            .day_total(today)
            .unwrap()
            .unwrap_or_default()
    }

    /// The wiring check: a completion passed through `tally` (what every
    /// provider's `complete` ends with) shows up in what the tooltip reads.
    #[test]
    fn tally_puts_a_completion_into_the_tooltip_text() {
        // The lock keeps the other recording tests out, so the before/after
        // difference below is exactly this request.
        let _lock = tally_lock();
        let today = local_date_today();
        let before = day_total_today(&today);
        let completion = Completion {
            text: String::new(),
            usage: Some(usage(1_000_000, 0)),
            stop: crate::provider::StopReason::Complete,
        };
        let out = tally("anthropic", "claude-opus-5", false, Ok(completion), || None).unwrap();
        assert_eq!(out.usage.unwrap().input_tokens, 1_000_000);
        let after = day_total_today(&today);
        assert_eq!(after.requests, before.requests + 1);
        assert!(close(after.usd - before.usd, 5.0), "{after:?}");
        assert!(tooltip_part().unwrap().contains("today"));
    }

    /// #730: a reply that is an error (here `max_tokens` with no text, the
    /// whole budget spent thinking) still costs money, so it is counted.
    #[test]
    fn tally_counts_the_usage_of_a_reply_that_is_an_error() {
        let body = r#"{"stop_reason":"max_tokens","content":[{"type":"thinking","thinking":"..."}],"usage":{"input_tokens":1000000,"output_tokens":0}}"#;
        let _lock = tally_lock();
        let today = local_date_today();
        let before = day_total_today(&today);
        let result = crate::provider::anthropic::Anthropic::parse_completion(body);
        assert!(result.is_err());
        let out = tally("anthropic", "claude-opus-5", false, result, || {
            crate::provider::anthropic::Anthropic::parse_usage(body)
        });
        assert!(out.is_err(), "the error still reaches the caller");
        let after = day_total_today(&today);
        assert_eq!(after.requests, before.requests + 1, "counted once");
        assert!(close(after.usd - before.usd, 5.0), "{after:?}");
    }

    /// An error with no usage block anywhere (a body that is not JSON) adds
    /// nothing: there is nothing to count.
    #[test]
    fn tally_adds_nothing_for_an_error_with_no_usage() {
        let _lock = tally_lock();
        let today = local_date_today();
        let before = day_total_today(&today);
        let called = std::cell::Cell::new(false);
        let out = tally(
            "anthropic",
            "claude-opus-5",
            false,
            Err(anyhow::anyhow!("bad")),
            || {
                called.set(true);
                None
            },
        );
        assert!(out.is_err());
        assert!(called.get());
        assert_eq!(day_total_today(&today), before, "nothing was counted");
    }

    /// #730 with the store down (#713): a failed reply's usage still lands in
    /// the in-memory total the budget falls back to.
    #[test]
    fn with_the_store_down_an_error_reply_still_counts_toward_the_budget() {
        let _scope = down_scope(0.01);
        assert!(check_budget().is_ok());
        for _ in 0..2 {
            let _ = tally(
                "anthropic",
                "claude-opus-5",
                false,
                Err(anyhow::anyhow!("anthropic: model refused to answer")),
                || Some(usage(900, 60)),
            );
        }
        let err = check_budget().expect_err("two billed refusals reach the limit");
        assert!(err.contains("Daily budget"), "{err}");
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
            "http://localhost.:11434/api/chat",
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
    const UNPRICED_ON_PURPOSE: &[(&str, &str)] = &[];

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
    fn a_downed_store_falls_back_to_the_in_memory_total_and_refuses_the_second_request() {
        // #713: a budget of $0.05, a store that cannot be read, two $0.03 requests.
        let b = Budget {
            limit_usd: 0.05,
            warn_percent: 80,
        };
        let mut mem = None;
        let (spent, approx) = pick_spent(None, mem_total(&mem, "2026-10-16"), 0.0);
        assert!(approx);
        assert!(check_budget_with(&b, spent).is_ok(), "nothing spent yet");

        mem_add(&mut mem, "2026-10-16", 0.03);
        let (spent, _) = pick_spent(None, mem_total(&mem, "2026-10-16"), 0.0);
        assert!(
            check_budget_with(&b, spent).is_ok(),
            "first request allowed"
        );

        mem_add(&mut mem, "2026-10-16", 0.03);
        let (spent, _) = pick_spent(None, mem_total(&mem, "2026-10-16"), 0.0);
        let err = check_budget_with(&b, spent).unwrap_err();
        assert!(err.contains("Daily budget"), "{err}");
    }

    /// Puts every global this test touches back, also when an assertion panics.
    struct DownScope {
        /// Released only after `drop` has put every global back.
        _locks: DownGuards,
    }
    impl Drop for DownScope {
        fn drop(&mut self) {
            set_daily_budget(0.0, 80);
            STORE_DOWN.store(false, std::sync::atomic::Ordering::SeqCst);
            WRITES_FAIL.store(false, std::sync::atomic::Ordering::SeqCst);
            *lock(&MEM_SPEND) = None;
            *lock(&MEM_UNSAVED) = None;
            lock(&LAST_COST).clear();
            APPROX_NOTICED.store(false, std::sync::atomic::Ordering::SeqCst);
            *lock(&NOTIFIED) = None;
            let _ = take_budget_notice();
        }
    }

    type DownGuards = (
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
    );

    fn down_scope(limit: f64) -> DownScope {
        // Egress lock first, then the usage lock (see `USAGE_TEST_LOCK`): no
        // recording test may run while the store is switched off.
        let guard = (
            crate::config::EGRESS_PREVIEW_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
            tally_lock(),
        );
        let scope = DownScope { _locks: guard };
        set_daily_budget(limit, 80);
        STORE_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
        *lock(&MEM_SPEND) = None;
        scope
    }

    #[test]
    fn with_the_store_down_record_and_check_budget_still_stop_the_second_request() {
        let _scope = down_scope(0.01);
        // About $0.006 each.
        assert!(check_budget().is_ok());
        record("anthropic", "claude-opus-5", false, Some(usage(900, 60)));
        assert!(check_budget().is_ok(), "one request is under the limit");
        record("anthropic", "claude-opus-5", false, Some(usage(900, 60)));
        let err = check_budget().expect_err("the second request must be refused");
        assert!(err.contains("Daily budget"), "{err}");
        let notice = take_budget_notice().unwrap_or_default();
        assert!(notice.contains("approximate"), "{notice}");
    }

    #[test]
    fn with_writes_failing_but_reads_fine_the_budget_still_trips() {
        // #794: the saved total stays put when writes fail; this run's own
        // spend must still count toward the cap.
        let _scope = down_scope(0.01);
        STORE_DOWN.store(false, std::sync::atomic::Ordering::SeqCst);
        WRITES_FAIL.store(true, std::sync::atomic::Ordering::SeqCst);
        // The path under test needs a store that opens and reads; without
        // one this would silently exercise the read-failure fallback instead.
        let date = local_date_today();
        assert!(
            crate::store::global()
                .and_then(|s| s.day_total(&date))
                .is_ok(),
            "the test store must open and read"
        );
        // About $0.006 each, on top of whatever the test store already holds.
        let before = spent_today(&date).0;
        let limit = before + 0.01;
        set_daily_budget(limit, 80);
        record("anthropic", "claude-opus-5", false, Some(usage(900, 60)));
        assert!(check_budget().is_ok(), "one request is under the limit");
        record("anthropic", "claude-opus-5", false, Some(usage(900, 60)));
        let err = check_budget().expect_err("the second request must be refused");
        assert!(err.contains("Daily budget"), "{err}");
        let notice = take_budget_notice().unwrap_or_default();
        assert!(notice.contains("approximate"), "{notice}");
    }

    #[test]
    fn an_unpriced_cloud_response_still_counts_in_the_fallback_total() {
        let _scope = down_scope(0.015);
        // No usage block: unpriced, counted at the flat guess.
        record("anthropic", "model-with-no-price", false, None);
        assert!(check_budget().is_ok());
        record("anthropic", "model-with-no-price", false, None);
        assert!(check_budget().is_err());
    }

    #[test]
    fn a_notice_already_waiting_is_joined_not_overwritten() {
        let _scope = down_scope(0.0);
        let _ = take_budget_notice();
        push_notice("first".to_string());
        push_notice("second".to_string());
        assert_eq!(take_budget_notice().unwrap(), "first\n\nsecond");
    }

    #[test]
    fn the_saved_total_wins_when_the_store_works() {
        assert_eq!(pick_spent(Some(0.02), 9.0, 0.0), (0.02, false));
    }

    #[test]
    fn unsaved_spend_is_added_to_the_saved_total_and_marks_it_approximate() {
        // #794: the store reads fine but a write failed.
        let (spent, approx) = pick_spent(Some(0.02), 9.0, 0.03);
        assert!(close(spent, 0.05));
        assert!(approx);
    }

    #[test]
    fn the_in_memory_total_starts_over_on_a_new_day() {
        let mut mem = None;
        mem_add(&mut mem, "2026-10-16", 0.04);
        assert_eq!(mem_total(&mem, "2026-10-17"), 0.0);
        mem_add(&mut mem, "2026-10-17", 0.01);
        assert_eq!(mem_total(&mem, "2026-10-16"), 0.0);
        assert!(close(mem_total(&mem, "2026-10-17"), 0.01));
    }

    #[test]
    fn the_approximate_notice_says_the_limit_is_approximate_and_has_no_em_dash() {
        assert!(APPROXIMATE_NOTICE.contains("approximate"));
        assert!(!APPROXIMATE_NOTICE.contains('\u{2014}'));
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
