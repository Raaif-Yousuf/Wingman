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

use anyhow::{anyhow, Result};
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
/// transport-level failure (DNS, connect, timeout, a dropped connection
/// while reading the body) -- never a non-2xx status, which is a normal
/// `Ok(RawResponse)` with `status` set accordingly.
pub(crate) struct RawResponse {
    pub status: u16,
    /// Header names as the server sent them; look up with
    /// [`find_header`] (case-insensitive) rather than indexing directly.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

pub(crate) trait Transport {
    fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
        timeout: Duration,
    ) -> std::result::Result<RawResponse, String>;
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
    ) -> std::result::Result<RawResponse, String> {
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
            .map_err(|e| e.to_string())?;

        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_string(), value.to_str().unwrap_or_default().to_string()))
            .collect();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("failed to read response body: {e}"))?;

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
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_millis(200),
            max_total_backoff: Duration::from_secs(3),
            max_retry_after: Duration::from_secs(5),
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
///   `policy.max_total_backoff` cumulative sleep.
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
    let policy = env.policy;
    let mut retries = 0u32;
    let mut backoff_spent = Duration::ZERO;
    let mut retried_429 = false;

    loop {
        match env.transport.post_json(url, headers, body, timeout) {
            Err(transport_err) => {
                let remaining = policy.max_total_backoff.saturating_sub(backoff_spent);
                if retries < policy.max_retries && !remaining.is_zero() {
                    let delay = jittered_backoff(retries, policy.base_delay, remaining);
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
                    if !retried_429 {
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
                    let remaining = policy.max_total_backoff.saturating_sub(backoff_spent);
                    if retries < policy.max_retries && !remaining.is_zero() {
                        let delay = jittered_backoff(retries, policy.base_delay, remaining);
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
        responses: Mutex<Vec<std::result::Result<RawResponse, String>>>,
        calls: Mutex<u32>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<std::result::Result<RawResponse, String>>) -> Self {
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
        ) -> std::result::Result<RawResponse, String> {
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

    fn ok(body: &str) -> std::result::Result<RawResponse, String> {
        Ok(RawResponse {
            status: 200,
            headers: vec![],
            body: body.to_string(),
        })
    }

    fn status(code: u16, headers: Vec<(&str, &str)>, body: &str) -> std::result::Result<RawResponse, String> {
        Ok(RawResponse {
            status: code,
            headers: headers.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: body.to_string(),
        })
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
        let transport = ScriptedTransport::new(vec![Err("connection refused".to_string()), ok("done")]);
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
            ScriptedTransport::new(vec![Err("e1".to_string()), Err("e2".to_string()), Err("e3".to_string())]);
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
}
