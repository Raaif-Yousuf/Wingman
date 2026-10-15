//! Fetching the page behind a link (#127), and the rules for which links may
//! be fetched at all. A link on screen is untrusted input: fetching it must
//! never make Wingman probe the user's own machine or network, never follow a
//! redirect into one, and never read without bound.
//!
//! The policy, in the order it is applied:
//!
//! 1. **Mode.** Offline and Local keep everything on this computer (the Modes
//!    table in the expansion plan), so neither fetches. This is checked
//!    before any lookup, and the Offline guard in `provider::common` is
//!    called as a backstop on every hop.
//! 2. **The link.** `https` only, port 443 only, no `user@` part, a real DNS
//!    name (no IP literal in any spelling, no `localhost`, no single-label or
//!    `.local`/`.internal`-style names). See [`parse_target`].
//! 3. **The address.** The name is looked up once, every address must be
//!    public ([`is_public_ip`]), and the connection is then pinned to exactly
//!    those addresses, so a DNS answer that changes between the check and the
//!    connection (rebinding) cannot reach a private address.
//! 4. **Redirects.** At most [`MAX_REDIRECTS`], each one run through steps
//!    1 to 3 again, and a redirect to `http` is refused.
//! 5. **The answer.** Only `text/html`, `application/xhtml+xml` and
//!    `text/plain`; at most [`MAX_BODY_BYTES`] read; the whole fetch must end
//!    within [`TOTAL_TIMEOUT`]. HTML is reduced to text by
//!    [`super::html_text`].
//!
//! Every hop is one row in the local egress log (`egress.log`): the host and
//! the bytes received, never the path, query or content. A fetch is a plain
//! GET with no cookies, no credentials and no proxy; it is not billed, so the
//! daily budget guard (`budget_guard`, which only the model calls reach)
//! does not apply to it.
//!
//! The network sits behind the [`Net`] trait so every rule above is tested
//! without a socket.

use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Redirects followed before giving up.
pub const MAX_REDIRECTS: usize = 4;
/// Most response bytes read; the rest is not downloaded.
pub const MAX_BODY_BYTES: usize = 1_000_000;
/// The whole fetch, lookups and redirects included.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
/// Most characters of page text handed to the model.
pub const MAX_TEXT_CHARS: usize = 20_000;

/// A link that passed [`parse_target`]: the exact `https` URL that will be
/// requested, and its host in the ASCII form (punycode for non-English
/// names), which is what the confirmation card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub url: String,
    /// The path and query, starting with `/` (used to resolve relative
    /// redirects).
    pub path_and_query: String,
}

/// The web addresses (`http://` or `https://`) in `text`, in order, each
/// once. Trailing sentence punctuation and an unbalanced closing bracket are
/// not part of the link.
pub fn find_links(text: &str) -> Vec<String> {
    // ASCII lower-casing keeps every byte offset, so `lower` indexes `text`.
    let lower = text.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut resume = 0;
    for (start, _) in lower.match_indices("http") {
        if start < resume {
            continue;
        }
        let scheme_len = if lower[start..].starts_with("https://") {
            8
        } else if lower[start..].starts_with("http://") {
            7
        } else {
            continue;
        };
        // Not the tail of a longer word (`xhttps://`).
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let rest = &text[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c.is_control() || "<>\"\\`".contains(c))
            .unwrap_or(rest.len());
        let link = trim_link_end(&rest[..end]);
        resume = start + end;
        if link.len() > scheme_len && !out.iter().any(|l| l == link) {
            out.push(link.to_string());
        }
    }
    out
}

/// `link` without the sentence punctuation or unbalanced closing bracket a
/// writer put after it.
fn trim_link_end(link: &str) -> &str {
    let mut link = link;
    while let Some(last) = link.chars().next_back() {
        let drop = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' => true,
            ')' | ']' | '}' => {
                let open = match last {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                link.matches(last).count() > link.matches(open).count()
            }
            _ => false,
        };
        if !drop {
            break;
        }
        link = &link[..link.len() - last.len_utf8()];
    }
    link
}

const NOT_A_LINK: &str = "That is not a web link Wingman can fetch.";
const HTTPS_ONLY: &str =
    "Wingman only fetches https links. This one is http, which is not encrypted.";
const UNUSUAL: &str = "The link has characters Wingman will not fetch.";
const USERINFO: &str =
    "The link has a name before the site, which can disguise where it goes. Wingman did not fetch it.";
const PORT: &str = "Wingman only fetches links on the standard https port.";
const BAD_HOST: &str =
    "The link does not point to a public website name, so Wingman did not fetch it.";
const TOO_LONG: &str = "The link is too long for Wingman to fetch.";
const MAX_URL_CHARS: usize = 2048;

/// Name endings that are never public websites.
const LOCAL_SUFFIXES: &[&str] = &[
    "localhost",
    "local",
    "internal",
    "lan",
    "corp",
    "home",
    "intranet",
    "localdomain",
    "private",
    "arpa",
];

/// Checks `raw` against the link rules and returns what to fetch, or the
/// sentence to show the user when it may not be fetched.
pub fn parse_target(raw: &str) -> Result<Target, String> {
    let raw = raw.trim();
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) || raw.contains('\\') {
        return Err(UNUSUAL.into());
    }
    let lower_prefix = |p: &str| {
        raw.get(..p.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(p))
    };
    let rest = if lower_prefix("https://") {
        &raw[8..]
    } else if lower_prefix("http://") {
        return Err(HTTPS_ONLY.into());
    } else {
        return Err(NOT_A_LINK.into());
    };
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    if authority.contains('@') {
        return Err(USERINFO.into());
    }
    if authority.starts_with('[') {
        return Err(BAD_HOST.into());
    }
    let host_part = match authority.split_once(':') {
        Some((host, port)) => {
            if port != "443" {
                return Err(PORT.into());
            }
            host
        }
        None => authority,
    };
    let mut host = host_part.to_lowercase();
    if !host.is_ascii() {
        // The address on the wire: Punycode, as the read_qr host reader shows.
        host = crate::actions::read_qr::url_host(raw).ok_or(BAD_HOST)?;
    }
    let host = host.strip_suffix('.').unwrap_or(&host).to_string();
    check_host(&host)?;
    // Two readers must agree where the link goes.
    let browser = crate::actions::read_qr::url_host(raw).unwrap_or_default();
    if browser.strip_suffix('.').unwrap_or(&browser) != host {
        return Err(BAD_HOST.into());
    }

    let tail = tail.split('#').next().unwrap_or("");
    let mut path_and_query = match tail {
        "" => "/".to_string(),
        t if t.starts_with('?') => format!("/{t}"),
        t => t.to_string(),
    };
    if !path_and_query.is_ascii() {
        path_and_query = path_and_query
            .bytes()
            .map(|b| {
                if b.is_ascii() {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
    }
    let url = format!("https://{host}{path_and_query}");
    if url.len() > MAX_URL_CHARS {
        return Err(TOO_LONG.into());
    }
    Ok(Target {
        host,
        url,
        path_and_query,
    })
}

/// Whether `host` (lower-case, ASCII, no trailing dot) is a public website
/// name: letters, digits and hyphens in labels of 1 to 63 characters, at
/// least two labels, a last label that is not a number in any spelling, and
/// none of the local or internal endings.
fn check_host(host: &str) -> Result<(), String> {
    if host.is_empty() || host.len() > 253 {
        return Err(BAD_HOST.into());
    }
    let labels: Vec<&str> = host.split('.').collect();
    for label in &labels {
        let ok = (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return Err(BAD_HOST.into());
        }
    }
    let tld = labels[labels.len() - 1];
    // `2130706433`, `0x7f000001` and `127.1` are all addresses to a resolver.
    let numeric = tld.bytes().all(|b| b.is_ascii_digit())
        || tld
            .strip_prefix("0x")
            .is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()));
    if labels.len() < 2 || numeric || LOCAL_SUFFIXES.contains(&tld) {
        return Err(BAD_HOST.into());
    }
    Ok(())
}

/// Whether a connection to `ip` is allowed: a public, routable address.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_public(v4.octets()),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if s[..5] == [0; 5] {
                // `::ffff:a.b.c.d` is as public as the address inside it;
                // `::`, `::1` and the old `::a.b.c.d` form are never public.
                return s[5] == 0xffff
                    && v4_is_public(v6.octets()[12..].try_into().unwrap_or([0; 4]));
            }
            // Only global unicast (2000::/3), minus what it reserves.
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && s[1] < 0x0200) // protocol assignments, Teredo
                && !(s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                && s[0] != 0x2002 // 6to4 wraps an IPv4 address
                && !(s[0] == 0x3fff && s[1] < 0x1000) // documentation
        }
    }
}

fn v4_is_public([a, b, c, _]: [u8; 4]) -> bool {
    !(a == 0
        || a == 10
        || (a == 100 && (64..=127).contains(&b))
        || a == 127
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

/// The page text and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The host the text was finally read from (after any redirects).
    pub host: String,
    pub text: String,
    /// The text was cut to [`MAX_TEXT_CHARS`], or the download stopped at
    /// [`MAX_BODY_BYTES`].
    pub truncated: bool,
}

/// One response, before any interpretation.
#[derive(Debug, Clone, Default)]
pub struct Hop {
    pub status: u16,
    pub content_type: Option<String>,
    pub location: Option<String>,
    pub body: Vec<u8>,
    /// More body was available than [`MAX_BODY_BYTES`].
    pub body_truncated: bool,
}

/// The network, so the policy is testable without one.
pub trait Net {
    /// Looks `host` up. Must give up after `timeout`.
    fn resolve(&self, host: &str, timeout: Duration) -> Result<Vec<IpAddr>, String>;
    /// One GET, no redirects followed, connecting only to `addrs`.
    fn get(&self, target: &Target, addrs: &[IpAddr], timeout: Duration) -> Result<Hop, String>;
    /// Logs one hop to the egress log.
    fn record(&self, host: &str, bytes_received: usize, error: Option<&str>);
}

/// Fetches the page behind `raw_url` under the rules in the module docs.
pub fn fetch_with(net: &dyn Net, raw_url: &str) -> Result<Page, String> {
    allow_network(&crate::mode::current())?;
    let deadline = Instant::now() + TOTAL_TIMEOUT;
    let mut target = parse_target(raw_url)?;
    for _ in 0..=MAX_REDIRECTS {
        // Checked on every hop, so a mode switched mid-fetch is honoured.
        allow_network(&crate::mode::current())?;
        crate::provider::common::offline_guard(&target.url).map_err(|e| e.to_string())?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("{} took too long to answer.", target.host));
        }
        let host = target.host.clone();

        let addrs = match net.resolve(&host, remaining) {
            Ok(addrs) if !addrs.is_empty() => addrs,
            _ => {
                net.record(&host, 0, Some("lookup failed"));
                return Err(format!("Couldn't find {host}."));
            }
        };
        if addrs.iter().any(|ip| !is_public_ip(*ip)) {
            net.record(&host, 0, Some("refused: private or local address"));
            return Err(format!(
                "{host} points to a private or local address, so Wingman did not connect."
            ));
        }

        let hop = match net.get(&target, &addrs, remaining) {
            Ok(hop) => hop,
            Err(e) => {
                let e = scrub(&e, &target);
                net.record(&host, 0, Some(&e));
                return Err(format!("Couldn't reach {host}: {e}"));
            }
        };
        let status = hop.status;
        net.record(
            &host,
            hop.body.len(),
            (!is_redirect(status) && (!(200..300).contains(&status) || status == 204))
                .then(|| format!("HTTP {status}"))
                .as_deref(),
        );

        if is_redirect(status) {
            let location = hop
                .location
                .as_deref()
                .ok_or_else(|| format!("{host} redirected without saying where."))?;
            target = parse_target(&resolve_location(&target, location))
                .map_err(|e| format!("{host} redirected somewhere Wingman will not follow. {e}"))?;
            continue;
        }
        if !(200..300).contains(&status) || status == 204 {
            return Err(format!("{host} answered with an error (HTTP {status})."));
        }
        return page_from(&target.host, &hop);
    }
    Err(format!(
        "The link redirected more than {MAX_REDIRECTS} times, so Wingman stopped."
    ))
}

/// Whether `status` is a redirect whose `Location` is followed.
fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The text of one successful response.
fn page_from(host: &str, hop: &Hop) -> Result<Page, String> {
    let content_type = hop.content_type.as_deref().unwrap_or("");
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let is_html = matches!(media.as_str(), "text/html" | "application/xhtml+xml");
    if !is_html && media != "text/plain" {
        return Err(format!(
            "{host} sent something that is not a web page or text."
        ));
    }
    let decoded = decode_body(&hop.body, content_type);
    let text = if is_html {
        super::html_text::html_to_text(&decoded)
    } else {
        super::html_text::plain_to_text(&decoded)
    };
    if text.is_empty() {
        return Err(format!(
            "{host} has no readable text. It may need JavaScript to show its content."
        ));
    }
    let (text, cut) = super::selection::truncate_bounded(&text, MAX_TEXT_CHARS);
    Ok(Page {
        host: host.to_string(),
        text,
        truncated: cut || hop.body_truncated,
    })
}

/// The body as text: Latin-1 when the page says so, UTF-8 (with bad bytes
/// replaced) otherwise.
fn decode_body(body: &[u8], content_type: &str) -> String {
    let lower = content_type.to_ascii_lowercase();
    let charset = lower
        .split("charset=")
        .nth(1)
        .map(|c| c.trim_matches(|ch: char| ch == '"' || ch == '\'' || ch.is_whitespace()))
        .map(|c| c.split(';').next().unwrap_or(c).trim());
    if matches!(
        charset,
        Some("iso-8859-1" | "latin1" | "windows-1252" | "us-ascii")
    ) {
        body.iter().map(|&b| b as char).collect()
    } else {
        String::from_utf8_lossy(body).into_owned()
    }
}

/// Where a `Location` header points, as an absolute URL string (not yet
/// checked: the caller runs it through [`parse_target`]).
fn resolve_location(base: &Target, location: &str) -> String {
    let location = location.trim();
    let has_scheme = location.split_once(':').is_some_and(|(s, _)| {
        s.starts_with(|c: char| c.is_ascii_alphabetic())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    });
    if has_scheme {
        location.to_string()
    } else if location.starts_with("//") {
        format!("https:{location}")
    } else if location.starts_with('/') {
        format!("https://{}{location}", base.host)
    } else {
        let path = base.path_and_query.split('?').next().unwrap_or("/");
        let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
        format!("https://{}{dir}{location}", base.host)
    }
}

/// An error text with the link's path and query taken out: a link can carry
/// a token, and error text ends up on a card and in diagnostics.
fn scrub(error: &str, target: &Target) -> String {
    let mut out = error.replace(&target.url, "the link");
    if target.path_and_query.len() > 1 {
        out = out.replace(&target.path_and_query, "");
    }
    crate::egress::redact_opaque_tokens(&out)
}

/// Offline and Local keep everything on this computer (the Modes table).
fn allow_network(mode: &crate::mode::Mode) -> Result<(), String> {
    use crate::mode::Mode;
    match mode {
        Mode::Offline => Err(
            "Offline mode is on, so Wingman did not fetch the link. Turn Offline mode off to read web pages."
                .into(),
        ),
        Mode::Local => Err(
            "Local mode keeps everything on this computer, so Wingman did not fetch the link. Switch to Auto or Cloud to read web pages."
                .into(),
        ),
        Mode::Auto | Mode::Cloud => Ok(()),
    }
}

/// Why a fetch cannot happen in the current mode, if it cannot. Asked before
/// a link is offered for confirmation, so the user is not asked to confirm
/// something that will be refused.
pub fn network_refusal() -> Option<String> {
    allow_network(&crate::mode::current()).err()
}

/// The real network: the system resolver (on a thread, so it can time out)
/// and `ureq` connected only to the addresses the policy approved.
pub struct RealNet;

/// Hands `ureq` the already-checked addresses instead of a second lookup.
#[derive(Debug)]
struct PinnedResolver(Vec<SocketAddr>);

impl ureq::unversioned::resolver::Resolver for PinnedResolver {
    fn resolve(
        &self,
        _uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        if self.0.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        let mut out = self.empty();
        for addr in &self.0 {
            out.push(*addr);
        }
        Ok(out)
    }
}

impl Net for RealNet {
    fn resolve(&self, host: &str, timeout: Duration) -> Result<Vec<IpAddr>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let host = host.to_string();
        std::thread::spawn(move || {
            let found = (host.as_str(), 443)
                .to_socket_addrs()
                .map(|it| it.map(|a| a.ip()).collect::<Vec<_>>())
                .map_err(|e| e.to_string());
            let _ = tx.send(found);
        });
        rx.recv_timeout(timeout)
            .map_err(|_| "lookup timed out".to_string())?
    }

    fn get(&self, target: &Target, addrs: &[IpAddr], timeout: Duration) -> Result<Hop, String> {
        let pinned = PinnedResolver(addrs.iter().map(|ip| SocketAddr::new(*ip, 443)).collect());
        let config = ureq::Agent::config_builder()
            // Redirects are followed by `fetch_with`, one checked hop at a time.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .https_only(true)
            // A proxy would do its own lookup and skip the address check.
            .proxy(None)
            .timeout_global(Some(timeout))
            .user_agent(concat!(
                "Wingman/",
                env!("CARGO_PKG_VERSION"),
                " (link summary)"
            ))
            .accept("text/html, application/xhtml+xml, text/plain;q=0.9")
            .build();
        let agent = ureq::Agent::with_parts(
            config,
            ureq::unversioned::transport::DefaultConnector::default(),
            pinned,
        );
        let response = agent.get(&target.url).call().map_err(|e| e.to_string())?;
        let (parts, mut body) = response.into_parts();
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let mut bytes = Vec::new();
        let mut reader = body.as_reader().take(MAX_BODY_BYTES as u64 + 1);
        reader
            .read_to_end(&mut bytes)
            .map_err(|e| format!("reading the page failed: {e}"))?;
        let body_truncated = bytes.len() > MAX_BODY_BYTES;
        bytes.truncate(MAX_BODY_BYTES);
        Ok(Hop {
            status: parts.status.as_u16(),
            content_type: header("content-type"),
            location: header("location"),
            body: bytes,
            body_truncated,
        })
    }

    fn record(&self, host: &str, bytes_received: usize, error: Option<&str>) {
        let outcome = if error.is_some() {
            crate::egress::Outcome::Failure
        } else {
            crate::egress::Outcome::Success
        };
        crate::egress::record(&crate::egress::build_entry(
            &format!("web_fetch {host}"),
            "",
            &serde_json::Value::Null,
            false,
            0,
            bytes_received,
            outcome,
            error,
        ));
    }
}

/// Fetches the page behind `raw_url` over the real network.
pub fn fetch_page(raw_url: &str) -> Result<Page, String> {
    fetch_with(&RealNet, raw_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn t(raw: &str) -> Target {
        parse_target(raw).unwrap_or_else(|e| panic!("{raw} should be allowed: {e}"))
    }

    fn refused(raw: &str) -> String {
        match parse_target(raw) {
            Ok(t) => panic!("{raw} should be refused, got {t:?}"),
            Err(e) => e,
        }
    }

    // -- find_links ----------------------------------------------------------

    #[test]
    fn find_links_takes_each_web_address_once_in_order() {
        let text = "see https://a.example/x and HTTP://B.example, then https://a.example/x again";
        assert_eq!(
            find_links(text),
            vec!["https://a.example/x", "HTTP://B.example"]
        );
    }

    #[test]
    fn find_links_drops_trailing_punctuation_but_keeps_balanced_brackets() {
        assert_eq!(
            find_links("(https://a.example/p)."),
            vec!["https://a.example/p"]
        );
        assert_eq!(
            find_links("https://en.wikipedia.org/wiki/Rust_(language)!"),
            vec!["https://en.wikipedia.org/wiki/Rust_(language)"]
        );
        assert_eq!(
            find_links("\"https://a.example/q?x=1\","),
            vec!["https://a.example/q?x=1"]
        );
        assert_eq!(
            find_links("<https://a.example/z>"),
            vec!["https://a.example/z"]
        );
    }

    #[test]
    fn find_links_ignores_other_schemes_and_glued_prefixes() {
        assert!(
            find_links("ftp://a.example file:///c:/x javascript:alert(1) a.example").is_empty()
        );
        assert!(find_links("xhttps://a.example").is_empty());
        assert!(find_links("").is_empty());
        assert_eq!(find_links("https://"), Vec::<String>::new());
    }

    #[test]
    fn find_links_splits_on_any_whitespace() {
        assert_eq!(
            find_links("https://a.example/1\nhttps://b.example/2\thttps://c.example/3"),
            vec![
                "https://a.example/1",
                "https://b.example/2",
                "https://c.example/3"
            ]
        );
    }

    // -- parse_target: what is allowed ---------------------------------------

    #[test]
    fn a_plain_https_link_is_allowed_and_normalised() {
        let x = t("HTTPS://Example.COM/a/b?q=1&r=2#frag");
        assert_eq!(x.host, "example.com");
        assert_eq!(x.url, "https://example.com/a/b?q=1&r=2");
        assert_eq!(x.path_and_query, "/a/b?q=1&r=2");
        assert_eq!(t("https://example.com").url, "https://example.com/");
        assert_eq!(t("https://example.com?x=1").url, "https://example.com/?x=1");
        assert_eq!(t("https://example.com:443/x").url, "https://example.com/x");
        assert_eq!(t("https://example.com./x").host, "example.com");
    }

    #[test]
    fn a_non_english_host_is_fetched_by_its_ascii_name() {
        let x = t("https://b\u{FC}cher.example/");
        assert_eq!(x.host, "xn--bcher-kva.example");
        assert_eq!(x.url, "https://xn--bcher-kva.example/");
    }

    #[test]
    fn non_ascii_in_the_path_is_percent_encoded() {
        assert_eq!(
            t("https://example.com/caf\u{E9}?q=\u{E9}").url,
            "https://example.com/caf%C3%A9?q=%C3%A9"
        );
    }

    #[test]
    fn the_host_agrees_with_the_read_qr_host_reader() {
        // Two parsers must not disagree about where a link goes.
        for raw in [
            "https://example.com/a",
            "https://Sub.Example.org:443/b?c",
            "https://b\u{FC}cher.example/x",
        ] {
            assert_eq!(
                Some(t(raw).host),
                crate::actions::read_qr::url_host(raw),
                "{raw}"
            );
        }
    }

    // -- parse_target: what is refused ---------------------------------------

    #[test]
    fn http_is_refused_and_the_sentence_says_https() {
        let e = refused("http://example.com/");
        assert!(e.contains("https"), "{e}");
    }

    #[test]
    fn other_schemes_and_non_links_are_refused() {
        for raw in [
            "ftp://example.com/",
            "file:///c:/windows/win.ini",
            "javascript:alert(1)",
            "example.com",
            "//example.com/",
            "",
            "https://",
            "https:///path",
        ] {
            refused(raw);
        }
    }

    #[test]
    fn a_name_before_the_site_is_refused() {
        refused("https://bank.com@evil.example/");
        refused("https://user:pass@example.com/");
        refused("https://example.com:443@evil.example/");
    }

    #[test]
    fn only_the_default_port_is_allowed() {
        for raw in [
            "https://example.com:8443/",
            "https://example.com:80/",
            "https://example.com:22/",
            "https://example.com:/",
            "https://example.com:99999/",
            "https://example.com:abc/",
        ] {
            refused(raw);
        }
    }

    #[test]
    fn ip_addresses_are_refused_in_every_spelling() {
        for raw in [
            "https://127.0.0.1/",
            "https://10.0.0.1/",
            "https://192.168.1.1/",
            "https://169.254.169.254/latest/meta-data/",
            "https://8.8.8.8/",
            "https://[::1]/",
            "https://[fe80::1]/",
            "https://[2606:4700::1111]/",
            "https://[::ffff:127.0.0.1]/",
            "https://2130706433/",
            "https://0x7f000001/",
            "https://0x7f.0.0.1/",
            "https://017700000001/",
            "https://127.1/",
            "https://0/",
            "https://1.1.1.1.1/",
            "https://example.123/",
        ] {
            refused(raw);
        }
    }

    #[test]
    fn local_and_internal_names_are_refused() {
        for raw in [
            "https://localhost/",
            "https://LOCALHOST./",
            "https://app.localhost/",
            "https://printer.local/",
            "https://wiki.internal/",
            "https://router.lan/",
            "https://nas.home.arpa/",
            "https://intranet/",
            "https://corp/",
            "https://machine.corp/",
        ] {
            refused(raw);
        }
    }

    #[test]
    fn odd_characters_in_the_host_are_refused() {
        for raw in [
            "https://exa mple.com/",
            "https://exa_mple.com/",
            "https://%65xample.com/",
            "https://-example.com/",
            "https://example-.com/",
            "https://exa..mple.com/",
            "https://.example.com/",
            "https://example.com\\@evil.example/",
            "https://example.com\\x",
            "https://exam\u{0}ple.com/",
            "https://example.com/a b",
            "https://example.com/\u{7}",
        ] {
            refused(raw);
        }
        refused(&format!("https://{}.com/", "a".repeat(64)));
        refused(&format!("https://{}.com/", "a.".repeat(130)));
        refused(&format!("https://example.com/{}", "a".repeat(5000)));
    }

    // -- is_public_ip --------------------------------------------------------

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    #[test]
    fn private_loopback_link_local_and_reserved_v4_are_not_public() {
        for ip in [
            v4(0, 0, 0, 0),
            v4(0, 1, 2, 3),
            v4(10, 0, 0, 1),
            v4(10, 255, 255, 255),
            v4(100, 64, 0, 1),
            v4(100, 127, 255, 255),
            v4(127, 0, 0, 1),
            v4(127, 255, 255, 254),
            v4(169, 254, 169, 254),
            v4(172, 16, 0, 1),
            v4(172, 31, 255, 255),
            v4(192, 0, 0, 1),
            v4(192, 0, 2, 1),
            v4(192, 168, 0, 1),
            v4(198, 18, 0, 1),
            v4(198, 19, 255, 255),
            v4(198, 51, 100, 1),
            v4(203, 0, 113, 1),
            v4(224, 0, 0, 1),
            v4(239, 255, 255, 255),
            v4(240, 0, 0, 1),
            v4(255, 255, 255, 255),
        ] {
            assert!(!is_public_ip(ip), "{ip} must not be public");
        }
    }

    #[test]
    fn ordinary_v4_addresses_next_to_the_reserved_ranges_are_public() {
        for ip in [
            v4(8, 8, 8, 8),
            v4(1, 1, 1, 1),
            v4(93, 184, 216, 34),
            v4(100, 63, 255, 255),
            v4(100, 128, 0, 1),
            v4(172, 15, 255, 255),
            v4(172, 32, 0, 1),
            v4(192, 167, 255, 255),
            v4(192, 169, 0, 1),
            v4(198, 17, 255, 255),
            v4(198, 20, 0, 1),
            v4(223, 255, 255, 255),
        ] {
            assert!(is_public_ip(ip), "{ip} should be public");
        }
    }

    #[test]
    fn private_loopback_link_local_and_embedded_v6_are_not_public() {
        for s in [
            "::",
            "::1",
            "fe80::1",
            "febf::1",
            "fc00::1",
            "fd12:3456::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "2002:7f00:1::1",
            "2002:c0a8:101::1",
            "64:ff9b::7f00:1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:192.168.1.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "::a00:1",
            "100::1",
            "3fff::1",
        ] {
            assert!(!is_public_ip(v6(s)), "{s} must not be public");
        }
    }

    #[test]
    fn global_unicast_v6_is_public() {
        for s in [
            "2606:4700::1111",
            "2a00:1450:4001:81b::200e",
            "2400:cb00::1",
        ] {
            assert!(is_public_ip(v6(s)), "{s} should be public");
        }
        // A mapped public address is as public as the address inside it.
        assert!(is_public_ip(v6("::ffff:8.8.8.8")));
    }

    // -- fetch_with, over a fake network -------------------------------------

    /// Scripted network: DNS answers per host and one response per URL, plus
    /// a record of what the policy let through.
    struct FakeNet {
        dns: HashMap<String, Vec<IpAddr>>,
        pages: HashMap<String, Hop>,
        resolved: RefCell<Vec<String>>,
        gets: RefCell<Vec<(String, Vec<IpAddr>)>>,
        logged: RefCell<Vec<(String, usize, Option<String>)>>,
    }

    impl FakeNet {
        fn new() -> Self {
            Self {
                dns: HashMap::new(),
                pages: HashMap::new(),
                resolved: RefCell::new(Vec::new()),
                gets: RefCell::new(Vec::new()),
                logged: RefCell::new(Vec::new()),
            }
        }
        fn host(mut self, name: &str, ip: IpAddr) -> Self {
            self.dns.entry(name.to_string()).or_default().push(ip);
            self
        }
        fn page(mut self, url: &str, hop: Hop) -> Self {
            self.pages.insert(url.to_string(), hop);
            self
        }
    }

    impl Net for FakeNet {
        fn resolve(&self, host: &str, _timeout: Duration) -> Result<Vec<IpAddr>, String> {
            self.resolved.borrow_mut().push(host.to_string());
            self.dns
                .get(host)
                .cloned()
                .ok_or_else(|| format!("no such host {host}"))
        }
        fn get(&self, target: &Target, addrs: &[IpAddr], _t: Duration) -> Result<Hop, String> {
            self.gets
                .borrow_mut()
                .push((target.url.clone(), addrs.to_vec()));
            self.pages
                .get(&target.url)
                .cloned()
                .ok_or_else(|| "connection refused".to_string())
        }
        fn record(&self, host: &str, bytes: usize, error: Option<&str>) {
            self.logged
                .borrow_mut()
                .push((host.to_string(), bytes, error.map(String::from)));
        }
    }

    fn html(body: &str) -> Hop {
        Hop {
            status: 200,
            content_type: Some("text/html; charset=utf-8".into()),
            body: body.as_bytes().to_vec(),
            ..Hop::default()
        }
    }

    fn redirect(status: u16, to: &str) -> Hop {
        Hop {
            status,
            location: Some(to.to_string()),
            ..Hop::default()
        }
    }

    const PUBLIC: IpAddr = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));

    /// Runs `f` with the process-wide mode set to Auto (the mode is shared
    /// by every test in the binary, so hold its lock).
    fn in_mode<R>(mode: crate::mode::Mode, f: impl FnOnce() -> R) -> R {
        let _g = crate::mode::MODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::mode::set_current(mode);
        let r = f();
        crate::mode::set_current(crate::mode::Mode::Auto);
        r
    }

    fn fetch(net: &FakeNet, url: &str) -> Result<Page, String> {
        in_mode(crate::mode::Mode::Auto, || fetch_with(net, url))
    }

    #[test]
    fn a_public_page_is_fetched_stripped_and_logged() {
        let net = FakeNet::new().host("example.com", PUBLIC).page(
            "https://example.com/a",
            html("<title>T</title><p>Hello &amp; welcome</p><script>x()</script>"),
        );
        let page = fetch(&net, "https://example.com/a#frag").unwrap();
        assert_eq!(page.host, "example.com");
        assert_eq!(page.text, "T\nHello & welcome");
        assert!(!page.truncated);
        // The connection is pinned to the address that was checked.
        assert_eq!(
            net.gets.borrow().as_slice(),
            &[("https://example.com/a".to_string(), vec![PUBLIC])]
        );
        // One egress row, host only.
        let logged = net.logged.borrow();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].0, "example.com");
        assert!(logged[0].1 > 0);
        assert_eq!(logged[0].2, None);
    }

    #[test]
    fn plain_text_is_accepted() {
        let net = FakeNet::new().host("example.com", PUBLIC).page(
            "https://example.com/",
            Hop {
                status: 200,
                content_type: Some("text/plain".into()),
                body: b"line one\r\nline two".to_vec(),
                ..Hop::default()
            },
        );
        assert_eq!(
            fetch(&net, "https://example.com").unwrap().text,
            "line one\nline two"
        );
    }

    #[test]
    fn offline_and_local_mode_never_touch_the_network() {
        for mode in [crate::mode::Mode::Offline, crate::mode::Mode::Local] {
            let net = FakeNet::new()
                .host("example.com", PUBLIC)
                .page("https://example.com/", html("<p>x</p>"));
            let err = in_mode(mode, || fetch_with(&net, "https://example.com/")).unwrap_err();
            assert!(
                err.contains("Offline") || err.contains("Local"),
                "{mode:?}: {err}"
            );
            assert!(net.resolved.borrow().is_empty(), "{mode:?} resolved a name");
            assert!(net.gets.borrow().is_empty(), "{mode:?} made a request");
            assert!(net.logged.borrow().is_empty(), "{mode:?} logged a hop");
        }
    }

    #[test]
    fn a_refused_link_is_never_looked_up() {
        let net = FakeNet::new();
        for raw in [
            "http://example.com/",
            "https://127.0.0.1/",
            "https://localhost/",
            "https://user@example.com/",
        ] {
            fetch(&net, raw).unwrap_err();
        }
        assert!(net.resolved.borrow().is_empty());
        assert!(net.gets.borrow().is_empty());
    }

    #[test]
    fn a_name_that_resolves_to_a_private_address_is_not_connected_to() {
        for ip in [
            v4(127, 0, 0, 1),
            v4(10, 1, 2, 3),
            v4(192, 168, 1, 10),
            v4(169, 254, 169, 254),
            v6("::1"),
            v6("fc00::5"),
            v6("fe80::1"),
        ] {
            let net = FakeNet::new()
                .host("sneaky.example", ip)
                .page("https://sneaky.example/", html("<p>secret</p>"));
            let err = fetch(&net, "https://sneaky.example/").unwrap_err();
            assert!(err.contains("private or local"), "{ip}: {err}");
            assert!(net.gets.borrow().is_empty(), "{ip} was requested");
        }
    }

    #[test]
    fn one_private_address_among_public_ones_refuses_the_whole_name() {
        let net = FakeNet::new()
            .host("mixed.example", PUBLIC)
            .host("mixed.example", v4(10, 0, 0, 1))
            .page("https://mixed.example/", html("<p>x</p>"));
        fetch(&net, "https://mixed.example/").unwrap_err();
        assert!(net.gets.borrow().is_empty());
    }

    #[test]
    fn a_name_with_no_address_is_an_error_not_a_panic() {
        let net = FakeNet::new();
        let err = fetch(&net, "https://nowhere.example/").unwrap_err();
        assert!(err.contains("find"), "{err}");
    }

    #[test]
    fn a_redirect_to_another_public_host_is_followed_and_each_hop_is_logged() {
        let net = FakeNet::new()
            .host("short.example", PUBLIC)
            .host("long.example", v4(8, 8, 8, 8))
            .page(
                "https://short.example/x",
                redirect(301, "https://long.example/final"),
            )
            .page("https://long.example/final", html("<p>landed</p>"));
        let page = fetch(&net, "https://short.example/x").unwrap();
        assert_eq!(page.text, "landed");
        assert_eq!(page.host, "long.example");
        assert_eq!(net.logged.borrow().len(), 2);
        assert_eq!(net.logged.borrow()[0].0, "short.example");
        assert_eq!(net.logged.borrow()[1].0, "long.example");
    }

    #[test]
    fn relative_redirects_stay_on_the_same_host_and_are_rechecked() {
        let net = FakeNet::new()
            .host("example.com", PUBLIC)
            .page("https://example.com/a/b", redirect(302, "c?x=1"))
            .page("https://example.com/a/c?x=1", redirect(307, "/top"))
            .page(
                "https://example.com/top",
                redirect(308, "//example.com/end"),
            )
            .page("https://example.com/end", html("<p>end</p>"));
        assert_eq!(fetch(&net, "https://example.com/a/b").unwrap().text, "end");
        assert_eq!(
            net.resolved.borrow().len(),
            4,
            "every hop is looked up again"
        );
    }

    #[test]
    fn a_redirect_into_a_private_address_is_refused_after_the_first_request() {
        let net = FakeNet::new()
            .host("open.example", PUBLIC)
            .host("inner.example", v4(10, 0, 0, 7))
            .page(
                "https://open.example/",
                redirect(302, "https://inner.example/admin"),
            )
            .page("https://inner.example/admin", html("<p>internal</p>"));
        let err = fetch(&net, "https://open.example/").unwrap_err();
        assert!(err.contains("private or local"), "{err}");
        assert_eq!(
            net.gets.borrow().len(),
            1,
            "only the public hop was requested"
        );
    }

    #[test]
    fn a_redirect_to_a_literal_loopback_or_localhost_is_refused_before_any_lookup() {
        for to in [
            "https://127.0.0.1/",
            "https://localhost/x",
            "https://[::1]/",
            "http://example.com/",
            "ftp://example.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            let net = FakeNet::new()
                .host("open.example", PUBLIC)
                .page("https://open.example/", redirect(302, to));
            fetch(&net, "https://open.example/").unwrap_err();
            assert_eq!(net.resolved.borrow().as_slice(), ["open.example"], "{to}");
            assert_eq!(net.gets.borrow().len(), 1, "{to}");
        }
    }

    #[test]
    fn a_redirect_without_a_location_is_an_error() {
        let net = FakeNet::new().host("example.com", PUBLIC).page(
            "https://example.com/",
            Hop {
                status: 302,
                ..Hop::default()
            },
        );
        fetch(&net, "https://example.com/").unwrap_err();
    }

    #[test]
    fn too_many_redirects_stop_the_fetch() {
        let mut net = FakeNet::new().host("loop.example", PUBLIC);
        for n in 0..10 {
            net = net.page(
                &format!("https://loop.example/{n}"),
                redirect(302, &format!("/{}", n + 1)),
            );
        }
        let err = fetch(&net, "https://loop.example/0").unwrap_err();
        assert!(err.contains("redirect"), "{err}");
        assert_eq!(net.gets.borrow().len(), MAX_REDIRECTS + 1);
    }

    #[test]
    fn only_web_pages_and_text_are_read() {
        for ct in [
            Some("application/json"),
            Some("application/pdf"),
            Some("image/png"),
            Some("application/octet-stream"),
            None,
        ] {
            let net = FakeNet::new().host("example.com", PUBLIC).page(
                "https://example.com/",
                Hop {
                    status: 200,
                    content_type: ct.map(String::from),
                    body: b"{}".to_vec(),
                    ..Hop::default()
                },
            );
            let err = fetch(&net, "https://example.com/").unwrap_err();
            assert!(err.contains("not a web page or text"), "{ct:?}: {err}");
        }
        for ct in [
            "text/html",
            "TEXT/HTML; charset=UTF-8",
            "application/xhtml+xml",
            "text/plain; charset=utf-8",
        ] {
            let net = FakeNet::new().host("example.com", PUBLIC).page(
                "https://example.com/",
                Hop {
                    status: 200,
                    content_type: Some(ct.into()),
                    body: b"<p>ok</p>".to_vec(),
                    ..Hop::default()
                },
            );
            fetch(&net, "https://example.com/").unwrap_or_else(|e| panic!("{ct}: {e}"));
        }
    }

    #[test]
    fn an_error_status_is_an_error_naming_the_status() {
        for status in [403u16, 404, 500, 204] {
            let net = FakeNet::new().host("example.com", PUBLIC).page(
                "https://example.com/",
                Hop {
                    status,
                    content_type: Some("text/html".into()),
                    body: b"<p>nope</p>".to_vec(),
                    ..Hop::default()
                },
            );
            let err = fetch(&net, "https://example.com/").unwrap_err();
            assert!(err.contains(&status.to_string()), "{status}: {err}");
            // A failed hop is still a logged hop.
            assert_eq!(net.logged.borrow().len(), 1);
            assert!(net.logged.borrow()[0].2.is_some());
        }
    }

    #[test]
    fn a_page_with_no_readable_text_says_so() {
        let net = FakeNet::new().host("example.com", PUBLIC).page(
            "https://example.com/",
            html("<script>app()</script><div></div>"),
        );
        let err = fetch(&net, "https://example.com/").unwrap_err();
        assert!(err.contains("no readable text"), "{err}");
    }

    #[test]
    fn a_connection_failure_is_an_error_naming_the_host_and_is_logged() {
        let net = FakeNet::new().host("example.com", PUBLIC);
        let err = fetch(&net, "https://example.com/").unwrap_err();
        assert!(err.contains("example.com"), "{err}");
        assert_eq!(net.logged.borrow().len(), 1);
        assert!(net.logged.borrow()[0].2.is_some());
    }

    #[test]
    fn long_text_and_a_stopped_download_are_marked_truncated() {
        let long = format!("<p>{}</p>", "word ".repeat(MAX_TEXT_CHARS));
        let net = FakeNet::new()
            .host("example.com", PUBLIC)
            .page("https://example.com/", html(&long));
        let page = fetch(&net, "https://example.com/").unwrap();
        assert!(page.truncated);
        assert_eq!(page.text.chars().count(), MAX_TEXT_CHARS);

        let net = FakeNet::new().host("example.com", PUBLIC).page(
            "https://example.com/",
            Hop {
                body_truncated: true,
                ..html("<p>short</p>")
            },
        );
        assert!(fetch(&net, "https://example.com/").unwrap().truncated);
    }

    #[test]
    fn latin1_pages_are_decoded_by_their_declared_charset() {
        let net = FakeNet::new().host("example.com", PUBLIC).page(
            "https://example.com/",
            Hop {
                status: 200,
                content_type: Some("text/html; charset=ISO-8859-1".into()),
                body: b"<p>caf\xE9</p>".to_vec(),
                ..Hop::default()
            },
        );
        assert_eq!(
            fetch(&net, "https://example.com/").unwrap().text,
            "caf\u{E9}"
        );
    }

    #[test]
    fn invalid_utf8_does_not_fail_the_fetch() {
        let net = FakeNet::new().host("example.com", PUBLIC).page(
            "https://example.com/",
            Hop {
                status: 200,
                content_type: Some("text/html".into()),
                body: b"<p>a\xFF\xFEb</p>".to_vec(),
                ..Hop::default()
            },
        );
        let text = fetch(&net, "https://example.com/").unwrap().text;
        assert!(text.starts_with('a') && text.ends_with('b'), "{text}");
    }

    /// The HTTP library is called from exactly two files, each behind the
    /// Offline guard. A new caller elsewhere must go through one of them, or
    /// be added here on purpose.
    #[test]
    fn only_the_known_files_call_ureq() {
        fn walk(dir: &std::path::Path, hits: &mut Vec<String>, seen: &mut usize) {
            for entry in std::fs::read_dir(dir).expect("read src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    walk(&path, hits, seen);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    *seen += 1;
                    let text = std::fs::read_to_string(&path).expect("read source file");
                    // Built from pieces so this file does not match itself.
                    if text.replace('\r', "").contains(concat!("ure", "q::")) {
                        hits.push(path.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let (mut hits, mut seen) = (Vec::new(), 0);
        walk(&src, &mut hits, &mut seen);
        assert!(seen > 50, "the scan found only {seen} source files");
        hits.sort();
        let names: Vec<&str> = hits
            .iter()
            .map(|h| h.rsplit("/src/").next().unwrap_or(h))
            .collect();
        assert_eq!(
            names,
            ["inputs/web_page.rs", "provider/common.rs"],
            "who calls the HTTP library changed: every caller must sit behind the Offline guard"
        );
    }

    /// Live check of the real transport (the pinned resolver, TLS, the body
    /// cap), against a public page. `#[ignore]`d because it needs the
    /// internet: `cargo test web_page_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn web_page_live_fetches_a_public_page_through_the_real_transport() {
        let _g = crate::mode::MODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::mode::set_current(crate::mode::Mode::Auto);
        let page = fetch_page("https://example.com/").expect("example.com should be fetchable");
        println!(
            "host={} truncated={} text={:?}",
            page.host, page.truncated, page.text
        );
        assert_eq!(page.host, "example.com");
        assert!(page.text.contains("Example Domain"), "{}", page.text);
        assert!(!page.text.contains('<'), "markup leaked: {}", page.text);
        // google.com answers with a 301 to www.google.com: the redirect is
        // followed one checked hop at a time.
        let redirected = fetch_page("https://google.com/").expect("google.com should redirect");
        assert_eq!(redirected.host, "www.google.com");
        let err = fetch_page("https://localhost/").unwrap_err();
        assert!(err.contains("public website name"), "{err}");
        let err = fetch_page("http://example.com/").unwrap_err();
        assert!(err.contains("https"), "{err}");
    }

    #[test]
    fn a_failure_text_never_contains_the_path_or_query_of_the_link() {
        // The link may carry a token; error text can end up on a card and in
        // diagnostics, so it names the host only.
        let net = FakeNet::new().host("example.com", PUBLIC);
        let err = fetch(&net, "https://example.com/reset?token=SECRET123").unwrap_err();
        assert!(!err.contains("SECRET123"), "{err}");
        for e in net.logged.borrow().iter() {
            assert!(!format!("{e:?}").contains("SECRET123"));
        }
    }
}
