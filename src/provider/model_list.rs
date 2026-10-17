//! Live model lists for the cloud providers (#380).
//!
//! Settings asks each configured cloud provider which models it offers, so a
//! model released after this build still shows up in the dropdown. The
//! parsers here are pure (a JSON body in, ids out) and tested against
//! fixtures written from each vendor's documented response shape; the one
//! function that touches the network goes through
//! `provider::common::get_json_with_headers`, so the Offline guard and the
//! egress log apply.
//!
//! The ids are filtered to models that can plausibly read an image and
//! answer in text, since that is what Wingman sends. Neither OpenAI's nor
//! Gemini's list says outright which models take images, so those two are
//! filtered by name and by `supportedGenerationMethods` (Gemini). THEORY
//! (unverified): a few text-only models (for example an older OpenAI
//! reasoning model) still get through the name filter and fail at request
//! time with the vendor's own error.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use super::common;

/// The cloud vendors whose model list Settings can refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    OpenAi,
    Anthropic,
    Gemini,
}

impl Vendor {
    /// The short tag the egress log and diagnostics show for this vendor.
    pub fn tag(self) -> &'static str {
        match self {
            Vendor::OpenAi => "openai models",
            Vendor::Anthropic => "anthropic models",
            Vendor::Gemini => "gemini models",
        }
    }
}

const LIST_TIMEOUT: Duration = Duration::from_secs(10);
const OPENAI_URL: &str = "https://api.openai.com/v1/models";
/// `limit=1000` is the documented maximum, so one page is normally the whole
/// list; [`next_page_url`] follows `has_more` if it ever is not.
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/models?limit=1000";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// `pageSize=1000` is the documented maximum; `nextPageToken` is followed.
const GEMINI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000";
/// A vendor that keeps saying "more" stops here: a runaway list is a bug on
/// their side, and the pages read so far are still a usable list.
const MAX_PAGES: usize = 10;
/// Stop following pages once this many bytes have been read in all.
const MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// The page to read next, or `None` to stop: there is no next page, the
/// vendor sent the URL just read again (a loop), or enough has been read.
fn follow(current: &str, next: Option<String>, total_bytes: usize) -> Option<String> {
    let next = next?;
    (next != current && total_bytes < MAX_TOTAL_BYTES).then_some(next)
}

/// Asks the vendor for its model list. An empty result counts as a failure,
/// so the caller keeps its built-in list instead of showing an empty
/// dropdown. The key goes in a header, never the URL.
pub fn fetch(vendor: Vendor, api_key: &str) -> Result<Vec<String>> {
    fetch_with(vendor, api_key, &|url, headers| {
        common::get_json_with_headers(url, headers, LIST_TIMEOUT, vendor.tag())
    })
}

/// One GET of a list page: the URL, the request headers, the body or why not.
/// The seam the key check's tests put a fake transport behind.
type GetPage<'a> = &'a dyn Fn(&str, &[(&str, &str)]) -> Result<String>;

fn fetch_with(vendor: Vendor, api_key: &str, get: GetPage) -> Result<Vec<String>> {
    fetch_pages(vendor, api_key, get, MAX_PAGES).map(|(ids, _)| ids)
}

/// [`fetch_with`], reading at most `max_pages` pages. The flag is true when the
/// vendor had no further page to give, so the ids are the whole list.
fn fetch_pages(
    vendor: Vendor,
    api_key: &str,
    get: GetPage,
    max_pages: usize,
) -> Result<(Vec<String>, bool)> {
    let mut url = match vendor {
        Vendor::OpenAi => OPENAI_URL.to_string(),
        Vendor::Anthropic => ANTHROPIC_URL.to_string(),
        Vendor::Gemini => GEMINI_URL.to_string(),
    };
    let auth = format!("Bearer {api_key}");
    let mut bodies: Vec<String> = Vec::new();
    let mut total_bytes = 0usize;
    let mut complete = false;
    for _ in 0..max_pages {
        let body = match vendor {
            Vendor::OpenAi => get(&url, &[("Authorization", auth.as_str())]),
            Vendor::Anthropic => get(
                &url,
                &[
                    ("x-api-key", api_key),
                    ("anthropic-version", ANTHROPIC_VERSION),
                ],
            ),
            Vendor::Gemini => get(&url, &[("x-goog-api-key", api_key)]),
        };
        let body = match body {
            Ok(b) => b,
            // A later page failing keeps what was read; the first failing is
            // a failure.
            Err(e) if bodies.is_empty() => return Err(e),
            Err(_) => break,
        };
        let next = next_page_url(vendor, &body);
        total_bytes += body.len();
        bodies.push(body);
        match follow(&url, next, total_bytes) {
            Some(n) => url = n,
            None => {
                complete = true;
                break;
            }
        }
    }
    let ids = parse_pages(vendor, &bodies)?;
    if ids.is_empty() {
        return Err(anyhow!("{}: the list had no usable models", vendor.tag()));
    }
    Ok((ids, complete))
}

/// What asking the vendor's list endpoint with a typed key found out (#876).
#[derive(Debug)]
pub enum KeyCheck {
    /// 2xx: the vendor knows the key. The first page of its model list came with
    /// the answer; the flag says that page was the whole list.
    Accepted(Vec<String>, bool),
    /// 401 or 403 (or Gemini's 400 `API_KEY_INVALID`): the vendor refused it.
    Rejected,
    /// No network, a timeout, 5xx, a rate limit, an answer that is not a list:
    /// nothing is known about the key.
    Unchecked,
}

/// The cheapest check of a key: the read-only model-list request Settings makes
/// anyway, through the guarded transport (Offline guard, egress log). Never
/// called from the UI thread. No key text is in the result.
#[cfg_attr(test, allow(dead_code))] // a test build's windows use a fake instead
pub fn check_key(vendor: Vendor, api_key: &str) -> KeyCheck {
    classify(check_pages(vendor, api_key, &|url, headers| {
        common::get_json_with_headers(url, headers, LIST_TIMEOUT, vendor.tag())
    }))
}

/// #1140: one page answers "does the vendor know this key", so the check
/// never waits on the rest of a paginated list (up to `MAX_PAGES` x
/// `LIST_TIMEOUT`). A list that goes on is fetched in full afterwards.
fn check_pages(vendor: Vendor, api_key: &str, get: GetPage) -> Result<(Vec<String>, bool)> {
    fetch_pages(vendor, api_key, get, 1)
}

#[cfg(test)]
fn check_key_with(vendor: Vendor, api_key: &str, get: GetPage) -> KeyCheck {
    classify(check_pages(vendor, api_key, get))
}

fn classify(result: Result<(Vec<String>, bool)>) -> KeyCheck {
    let err = match result {
        Ok((ids, complete)) => return KeyCheck::Accepted(ids, complete),
        Err(err) => err,
    };
    let refused = err
        .downcast_ref::<common::HttpStatus>()
        .is_some_and(|http| match http.status {
            401 | 403 => true,
            // Gemini says a bad key with a 400 and a reason in the body.
            400 => err.to_string().contains(common::KIND_KEY_INVALID),
            _ => false,
        });
    if refused {
        KeyCheck::Rejected
    } else {
        KeyCheck::Unchecked
    }
}

/// The URL of the page after the one that returned `body`, if the vendor says
/// there is one. OpenAI's list is a single page.
pub fn next_page_url(vendor: Vendor, body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    match vendor {
        Vendor::OpenAi => None,
        Vendor::Anthropic => {
            if v.get("has_more").and_then(Value::as_bool) != Some(true) {
                return None;
            }
            let last = v.get("last_id").and_then(Value::as_str)?;
            (!last.is_empty()).then(|| format!("{ANTHROPIC_URL}&after_id={}", percent_encode(last)))
        }
        Vendor::Gemini => {
            let token = v.get("nextPageToken").and_then(Value::as_str)?;
            (!token.is_empty()).then(|| format!("{GEMINI_URL}&pageToken={}", percent_encode(token)))
        }
    }
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Parses every page as one list, so the newest-first sort and the
/// dated-snapshot filter see all the models together. A page that is not
/// valid is an error only if it is the first.
pub fn parse_pages(vendor: Vendor, bodies: &[String]) -> Result<Vec<String>> {
    type Parser = fn(&str) -> Result<Vec<String>>;
    let (key, parse): (&str, Parser) = match vendor {
        Vendor::OpenAi => ("data", parse_openai),
        Vendor::Anthropic => ("data", parse_anthropic),
        Vendor::Gemini => ("models", parse_gemini),
    };
    let mut all: Vec<Value> = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        match data_array(body, key) {
            Ok(items) => all.extend(items),
            Err(e) if i == 0 => return Err(e),
            Err(_) => {}
        }
    }
    let mut combined = serde_json::Map::new();
    combined.insert(key.to_string(), Value::Array(all));
    parse(&Value::Object(combined).to_string())
}

/// `GET /v1/models`: `{"data": [{"id", "created", ...}]}`. Ids only, no
/// capability flags, so this keeps chat-style ids by name, drops dated
/// snapshots whose undated alias is also listed, and sorts newest first.
pub fn parse_openai(body: &str) -> Result<Vec<String>> {
    let data = data_array(body, "data")?;
    let all: Vec<&str> = data
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .collect();
    let mut kept: Vec<(i64, &str)> = data
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            let chat = id.starts_with("gpt-")
                || id.starts_with("chatgpt-")
                || (id.starts_with('o') && id[1..].starts_with(|c: char| c.is_ascii_digit()));
            if !chat || NOT_CHAT.iter().any(|w| id.contains(w)) {
                return None;
            }
            if let Some(alias) = undated_alias(id) {
                if all.contains(&alias) {
                    return None;
                }
            }
            Some((m.get("created").and_then(Value::as_i64).unwrap_or(0), id))
        })
        .collect();
    // Newest first; the id breaks a tie so the order never depends on the
    // vendor's.
    kept.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    Ok(kept.into_iter().map(|(_, id)| id.to_string()).collect())
}

/// Words that mark a model that is not a text-and-image chat model.
const NOT_CHAT: [&str; 11] = [
    "audio",
    "realtime",
    "transcribe",
    "tts",
    "image",
    "instruct",
    "embedding",
    "moderation",
    "whisper",
    "dall-e",
    "search",
];

/// `id` without a trailing `-YYYY-MM-DD` snapshot date, if it has one.
fn undated_alias(id: &str) -> Option<&str> {
    let (head, tail) = id
        .len()
        .checked_sub(11)
        .and_then(|i| id.split_at_checked(i))?;
    let parts: Vec<&str> = tail.strip_prefix('-')?.split('-').collect();
    let shape_ok = parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()));
    shape_ok.then_some(head)
}

/// The array under `key` in a model-list response, or why there is none.
fn data_array(body: &str, key: &str) -> Result<Vec<Value>> {
    let v: Value = serde_json::from_str(body).context("model list: not valid JSON")?;
    v.get(key)
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| anyhow!("model list: no \"{key}\" array in the response"))
}

/// `GET /v1/models`: `{"data": [{"id", "created_at", "capabilities": {...}}]}`.
/// Drops a model whose `capabilities.image_input.supported` is false; a model
/// with no `capabilities` object is kept. Newest first.
pub fn parse_anthropic(body: &str) -> Result<Vec<String>> {
    let data = data_array(body, "data")?;
    let mut kept: Vec<(&str, &str)> = data
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            let image_ok = m
                .pointer("/capabilities/image_input/supported")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let created = m.get("created_at").and_then(Value::as_str).unwrap_or("");
            image_ok.then_some((created, id))
        })
        .collect();
    // RFC 3339 timestamps sort correctly as text; a missing one sorts last.
    kept.sort_by(|a, b| b.0.cmp(a.0).then_with(|| a.1.cmp(b.1)));
    Ok(kept.into_iter().map(|(_, id)| id.to_string()).collect())
}

/// `GET /v1beta/models`: `{"models": [{"name": "models/<id>",
/// "supportedGenerationMethods": [...]}]}`. Keeps `gemini-*` models that
/// support `generateContent` and are not speech or image generators.
pub fn parse_gemini(body: &str) -> Result<Vec<String>> {
    let models = data_array(body, "models")?;
    let mut kept: Vec<&str> = models
        .iter()
        .filter_map(|m| {
            let id = m.get("name").and_then(Value::as_str)?;
            let id = id.strip_prefix("models/").unwrap_or(id);
            let generates = m
                .get("supportedGenerationMethods")
                .and_then(Value::as_array)
                .is_some_and(|ms| ms.iter().any(|v| v.as_str() == Some("generateContent")));
            let chat = id.starts_with("gemini-")
                && !["tts", "image", "audio", "embedding", "live"]
                    .iter()
                    .any(|w| id.contains(w));
            (generates && chat).then_some(id)
        })
        .collect();
    // No date in the response, so a descending id puts a higher version first.
    kept.sort_unstable_by(|a, b| b.cmp(a));
    Ok(kept.into_iter().map(str::to_string).collect())
}

/// The dropdown's list: the fetched ids first, then any configured model the
/// vendor did not list (a hand-added or since-retired id stays selectable).
/// The user's current model is never dropped: the caller's combo-fill puts
/// it at the top if it is in neither list.
pub fn merge(fetched: &[String], configured: &[String]) -> Vec<String> {
    let mut out: Vec<String> = fetched.to_vec();
    for c in configured {
        if !out.contains(c) {
            out.push(c.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("tests/fixtures/{name}"))
            .expect("fixture file should exist")
            .replace('\r', "")
    }

    #[test]
    fn openai_keeps_chat_models_newest_first_and_drops_the_rest() {
        let ids = parse_openai(&fixture("openai_models.json")).unwrap();
        assert_eq!(
            ids,
            [
                "gpt-5.9-future",
                "gpt-5-mini",
                "gpt-5",
                "o3",
                "chatgpt-4o-latest",
                "gpt-4o-mini",
                "gpt-4o",
            ]
        );
    }

    #[test]
    fn openai_drops_audio_image_embedding_and_moderation_models() {
        let ids = parse_openai(&fixture("openai_models.json")).unwrap();
        for bad in [
            "gpt-4o-realtime-preview",
            "gpt-4o-audio-preview",
            "gpt-4o-transcribe",
            "gpt-4o-mini-tts",
            "gpt-image-1",
            "gpt-3.5-turbo-instruct",
            "text-embedding-3-small",
            "omni-moderation-latest",
            "whisper-1",
            "dall-e-3",
            "tts-1",
        ] {
            assert!(!ids.iter().any(|i| i == bad), "{bad} should be filtered");
        }
    }

    #[test]
    fn openai_drops_a_dated_snapshot_when_its_alias_is_listed() {
        let ids = parse_openai(&fixture("openai_models.json")).unwrap();
        assert!(!ids.iter().any(|i| i == "gpt-4o-2024-08-06"));
    }

    #[test]
    fn openai_keeps_a_dated_snapshot_when_no_alias_is_listed() {
        let body = r#"{"data":[{"id":"gpt-9-2030-01-02","created":5}]}"#;
        assert_eq!(parse_openai(body).unwrap(), ["gpt-9-2030-01-02"]);
    }

    #[test]
    fn anthropic_keeps_image_capable_models_newest_first() {
        let ids = parse_anthropic(&fixture("anthropic_models.json")).unwrap();
        assert_eq!(
            ids,
            [
                "claude-future-9-0",
                "claude-haiku-5-5",
                "claude-no-capabilities"
            ]
        );
    }

    #[test]
    fn anthropic_drops_a_model_that_cannot_read_images() {
        let ids = parse_anthropic(&fixture("anthropic_models.json")).unwrap();
        assert!(!ids.iter().any(|i| i == "claude-text-only-1"));
    }

    #[test]
    fn gemini_keeps_generate_content_models_and_strips_the_prefix() {
        let ids = parse_gemini(&fixture("gemini_models.json")).unwrap();
        assert_eq!(
            ids,
            ["gemini-9.0-future", "gemini-2.5-pro", "gemini-2.5-flash"]
        );
    }

    #[test]
    fn gemini_drops_speech_image_embedding_and_non_generate_models() {
        let ids = parse_gemini(&fixture("gemini_models.json")).unwrap();
        for bad in [
            "gemini-2.5-flash-preview-tts",
            "gemini-2.5-flash-image",
            "gemini-embedding-001",
            "embedding-gecko-001",
            "imagen-4.0-generate-001",
            "aqa",
        ] {
            assert!(!ids.iter().any(|i| i == bad), "{bad} should be filtered");
        }
    }

    #[test]
    fn every_parser_rejects_bad_json_and_a_wrong_shape() {
        for parse in [parse_openai, parse_anthropic, parse_gemini] {
            assert!(parse("not json").is_err());
            assert!(parse(r#"{"error":{"message":"nope"}}"#).is_err());
        }
    }

    /// One real `GET /v1/models`, run by hand with `ANTHROPIC_API_KEY` set:
    /// `cargo test anthropic_live_list -- --ignored --nocapture`.
    #[test]
    #[ignore = "calls the real Anthropic API"]
    fn anthropic_live_list_parses_the_real_response() {
        let key = std::env::var("ANTHROPIC_API_KEY").expect("set ANTHROPIC_API_KEY");
        let ids = fetch(Vendor::Anthropic, &key).expect("live fetch");
        println!("live anthropic ids ({}): {ids:?}", ids.len());
        assert!(ids.iter().any(|i| i.starts_with("claude-")));
    }

    #[test]
    fn anthropic_follows_has_more_with_the_last_id() {
        let body = r#"{"data":[],"has_more":true,"last_id":"claude-x"}"#;
        let url = next_page_url(Vendor::Anthropic, body).unwrap();
        assert!(url.ends_with("&after_id=claude-x"), "{url}");
        let done = r#"{"data":[],"has_more":false,"last_id":"claude-x"}"#;
        assert_eq!(next_page_url(Vendor::Anthropic, done), None);
        assert_eq!(next_page_url(Vendor::Anthropic, "not json"), None);
    }

    #[test]
    fn gemini_follows_a_next_page_token_and_encodes_it() {
        let body = r#"{"models":[],"nextPageToken":"a+b/c=="}"#;
        let url = next_page_url(Vendor::Gemini, body).unwrap();
        assert!(url.ends_with("&pageToken=a%2Bb%2Fc%3D%3D"), "{url}");
        assert_eq!(
            next_page_url(Vendor::Gemini, r#"{"models":[],"nextPageToken":""}"#),
            None
        );
        assert_eq!(next_page_url(Vendor::Gemini, r#"{"models":[]}"#), None);
    }

    #[test]
    fn paging_stops_on_a_repeated_url_and_on_too_many_bytes() {
        let a = "https://x/models?p=1".to_string();
        let b = "https://x/models?p=2".to_string();
        assert_eq!(follow(&a, Some(b.clone()), 10), Some(b.clone()));
        // The vendor repeats the token it just gave: stop.
        assert_eq!(follow(&b, Some(b.clone()), 10), None);
        assert_eq!(follow(&a, None, 10), None);
        assert_eq!(follow(&a, Some(b), MAX_TOTAL_BYTES), None);
    }

    #[test]
    fn openai_has_no_second_page() {
        assert_eq!(
            next_page_url(Vendor::OpenAi, r#"{"data":[],"has_more":true}"#),
            None
        );
    }

    #[test]
    fn pages_are_sorted_together() {
        let p1 = r#"{"data":[{"id":"claude-a","created_at":"2025-01-01T00:00:00Z"}]}"#.to_string();
        let p2 = r#"{"data":[{"id":"claude-b","created_at":"2026-01-01T00:00:00Z"}]}"#.to_string();
        assert_eq!(
            parse_pages(Vendor::Anthropic, &[p1.clone(), p2]).unwrap(),
            ["claude-b", "claude-a"]
        );
        // A broken later page keeps the earlier ones; a broken first is an error.
        assert_eq!(
            parse_pages(Vendor::Anthropic, &[p1, "oops".to_string()]).unwrap(),
            ["claude-a"]
        );
        assert!(parse_pages(Vendor::Anthropic, &["oops".to_string()]).is_err());
    }

    #[test]
    fn merge_puts_fetched_first_then_configured_extras_without_duplicates() {
        let fetched = vec!["new-1".to_string(), "shared".to_string()];
        let configured = vec!["shared".to_string(), "hand-added".to_string()];
        assert_eq!(
            merge(&fetched, &configured),
            ["new-1", "shared", "hand-added"]
        );
    }

    #[test]
    fn merge_with_nothing_fetched_is_the_configured_list() {
        let configured = vec!["a".to_string(), "b".to_string()];
        assert_eq!(merge(&[], &configured), ["a", "b"]);
    }
}

/// What asking a vendor about a typed key found out (#876).
#[cfg(test)]
mod key_check_tests {
    use super::*;
    use crate::provider::common::{http_error, HttpStatus};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("tests/fixtures/{name}"))
            .expect("fixture file should exist")
            .replace('\r', "")
    }

    /// What the guarded transport hands back for a non-2xx answer.
    fn status_error(code: u16, body: &str) -> anyhow::Error {
        HttpStatus::wrap(http_error("openai models", code, body), code)
    }

    #[test]
    fn a_good_key_is_accepted_and_brings_the_list() {
        let body = fixture("openai_models.json");
        let seen = std::cell::RefCell::new(Vec::new());
        let got = check_key_with(Vendor::OpenAi, "sk-good", &|url, headers| {
            seen.borrow_mut().push((url.to_string(), headers.len()));
            Ok(body.clone())
        });
        match got {
            KeyCheck::Accepted(ids, _) => assert!(ids.contains(&"gpt-5".to_string())),
            other => panic!("expected Accepted, got {other:?}"),
        }
        // One cheap read-only request, to the vendor's own list endpoint.
        let seen = seen.borrow();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].0.starts_with("https://api.openai.com/"));
    }

    /// #1140: the check needs one answer, not the whole paginated list.
    #[test]
    fn a_key_check_reads_one_page_even_when_the_vendor_says_more() {
        let anthropic = r#"{"data":[{"id":"claude-sonnet-5-5","created_at":"2026-01-01T00:00:00Z"}],"has_more":true,"last_id":"claude-sonnet-5-5"}"#;
        let gemini = r#"{"models":[{"name":"models/gemini-3-pro","supportedGenerationMethods":["generateContent"]}],"nextPageToken":"abc"}"#;
        for (vendor, body) in [(Vendor::Anthropic, anthropic), (Vendor::Gemini, gemini)] {
            let calls = std::cell::Cell::new(0);
            let got = check_key_with(vendor, "key", &|_, _| {
                calls.set(calls.get() + 1);
                Ok(body.to_string())
            });
            assert!(
                matches!(got, KeyCheck::Accepted(_, false)),
                "{vendor:?}: {got:?}"
            );
            assert_eq!(calls.get(), 1, "{vendor:?} followed a next page");
        }
        // The full list fetch still follows pages.
        let calls = std::cell::Cell::new(0);
        let _ = fetch_with(Vendor::Anthropic, "key", &|_, _| {
            calls.set(calls.get() + 1);
            Ok(anthropic.to_string())
        });
        assert!(calls.get() > 1, "the full fetch must keep following pages");
    }

    #[test]
    fn a_401_or_403_is_a_rejection_for_every_vendor() {
        for vendor in [Vendor::OpenAi, Vendor::Anthropic, Vendor::Gemini] {
            for code in [401u16, 403] {
                let got = check_key_with(vendor, "key", &|_, _| {
                    Err(status_error(code, r#"{"error":{"message":"no"}}"#))
                });
                assert!(matches!(got, KeyCheck::Rejected), "{vendor:?} {code}");
            }
        }
    }

    /// Gemini answers a bad key with 400 and `API_KEY_INVALID`, not 401.
    #[test]
    fn geminis_400_api_key_invalid_is_a_rejection() {
        let body = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","details":[{"reason":"API_KEY_INVALID"}]}}"#;
        let got = check_key_with(Vendor::Gemini, "AIza", &|_, _| Err(status_error(400, body)));
        assert!(matches!(got, KeyCheck::Rejected));
        // Any other 400 says nothing about the key.
        let got = check_key_with(Vendor::Gemini, "AIza", &|_, _| {
            Err(status_error(
                400,
                r#"{"error":{"message":"bad page token"}}"#,
            ))
        });
        assert!(matches!(got, KeyCheck::Unchecked));
    }

    /// Only a refusal blocks: a request that could not finish says nothing.
    #[test]
    fn everything_else_leaves_the_key_unchecked() {
        let cases = [
            status_error(429, "{}"),
            status_error(500, "oops"),
            status_error(503, "oops"),
            status_error(404, "{}"),
            anyhow::anyhow!("openai models: transport error: timed out"),
        ];
        for err in cases {
            let text = format!("{err:#}");
            let err = std::cell::RefCell::new(Some(err));
            let got = check_key_with(Vendor::OpenAi, "sk-x", &|_, _| {
                Err(err.borrow_mut().take().unwrap())
            });
            assert!(matches!(got, KeyCheck::Unchecked), "{text}");
        }
    }

    /// A 2xx whose body is not a list proves nothing about the key.
    #[test]
    fn an_answer_with_no_usable_list_is_unchecked() {
        let got = check_key_with(Vendor::OpenAi, "sk-x", &|_, _| Ok("not json".to_string()));
        assert!(matches!(got, KeyCheck::Unchecked));
        let got = check_key_with(Vendor::OpenAi, "sk-x", &|_, _| {
            Ok(r#"{"data":[]}"#.to_string())
        });
        assert!(matches!(got, KeyCheck::Unchecked));
    }

    /// The key travels in a header and only to that vendor's own host.
    #[test]
    fn the_key_goes_in_a_header_to_the_vendors_own_host() {
        for (vendor, host) in [
            (Vendor::OpenAi, "api.openai.com"),
            (Vendor::Anthropic, "api.anthropic.com"),
            (Vendor::Gemini, "generativelanguage.googleapis.com"),
        ] {
            let _ = check_key_with(vendor, "SECRETKEY123", &|url, headers| {
                assert!(url.contains(host), "{url}");
                assert!(!url.contains("SECRETKEY123"), "key in the URL: {url}");
                assert!(headers.iter().any(|(_, v)| v.contains("SECRETKEY123")));
                Err(anyhow::anyhow!("down"))
            });
        }
    }
}
