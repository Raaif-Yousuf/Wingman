//! Shared helpers between `anthropic.rs` and `openai.rs` (#156). Both wrap a
//! different vendor API, so their request/response *shapes* stay separate --
//! only the genuinely identical boilerplate lives here: the HTTP send (with
//! retry, #98), the image encoding, the answer JSON Schema and the
//! difficulty-rubric prompt append.
//!
//! `answer_schema`/`augmented_system_prompt` are physics-answer-specific
//! today (the only action Wingman has), but neither provider calls them any
//! more: `provider::physics_request` does, and hands the result over as
//! opaque `Request` fields (see the 2026-09-16 expansion plan's "Provider
//! trait, extended"). They stay in this file rather than `mod.rs` simply
//! because that's where the JSON-building imports already are.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

// ---------------------------------------------------------------------
// Transport / clock / sleep abstractions (#98)
//
// `post_json` is the one place both providers reach the network. Retrying
// it needs three things a real network call can't give a test: a way to
// script responses without a socket, a way to prove a sleep happened
// without waiting, and a way to control "now" for HTTP-date retry-after
// parsing. Each is a one-method trait so `post_json_with` (the retry core)
// is fully unit-testable, and `post_json` (what the providers actually
// call) just wires up the real implementations.
// ---------------------------------------------------------------------

/// One HTTP response, transport-agnostic so retry logic can be unit tested
/// without a real network call. `Err` from `Transport::post_json` means a
/// transport-level failure -- see [`TransportError`] for the two kinds --
/// never a non-2xx status, which is a normal `Ok(RawResponse)` with `status`
/// set accordingly.
pub(crate) struct RawResponse {
    pub status: u16,
    /// Header names as the server sent them; look up with
    /// [`find_header`] (case-insensitive) rather than indexing directly.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// #187: a transport failure is only safe to retry when the vendor never
/// received (or at least never acknowledged) the request. Once a status has
/// come back, `send_json` succeeding means the request almost certainly
/// reached the vendor -- and, for a billed completion API, was almost
/// certainly already processed/charged -- so a failure reading the body
/// after that point must never be treated the same as "never connected".
pub(crate) enum TransportError {
    /// DNS, connect, TLS, or send failed -- no response was ever received.
    /// Safe to retry per [`RetryPolicy`].
    NoResponse(String),
    /// A status (and headers) were received, but reading the body then
    /// failed (e.g. the connection dropped mid-body). The request almost
    /// certainly already reached the vendor, so this is never retried as a
    /// fresh attempt -- it surfaces immediately, naming the status that was
    /// received, so the card is honest about what's known to have happened.
    BodyReadFailed { status: u16, error: String },
}

pub(crate) trait Transport {
    fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
        timeout: Duration,
    ) -> std::result::Result<RawResponse, TransportError>;
}

/// The real transport: `ureq`, blocking, no streaming (see the crate-level
/// note in `provider::mod` on why -- no chat, no partial state).
struct UreqTransport;

impl Transport for UreqTransport {
    fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
        timeout: Duration,
    ) -> std::result::Result<RawResponse, TransportError> {
        let mut builder = ureq::post(url);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }

        // `http_status_as_error(false)` is load-bearing: without it a
        // non-2xx comes back as an `Err` that has already discarded the
        // body, and the body is exactly what the caller needs to report
        // (the API's own error message, or the retry-after header on 429).
        let mut response = builder
            .config()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .build()
            .send_json(body)
            .map_err(|e| TransportError::NoResponse(e.to_string()))?;

        // A status was received from here on -- any further failure is
        // `BodyReadFailed`, never folded back into `NoResponse` (#187).
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_string(), value.to_str().unwrap_or_default().to_string()))
            .collect();
        let body = response.body_mut().read_to_string().map_err(|e| TransportError::BodyReadFailed {
            status,
            error: format!("failed to read response body: {e}"),
        })?;

        Ok(RawResponse { status, headers, body })
    }
}

/// Wall-clock time, injected so HTTP-date `retry-after` parsing is testable
/// without depending on the real clock.
pub(crate) trait Clock {
    fn now_unix(&self) -> u64;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// Sleeping, injected so tests prove a retry happened without ever
/// actually waiting.
pub(crate) trait Sleeper {
    fn sleep(&self, d: Duration);
}

struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// #98: retry policy for transport errors, 5xx, and 429. Deliberately
/// small and bounded so one Copilot-key press still ends in a card
/// promptly (rule 7) rather than hanging behind several long backoffs.
pub(crate) struct RetryPolicy {
    /// Max retries for a transport error or a 5xx. A 429 gets its own
    /// single retry (see `max_retry_after`) independent of this budget.
    pub max_retries: u32,
    /// Base delay for the jittered exponential backoff used by transport
    /// error / 5xx retries.
    pub base_delay: Duration,
    /// Cumulative cap on backoff sleep across all transport/5xx retries
    /// for one `post_json_with` call -- "a few seconds", not a compounding
    /// wait.
    pub max_total_backoff: Duration,
    /// A 429's stated `retry-after` is honoured with one retry only when
    /// it is at most this long; longer than this (or missing) means no
    /// retry, just a card naming the delay.
    pub max_retry_after: Duration,
    /// #186: wall-clock budget across every attempt and backoff/retry-after
    /// sleep in one `post_json_with` call, checked against a deadline
    /// computed once at the very start. This is what keeps the retry loop
    /// bounded independently of `max_retries` x the per-attempt `timeout`:
    /// without it, a connection that hangs rather than fails fast (a
    /// stalled TLS handshake, a server that accepts but never responds) can
    /// burn the *full* per-attempt timeout on every single retry, so 3
    /// attempts at 90s each is ~270s, not ~90s. Set comfortably above the
    /// cloud providers' `REQUEST_TIMEOUT` (90s, see
    /// `anthropic.rs`/`openai.rs`) so one legitimately slow single attempt
    /// is unaffected -- only a second or third attempt gets shrunk to
    /// whatever is left.
    pub max_total_wall_clock: Duration,
    /// Below this much remaining wall-clock budget, no further attempt is
    /// started at all -- the loop returns the last error immediately
    /// rather than sending one more request sized too small to plausibly
    /// succeed.
    pub retry_floor: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_millis(200),
            max_total_backoff: Duration::from_secs(3),
            max_retry_after: Duration::from_secs(5),
            max_total_wall_clock: Duration::from_secs(150),
            retry_floor: Duration::from_secs(5),
        }
    }
}

/// Cheap, dependency-free jitter (rule 2: no new crate where one can be
/// avoided). Mixes a monotonic counter with the sub-second clock so two
/// retries started in the same instant don't land on the same delay. Not
/// cryptographic -- it only needs to scatter retries, never resist
/// prediction.
fn jitter_millis(bound_ms: u64) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    if bound_ms == 0 {
        return 0;
    }
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    // xorshift64, seeded from the mix above -- cheap and good enough to
    // scatter a handful of retries, not a general-purpose RNG.
    let mut x = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x % bound_ms
}

/// Full-jitter exponential backoff (the "full jitter" algorithm): a
/// uniform random delay between 0 and `min(cap, base * 2^attempt)`. `cap`
/// is the remaining budget out of `RetryPolicy::max_total_backoff`, so the
/// last retry never overshoots it.
fn jittered_backoff(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let exp_ms = (base.as_millis() as u64).saturating_mul(1u64 << attempt.min(10));
    let capped_ms = exp_ms.min(cap.as_millis() as u64);
    Duration::from_millis(jitter_millis(capped_ms.max(1)))
}

/// Case-insensitive header lookup -- servers are inconsistent about casing
/// (`Retry-After` vs `retry-after`) and HTTP header names are
/// case-insensitive by spec regardless.
fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Parses a `retry-after` header value: either delta-seconds (`"30"`, both
/// Anthropic and OpenAI send this form on a 429) or an RFC 7231 IMF-fixdate
/// (`"Wed, 21 Oct 2026 07:28:00 GMT"`). `clock` resolves the date form to a
/// duration from "now", which is what makes this testable without the real
/// clock. Returns `None` when the header is absent or neither form parses.
pub(crate) fn parse_retry_after(headers: &[(String, String)], clock: &dyn Clock) -> Option<Duration> {
    let raw = find_header(headers, "retry-after")?.trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let target_unix = parse_http_date(raw)?;
    let now = clock.now_unix();
    Some(Duration::from_secs(target_unix.saturating_sub(now)))
}

/// Parses an RFC 7231 IMF-fixdate, e.g. `"Wed, 21 Oct 2026 07:28:00 GMT"`,
/// to a Unix timestamp. The weekday name is not validated (it's redundant
/// with the date and no provider is known to get it wrong); only the
/// `GMT` suffix is required, matching what both vendors send.
fn parse_http_date(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 || parts[5] != "GMT" {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let mut time = parts[4].split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next()?.parse().ok()?;
    if time.next().is_some() {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let total = days * 86_400 + hour * 3600 + minute * 60 + second;
    if total < 0 {
        None
    } else {
        Some(total as u64)
    }
}

/// Howard Hinnant's `days_from_civil`: days since the Unix epoch
/// (1970-01-01) for a proleptic-Gregorian calendar date. Pulled in as a
/// dozen lines rather than a date/time crate just to turn an HTTP-date
/// into a Unix timestamp for `retry-after` (rule 2: prefer no new
/// dependency where reasonable).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The truncated-body error text shared by every non-2xx branch except 429
/// (which gets its own message naming the retry delay, see
/// `rate_limited_error`).
fn http_error(tag: &str, status: u16, body: &str) -> anyhow::Error {
    let truncated: String = body.chars().take(300).collect();
    anyhow!("{tag}: HTTP {status}: {truncated}")
}

/// #98: names the provider (`tag`) and the retry-after delay so the card
/// the user sees is actionable rather than a bare "HTTP 429". No em dash
/// (rule 11).
fn rate_limited_error(tag: &str, retry_after: Option<Duration>) -> anyhow::Error {
    match retry_after {
        Some(d) => anyhow!("{tag}: too many requests. Retry after {} seconds.", d.as_secs()),
        None => anyhow!("{tag}: too many requests. No retry-after time was given."),
    }
}

/// Bundles the four retry dependencies (transport, sleep, clock, policy)
/// into one value so `post_json_with` stays under clippy's argument-count
/// lint -- these four always travel together (a caller building one builds
/// all four), unlike `url`/`headers`/`body`/`timeout`/`tag`, which vary per
/// call.
pub(crate) struct RetryEnv<'a> {
    pub transport: &'a dyn Transport,
    pub sleeper: &'a dyn Sleeper,
    pub clock: &'a dyn Clock,
    pub policy: &'a RetryPolicy,
}

/// The retry core behind `post_json`, parameterised over transport, sleep
/// and clock (via `env`) so it is unit-testable without a network call or
/// a real sleep (#98).
///
/// - A transport error or a 5xx is retried up to `policy.max_retries`
///   times with jittered exponential backoff, capped at
///   `policy.max_total_backoff` cumulative sleep. "A transport error" here
///   means [`TransportError::NoResponse`] specifically -- no status was ever
///   received, so nothing is known to have reached the vendor. A body-read
///   failure *after* a status was received ([`TransportError::BodyReadFailed`])
///   is never retried this way (#187): the request almost certainly already
///   reached the vendor, so retrying it risks a second billed completion.
/// - A 429 is retried exactly once, sleeping the server's stated
///   `retry-after` verbatim (never jittered -- that delay is the server's
///   instruction, not our backoff), but only when that delay is at most
///   `policy.max_retry_after`. A second 429, a too-long delay, or a
///   missing header all surface as an error naming the provider and the
///   delay (`rate_limited_error`) rather than retrying.
/// - Any other 4xx is never retried.
///
/// A provider that exhausts its retries here still falls through to the
/// next provider in the `Chain` as today -- this function only decides
/// whether *this* provider gets a second attempt, not what happens after
/// it gives up.
pub(crate) fn post_json_with(
    env: &RetryEnv,
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    timeout: Duration,
    tag: &str,
) -> Result<String> {
    offline_guard(url)?;

    let policy = env.policy;
    let mut retries = 0u32;
    let mut backoff_spent = Duration::ZERO;
    let mut retried_429 = false;

    // #186: a wall-clock deadline computed once, up front -- never
    // recomputed from a fresh "now" per attempt, or a slow attempt would
    // keep resetting its own budget. `wall_clock_remaining()` shrinks every
    // time it's called after real time (or, in a test, an injected `Clock`)
    // has advanced.
    let deadline_unix = env.clock.now_unix().saturating_add(policy.max_total_wall_clock.as_secs());
    let wall_clock_remaining = || Duration::from_secs(deadline_unix.saturating_sub(env.clock.now_unix()));

    loop {
        // Each attempt gets the smaller of the caller's requested timeout
        // and what's left of the wall-clock budget -- the first attempt is
        // unaffected as long as the budget is at least the requested
        // timeout (the default policy's is), but a second or third attempt
        // only gets whatever remains.
        let attempt_timeout = timeout.min(wall_clock_remaining());

        match env.transport.post_json(url, headers, body, attempt_timeout) {
            // #187: a status was already received -- the request almost
            // certainly reached the vendor (and, for a billed completion
            // API, was almost certainly already processed). Never retried
            // as a fresh attempt; surfaces immediately, naming the status
            // that was received so the card is honest about what's known.
            Err(TransportError::BodyReadFailed { status, error }) => {
                return Err(anyhow!("{tag}: HTTP {status} received, then failed reading the response body: {error}"));
            }
            Err(TransportError::NoResponse(transport_err)) => {
                let backoff_remaining = policy.max_total_backoff.saturating_sub(backoff_spent);
                if retries < policy.max_retries && !backoff_remaining.is_zero() && wall_clock_remaining() > policy.retry_floor {
                    let delay = jittered_backoff(retries, policy.base_delay, backoff_remaining);
                    env.sleeper.sleep(delay);
                    backoff_spent += delay;
                    retries += 1;
                    continue;
                }
                return Err(anyhow!("{tag}: transport error: {transport_err}"));
            }
            Ok(resp) => {
                if (200..300).contains(&resp.status) {
                    return Ok(resp.body);
                }

                if resp.status == 429 {
                    let retry_after = parse_retry_after(&resp.headers, env.clock);
                    if !retried_429 && wall_clock_remaining() > policy.retry_floor {
                        if let Some(delay) = retry_after {
                            if delay <= policy.max_retry_after {
                                env.sleeper.sleep(delay);
                                retried_429 = true;
                                continue;
                            }
                        }
                    }
                    return Err(rate_limited_error(tag, retry_after));
                }

                if resp.status >= 500 {
                    let backoff_remaining = policy.max_total_backoff.saturating_sub(backoff_spent);
                    if retries < policy.max_retries && !backoff_remaining.is_zero() && wall_clock_remaining() > policy.retry_floor {
                        let delay = jittered_backoff(retries, policy.base_delay, backoff_remaining);
                        env.sleeper.sleep(delay);
                        backoff_spent += delay;
                        retries += 1;
                        continue;
                    }
                }

                return Err(http_error(tag, resp.status, &resp.body));
            }
        }
    }
}

/// POSTs `body` as JSON to `url` with `headers`, waits up to `timeout`,
/// retries per [`RetryPolicy::default`] (#98: transport errors, 5xx, and a
/// bounded single 429 retry), and returns the raw response body text for
/// any 2xx status. See [`post_json_with`] for the exact retry semantics.
pub(crate) fn post_json(url: &str, headers: &[(&str, &str)], body: &Value, timeout: Duration, tag: &str) -> Result<String> {
    let policy = RetryPolicy::default();
    let env = RetryEnv {
        transport: &UreqTransport,
        sleeper: &ThreadSleeper,
        clock: &SystemClock,
        policy: &policy,
    };
    post_json_with(&env, url, headers, body, timeout, tag)
}

// ---------------------------------------------------------------------
// Offline guard (#19)
//
// Every function below that actually opens a socket (`post_json_with`,
// `post_json_with_connect_timeout`, `get_text_with_timeout`) calls this
// FIRST, before any transport, before any DNS resolution -- `classify_host`
// is pure string parsing, never a lookup, which is exactly what issue #19
// requires ("resolve nothing via DNS for the check"). A provider that
// somehow reached the network any other way would bypass this; there is
// deliberately no other way to reach the network from `src/provider` --
// see `no_provider_file_calls_ureq_directly_outside_common_rs` below, which
// fails the build if one shows up.
//
// CONNECTORS HOOK (not built yet, Phase 3+): `connectors/*.rs` and the
// opt-in update checker (Phase 5) must call `offline_guard` (or an
// equivalent guarded send path) the same way once they exist. The Modes
// table requires both to be disabled OUTRIGHT in Offline mode, stricter
// than the loopback-only rule providers get here -- this function alone is
// not sufficient for them, only necessary.
// ---------------------------------------------------------------------

fn offline_guard(url: &str) -> Result<()> {
    if !crate::mode::is_offline_now() {
        return Ok(());
    }
    match crate::mode::classify_host(url) {
        crate::mode::HostClass::Loopback => Ok(()),
        crate::mode::HostClass::Localhost => Err(anyhow!(
            "Offline mode blocked a request to localhost. Wingman only allows 127.0.0.1 or [::1] while Offline: point the provider at 127.0.0.1, or turn Offline mode off."
        )),
        crate::mode::HostClass::NotLoopback => Err(anyhow!(
            "Offline mode blocked a request to a non-local address. Turn off Offline mode, or point every provider at 127.0.0.1, to allow it."
        )),
    }
}

/// GET, single attempt, no retry (#19, for `mode::probe_ollama_ready`): the
/// caller treats any failure as "not reachable" and degrades gracefully, so
/// a retry here would only add latency to exactly the path issue #19
/// requires add none of when Ollama is not configured. Goes through
/// [`offline_guard`] like every other entry point in this file.
pub(crate) fn get_text_with_timeout(url: &str, timeout: Duration, tag: &str) -> Result<String> {
    offline_guard(url)?;

    let response = ureq::get(url)
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .call();
    let mut response = response.map_err(|e| anyhow!("{tag}: transport error: {e}"))?;

    let status = response.status();
    let body_text = response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("{tag}: failed to read response body"))?;

    if !status.is_success() {
        let truncated: String = body_text.chars().take(300).collect();
        return Err(anyhow!("{tag}: HTTP {status}: {truncated}"));
    }

    Ok(body_text)
}

/// POST whose response body is consumed incrementally by `read` (Ollama's
/// `/api/pull` NDJSON download progress; not model output, so the
/// no-streaming rule does not apply). Single attempt, only the connect phase
/// is timed out: a model download can legitimately run for minutes. Goes
/// through [`offline_guard`] like every other entry point in this file
/// (#189).
pub(crate) fn post_json_read_body<T>(
    url: &str,
    body: &Value,
    connect_timeout: Duration,
    tag: &str,
    read: impl FnOnce(&mut dyn std::io::Read) -> Result<T>,
) -> Result<T> {
    offline_guard(url)?;

    let mut response = ureq::post(url)
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(connect_timeout))
        .build()
        .send_json(body)
        .map_err(|e| anyhow!("{tag}: transport error: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.body_mut().read_to_string().unwrap_or_default();
        let truncated: String = body_text.chars().take(300).collect();
        return Err(anyhow!("{tag}: HTTP {status}: {truncated}"));
    }

    let mut reader = response.body_mut().as_reader();
    read(&mut reader)
}

/// Same as [`post_json`] but with the connect phase timed out separately
/// from the whole exchange. `post_json`'s single `timeout_global` is right
/// for a cloud API, which is either reachable in well under a second or not
/// reachable at all -- one timeout for both phases loses nothing. A local
/// Ollama server is the opposite: the TCP connect to loopback should be
/// near-instant (so a slow/wedged server is detected quickly), but the
/// first request after a model is not yet loaded can legitimately take tens
/// of seconds while it loads into memory, so the overall exchange needs a
/// much longer budget. Reusing `post_json`'s single timeout for both would
/// force picking one of "detects a hung connect fast" or "doesn't abort a
/// cold model load", so the two are split here instead.
pub(crate) fn post_json_with_connect_timeout(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    connect_timeout: Duration,
    total_timeout: Duration,
    tag: &str,
) -> Result<String> {
    offline_guard(url)?;

    let mut builder = ureq::post(url);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }

    let mut response = builder
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(connect_timeout))
        .timeout_global(Some(total_timeout))
        .build()
        .send_json(body)
        .map_err(|e| anyhow!("{tag}: transport error: {e}"))?;

    let status = response.status();
    let body_text = response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("{tag}: failed to read response body"))?;

    if !status.is_success() {
        let truncated: String = body_text.chars().take(300).collect();
        return Err(anyhow!("{tag}: HTTP {status}: {truncated}"));
    }

    Ok(body_text)
}

/// Base64-encodes each image, in order. Both providers embed the screenshot
/// as base64 PNG, just inside different envelope shapes.
pub(crate) fn encode_images_base64(images: &[Vec<u8>]) -> Vec<String> {
    images
        .iter()
        .map(|png| base64::engine::general_purpose::STANDARD.encode(png))
        .collect()
}

/// The JSON Schema for the physics-check `Answer`: `detail` is listed (and
/// required) before `headline` deliberately -- with `headline` first the
/// model committed to a verdict before doing the arithmetic and then
/// contradicted itself (rule 3). `difficulty` goes last, after `headline`,
/// so the model rates the problem only once it has actually worked through
/// it rather than up front.
pub(crate) fn answer_schema(want_difficulty: bool) -> Value {
    let mut properties = json!({
        "detail": {"type": "string"},
        "headline": {"type": "string"}
    });
    let mut required = vec!["detail", "headline"];
    if want_difficulty {
        properties["difficulty"] = json!({
            "type": "string",
            "enum": ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "U", "N"]
        });
        required.push("difficulty");
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// Appends [`super::DIFFICULTY_RUBRIC`] to `prompt` when requested, never
/// merging it into `super::DEFAULT_PROMPT` itself, so the user's own edited
/// prompt text in Settings is untouched.
pub(crate) fn augmented_system_prompt(prompt: &str, want_difficulty: bool) -> String {
    if want_difficulty {
        format!("{prompt}{}", super::DIFFICULTY_RUBRIC)
    } else {
        prompt.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // -- test doubles ----------------------------------------------------

    struct ScriptedTransport {
        responses: Mutex<Vec<std::result::Result<RawResponse, TransportError>>>,
        calls: Mutex<u32>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<std::result::Result<RawResponse, TransportError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(0),
            }
        }

        fn call_count(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl Transport for ScriptedTransport {
        fn post_json(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &Value,
            _timeout: Duration,
        ) -> std::result::Result<RawResponse, TransportError> {
            *self.calls.lock().unwrap() += 1;
            let mut responses = self.responses.lock().unwrap();
            assert!(!responses.is_empty(), "transport called more times than scripted");
            responses.remove(0)
        }
    }

    struct RecordingSleeper {
        sleeps: Mutex<Vec<Duration>>,
    }

    impl RecordingSleeper {
        fn new() -> Self {
            Self { sleeps: Mutex::new(Vec::new()) }
        }

        fn recorded(&self) -> Vec<Duration> {
            self.sleeps.lock().unwrap().clone()
        }
    }

    impl Sleeper for RecordingSleeper {
        fn sleep(&self, d: Duration) {
            self.sleeps.lock().unwrap().push(d);
        }
    }

    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now_unix(&self) -> u64 {
            self.0
        }
    }

    /// A scripted pre-response failure -- no status was ever received.
    fn no_response(msg: &str) -> std::result::Result<RawResponse, TransportError> {
        Err(TransportError::NoResponse(msg.to_string()))
    }

    fn ok(body: &str) -> std::result::Result<RawResponse, TransportError> {
        Ok(RawResponse {
            status: 200,
            headers: vec![],
            body: body.to_string(),
        })
    }

    fn status(code: u16, headers: Vec<(&str, &str)>, body: &str) -> std::result::Result<RawResponse, TransportError> {
        Ok(RawResponse {
            status: code,
            headers: headers.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: body.to_string(),
        })
    }

    /// A scripted #187 failure: a status was received, then reading the
    /// body failed.
    fn body_read_failed(status: u16, msg: &str) -> std::result::Result<RawResponse, TransportError> {
        Err(TransportError::BodyReadFailed { status, error: msg.to_string() })
    }

    fn run(
        transport: &ScriptedTransport,
        sleeper: &RecordingSleeper,
        clock: &FixedClock,
    ) -> Result<String> {
        let policy = RetryPolicy::default();
        let env = RetryEnv {
            transport,
            sleeper,
            clock,
            policy: &policy,
        };
        post_json_with(&env, "https://example.invalid/", &[], &json!({}), Duration::from_secs(1), "anthropic")
    }

    // -- #98: retry policy -------------------------------------------------

    #[test]
    fn transport_error_retries_then_succeeds_without_a_real_sleep() {
        let transport = ScriptedTransport::new(vec![no_response("connection refused"), ok("done")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let result = run(&transport, &sleeper, &clock).unwrap();

        assert_eq!(result, "done");
        assert_eq!(transport.call_count(), 2);
        assert_eq!(sleeper.recorded().len(), 1);
    }

    #[test]
    fn http_500_then_200_succeeds() {
        let transport = ScriptedTransport::new(vec![status(500, vec![], "server error"), ok("done")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let result = run(&transport, &sleeper, &clock).unwrap();

        assert_eq!(result, "done");
        assert_eq!(transport.call_count(), 2);
        assert_eq!(sleeper.recorded().len(), 1);
    }

    #[test]
    fn http_400_never_retries() {
        let transport = ScriptedTransport::new(vec![status(400, vec![], "bad request")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let err = run(&transport, &sleeper, &clock).unwrap_err();

        assert_eq!(transport.call_count(), 1);
        assert!(sleeper.recorded().is_empty());
        assert!(err.to_string().contains("HTTP 400"));
    }

    #[test]
    fn http_404_never_retries() {
        let transport = ScriptedTransport::new(vec![status(404, vec![], "not found")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        run(&transport, &sleeper, &clock).unwrap_err();

        assert_eq!(transport.call_count(), 1);
        assert!(sleeper.recorded().is_empty());
    }

    #[test]
    fn transport_errors_stop_after_max_retries_within_the_backoff_budget() {
        let transport =
            ScriptedTransport::new(vec![no_response("e1"), no_response("e2"), no_response("e3")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let err = run(&transport, &sleeper, &clock).unwrap_err();

        // 1 initial attempt + `max_retries` (2) retries, never more.
        assert_eq!(transport.call_count(), 3);
        let sleeps = sleeper.recorded();
        assert_eq!(sleeps.len(), 2);
        let total: Duration = sleeps.iter().sum();
        assert!(total <= RetryPolicy::default().max_total_backoff);
        assert!(err.to_string().contains("transport error"));
    }

    // -- #187: a body-read failure after a status is never retried --------

    #[test]
    fn body_read_failure_after_200_is_not_retried_and_surfaces_immediately() {
        let transport = ScriptedTransport::new(vec![body_read_failed(200, "connection reset")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let err = run(&transport, &sleeper, &clock).unwrap_err();

        assert_eq!(
            transport.call_count(),
            1,
            "#187: never retried -- the vendor almost certainly already received (and may have billed) the request"
        );
        assert!(sleeper.recorded().is_empty(), "no retry means no backoff sleep either");
        let msg = err.to_string();
        assert!(msg.contains("200"), "names the status that was received: {msg}");
        assert!(msg.contains("connection reset"), "names the underlying error: {msg}");
    }

    #[test]
    fn body_read_failure_after_5xx_is_not_retried_either() {
        // The "do not retry a body-read failure" rule doesn't depend on
        // which status came back -- any status at all means the request
        // reached the vendor.
        let transport = ScriptedTransport::new(vec![body_read_failed(500, "reset mid-body")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let err = run(&transport, &sleeper, &clock).unwrap_err();

        assert_eq!(transport.call_count(), 1);
        assert!(sleeper.recorded().is_empty());
        assert!(err.to_string().contains("500"));
    }

    #[test]
    fn a_pre_response_transport_error_is_still_retried_normally() {
        // Contrast case: `NoResponse` (never got a status at all) keeps the
        // existing retry-then-succeed behaviour -- #187 only changes the
        // BodyReadFailed path.
        let transport = ScriptedTransport::new(vec![no_response("connection refused"), ok("done")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let result = run(&transport, &sleeper, &clock).unwrap();

        assert_eq!(result, "done");
        assert_eq!(transport.call_count(), 2);
    }

    #[test]
    fn http_429_with_short_retry_after_retries_once_then_surfaces_a_card() {
        let transport = ScriptedTransport::new(vec![
            status(429, vec![("retry-after", "3")], "slow down"),
            status(429, vec![("retry-after", "3")], "slow down"),
        ]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let policy = RetryPolicy::default();
        let env = RetryEnv {
            transport: &transport,
            sleeper: &sleeper,
            clock: &clock,
            policy: &policy,
        };
        let err = post_json_with(&env, "https://example.invalid/", &[], &json!({}), Duration::from_secs(1), "openai").unwrap_err();

        assert_eq!(transport.call_count(), 2, "exactly one 429 retry");
        assert_eq!(sleeper.recorded(), vec![Duration::from_secs(3)], "the stated delay, not jittered");
        let msg = err.to_string();
        assert!(msg.contains("openai"), "names the provider: {msg}");
        assert!(msg.contains("3 seconds"), "names the retry-after: {msg}");
        assert!(!msg.contains('\u{2014}'), "rule 11: no em dash: {msg}");
    }

    #[test]
    fn http_429_that_recovers_on_retry_succeeds() {
        let transport = ScriptedTransport::new(vec![status(429, vec![("retry-after", "2")], "slow down"), ok("done")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let result = run(&transport, &sleeper, &clock).unwrap();

        assert_eq!(result, "done");
        assert_eq!(sleeper.recorded(), vec![Duration::from_secs(2)]);
    }

    #[test]
    fn http_429_with_long_retry_after_never_retries() {
        let transport = ScriptedTransport::new(vec![status(429, vec![("retry-after", "30")], "slow down")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let err = run(&transport, &sleeper, &clock).unwrap_err();

        assert_eq!(transport.call_count(), 1, "delay over the threshold: no retry");
        assert!(sleeper.recorded().is_empty());
        assert!(err.to_string().contains("30 seconds"));
    }

    #[test]
    fn http_429_without_retry_after_header_never_retries() {
        let transport = ScriptedTransport::new(vec![status(429, vec![], "slow down")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let err = run(&transport, &sleeper, &clock).unwrap_err();

        assert_eq!(transport.call_count(), 1);
        assert!(sleeper.recorded().is_empty());
        assert!(err.to_string().contains("No retry-after time was given"));
    }

    #[test]
    fn http_429_retry_after_is_case_insensitive() {
        let transport = ScriptedTransport::new(vec![status(429, vec![("Retry-After", "1")], "slow down"), ok("done")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);

        let result = run(&transport, &sleeper, &clock).unwrap();

        assert_eq!(result, "done");
        assert_eq!(sleeper.recorded(), vec![Duration::from_secs(1)]);
    }

    #[test]
    fn http_429_retry_after_http_date_is_resolved_against_the_injected_clock() {
        let transport = ScriptedTransport::new(vec![
            status(429, vec![("retry-after", "Thu, 01 Jan 1970 00:00:04 GMT")], "slow down"),
            ok("done"),
        ]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(1); // "now" is 1s past epoch; target is 4s past epoch.

        let result = run(&transport, &sleeper, &clock).unwrap();

        assert_eq!(result, "done");
        assert_eq!(sleeper.recorded(), vec![Duration::from_secs(3)]);
    }

    #[test]
    fn http_429_http_date_already_in_the_past_retries_immediately() {
        let transport = ScriptedTransport::new(vec![
            status(429, vec![("retry-after", "Thu, 01 Jan 1970 00:00:01 GMT")], "slow down"),
            ok("done"),
        ]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(100); // well past the target -- delay saturates to 0.

        let result = run(&transport, &sleeper, &clock).unwrap();

        assert_eq!(result, "done");
        assert_eq!(sleeper.recorded(), vec![Duration::from_secs(0)]);
    }

    // -- #186: wall-clock budget across attempts --------------------------

    /// Mutable so a transport double sharing it can simulate real elapsed
    /// time without an actual sleep (`FixedClock` above is deliberately
    /// immovable, which is right for the retry-after tests but wrong here).
    struct FakeClock(std::cell::Cell<u64>);

    impl FakeClock {
        fn new(t: u64) -> Self {
            Self(std::cell::Cell::new(t))
        }
    }

    impl Clock for FakeClock {
        fn now_unix(&self) -> u64 {
            self.0.get()
        }
    }

    /// Simulates a connection that always takes (almost) exactly the
    /// timeout it was given before failing -- a stalled TLS handshake or a
    /// server that accepts but never responds, the shape #186 is about.
    /// Advances the shared `FakeClock` by the timeout it received, so the
    /// retry loop's wall-clock bookkeeping is exercised without a real
    /// sleep, and records every timeout it was called with so the test can
    /// assert a later attempt's timeout actually shrank.
    struct HangingTransport<'a> {
        clock: &'a FakeClock,
        timeouts_seen: Mutex<Vec<Duration>>,
    }

    impl<'a> HangingTransport<'a> {
        fn new(clock: &'a FakeClock) -> Self {
            Self { clock, timeouts_seen: Mutex::new(Vec::new()) }
        }

        fn timeouts_seen(&self) -> Vec<Duration> {
            self.timeouts_seen.lock().unwrap().clone()
        }
    }

    impl Transport for HangingTransport<'_> {
        fn post_json(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &Value,
            timeout: Duration,
        ) -> std::result::Result<RawResponse, TransportError> {
            self.timeouts_seen.lock().unwrap().push(timeout);
            self.clock.0.set(self.clock.0.get() + timeout.as_secs());
            Err(TransportError::NoResponse("simulated hang".to_string()))
        }
    }

    #[test]
    fn wall_clock_budget_shrinks_a_later_attempts_timeout_and_stays_bounded() {
        let clock = FakeClock::new(0);
        let transport = HangingTransport::new(&clock);
        let sleeper = RecordingSleeper::new();
        let policy = RetryPolicy {
            max_total_wall_clock: Duration::from_secs(200),
            retry_floor: Duration::from_secs(10),
            ..RetryPolicy::default()
        };
        let env = RetryEnv { transport: &transport, sleeper: &sleeper, clock: &clock, policy: &policy };

        let err = post_json_with(&env, "https://example.invalid/", &[], &json!({}), Duration::from_secs(90), "anthropic")
            .unwrap_err();

        let timeouts = transport.timeouts_seen();
        assert_eq!(timeouts.len(), 3, "max_retries=2 still allows 3 attempts here");
        assert_eq!(timeouts[0], Duration::from_secs(90), "first attempt: full budget available, unaffected");
        assert_eq!(timeouts[1], Duration::from_secs(90), "second attempt: still enough budget left");
        assert_eq!(
            timeouts[2],
            Duration::from_secs(20),
            "third attempt's timeout is capped by what's left of the wall-clock budget: {:?}",
            timeouts[2]
        );

        // 90 + 90 + 20 = 200s, nowhere near 3 * 90s = 270s -- #186's worst case.
        assert_eq!(clock.now_unix(), 200);
        assert!(err.to_string().contains("transport error"));
    }

    #[test]
    fn wall_clock_floor_stops_retrying_before_max_retries_is_reached() {
        let clock = FakeClock::new(0);
        let transport = HangingTransport::new(&clock);
        let sleeper = RecordingSleeper::new();
        let policy = RetryPolicy {
            max_total_wall_clock: Duration::from_secs(95),
            retry_floor: Duration::from_secs(10),
            ..RetryPolicy::default()
        };
        let env = RetryEnv { transport: &transport, sleeper: &sleeper, clock: &clock, policy: &policy };

        let err = post_json_with(&env, "https://example.invalid/", &[], &json!({}), Duration::from_secs(90), "openai")
            .unwrap_err();

        let timeouts = transport.timeouts_seen();
        assert_eq!(
            timeouts.len(),
            1,
            "remaining budget (5s) is below the floor (10s) after the first attempt: no second attempt at all"
        );
        assert_eq!(timeouts[0], Duration::from_secs(90));
        assert!(sleeper.recorded().is_empty(), "never slept for a retry it wasn't going to make");
        assert!(err.to_string().contains("transport error"));
    }

    // -- retry-after parsing (unit-level) ---------------------------------

    #[test]
    fn parse_retry_after_reads_delta_seconds() {
        let headers = vec![("retry-after".to_string(), "45".to_string())];
        assert_eq!(parse_retry_after(&headers, &FixedClock(0)), Some(Duration::from_secs(45)));
    }

    #[test]
    fn parse_retry_after_reads_http_date() {
        let headers = vec![("retry-after".to_string(), "Wed, 21 Oct 2026 07:28:00 GMT".to_string())];
        // Reference value cross-checked against `date -u -d ... +%s`.
        assert_eq!(parse_http_date("Wed, 21 Oct 2026 07:28:00 GMT"), Some(1_792_567_680));
        assert_eq!(
            parse_retry_after(&headers, &FixedClock(1_792_567_680 - 10)),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn parse_http_date_epoch_is_zero() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
    }

    #[test]
    fn parse_retry_after_returns_none_for_garbage() {
        let headers = vec![("retry-after".to_string(), "soon".to_string())];
        assert_eq!(parse_retry_after(&headers, &FixedClock(0)), None);
    }

    #[test]
    fn parse_retry_after_returns_none_when_header_missing() {
        assert_eq!(parse_retry_after(&[], &FixedClock(0)), None);
    }

    // -- jitter / backoff ---------------------------------------------------

    #[test]
    fn jittered_backoff_never_exceeds_the_cap() {
        let cap = Duration::from_millis(500);
        for attempt in 0..5 {
            for _ in 0..20 {
                let d = jittered_backoff(attempt, Duration::from_millis(200), cap);
                assert!(d <= cap, "attempt {attempt}: {d:?} exceeds cap {cap:?}");
            }
        }
    }

    #[test]
    fn jitter_millis_is_bounded() {
        for _ in 0..50 {
            assert!(jitter_millis(100) < 100);
        }
        assert_eq!(jitter_millis(0), 0);
    }

    // -- Offline guard (#19) ------------------------------------------------

    fn mode_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::mode::MODE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn offline_guard_refuses_a_non_loopback_url_before_the_transport_runs() {
        let _g = mode_guard();
        crate::mode::set_current(crate::mode::Mode::Offline);

        let transport = ScriptedTransport::new(vec![ok("should never be reached")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);
        let policy = RetryPolicy::default();
        let env = RetryEnv { transport: &transport, sleeper: &sleeper, clock: &clock, policy: &policy };

        let err = post_json_with(
            &env,
            "https://api.openai.com/v1/responses",
            &[],
            &json!({}),
            Duration::from_secs(1),
            "openai",
        )
        .unwrap_err();

        crate::mode::set_current(crate::mode::Mode::Auto);

        assert_eq!(transport.call_count(), 0, "the guard must refuse before the transport is ever invoked");
        assert!(sleeper.recorded().is_empty());
        let msg = err.to_string();
        assert!(msg.contains("Offline"), "{msg}");
        assert!(!msg.contains('\u{2014}'), "rule 11: no em dash: {msg}");
    }

    #[test]
    fn offline_guard_names_localhost_with_guidance() {
        let _g = mode_guard();
        crate::mode::set_current(crate::mode::Mode::Offline);

        let transport = ScriptedTransport::new(vec![]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);
        let policy = RetryPolicy::default();
        let env = RetryEnv { transport: &transport, sleeper: &sleeper, clock: &clock, policy: &policy };

        let err = post_json_with(
            &env,
            "http://localhost:11434/api/chat",
            &[],
            &json!({}),
            Duration::from_secs(1),
            "ollama",
        )
        .unwrap_err();

        crate::mode::set_current(crate::mode::Mode::Auto);

        assert_eq!(transport.call_count(), 0);
        let msg = err.to_string();
        assert!(msg.contains("127.0.0.1"), "should point at the fix: {msg}");
    }

    #[test]
    fn offline_guard_allows_a_loopback_url_while_offline() {
        let _g = mode_guard();
        crate::mode::set_current(crate::mode::Mode::Offline);

        let transport = ScriptedTransport::new(vec![ok("done")]);
        let sleeper = RecordingSleeper::new();
        let clock = FixedClock(0);
        let policy = RetryPolicy::default();
        let env = RetryEnv { transport: &transport, sleeper: &sleeper, clock: &clock, policy: &policy };

        let result = post_json_with(
            &env,
            "http://127.0.0.1:11434/api/chat",
            &[],
            &json!({}),
            Duration::from_secs(1),
            "ollama",
        );

        crate::mode::set_current(crate::mode::Mode::Auto);

        assert_eq!(result.unwrap(), "done");
        assert_eq!(transport.call_count(), 1);
    }

    #[test]
    fn offline_guard_is_inert_outside_offline_mode() {
        let _g = mode_guard();
        for mode in [crate::mode::Mode::Cloud, crate::mode::Mode::Local, crate::mode::Mode::Auto] {
            crate::mode::set_current(mode);

            let transport = ScriptedTransport::new(vec![ok("done")]);
            let sleeper = RecordingSleeper::new();
            let clock = FixedClock(0);
            let policy = RetryPolicy::default();
            let env = RetryEnv { transport: &transport, sleeper: &sleeper, clock: &clock, policy: &policy };

            let result = post_json_with(
                &env,
                "https://api.openai.com/v1/responses",
                &[],
                &json!({}),
                Duration::from_secs(1),
                "openai",
            );

            assert_eq!(result.unwrap(), "done", "mode {mode:?} must not block a non-loopback request");
            assert_eq!(transport.call_count(), 1, "mode {mode:?}");
        }
        crate::mode::set_current(crate::mode::Mode::Auto);
    }

    /// #19: the guard lives entirely in this file. A provider that called
    /// `ureq::` directly would bypass it -- this grep is the enforcement,
    /// since Rust's module privacy alone can't stop a sibling module from
    /// importing the crate and reaching the network its own way.
    #[test]
    fn no_provider_file_calls_ureq_directly_outside_common_rs() {
        let provider_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/provider");
        let entries = std::fs::read_dir(&provider_dir).expect("read src/provider");
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.file_name().and_then(|n| n.to_str()) == Some("common.rs") {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read provider file");
            assert!(
                !text.contains("ureq::"),
                "{} calls ureq:: directly; every HTTP send must go through \
                 provider::common so the Offline guard (issue #19) cannot be bypassed",
                path.display()
            );
        }
    }
}
