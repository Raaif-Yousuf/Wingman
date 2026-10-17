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
//! Every hop is one row in the local egress log (the store's `egress` table): the host and
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
    /// The Content-Encoding header. Wingman asks for none, so anything but
    /// identity is a server compressing unprompted, and the bytes are unreadable.
    pub content_encoding: Option<String>,
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

        // The lookup used some of the time: the request gets what is left.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            net.record(&host, 0, Some("timed out"));
            return Err(format!("{host} took too long to answer."));
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
    // Wingman does not ask for compressed bodies, so a server that sends one
    // anyway would be read as garbage text. Refuse it instead.
    let encoding = hop.content_encoding.as_deref().unwrap_or("").trim();
    if !encoding.is_empty() && !encoding.eq_ignore_ascii_case("identity") {
        return Err(format!("{host} sent compressed data Wingman can't read."));
    }
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

/// The body as text, in the character set the page declares (#552): the
/// `Content-Type` header first, then a `<meta>` tag near the top. A set this
/// code does not know, or no declaration, reads as UTF-8 with bad bytes
/// replaced. Never fails and never panics on malformed bytes.
fn decode_body(body: &[u8], content_type: &str) -> String {
    let label = charset_from_header(content_type).or_else(|| charset_from_meta(body));
    if let Some(code_page) = label.as_deref().and_then(code_page_for) {
        if let Some(text) = decode_code_page(body, code_page) {
            return text;
        }
    }
    String::from_utf8_lossy(body).into_owned()
}

/// The `charset=` value of a `Content-Type` header, lower-cased.
fn charset_from_header(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    let value = lower.split("charset=").nth(1)?;
    let value = value.split(';').next().unwrap_or(value);
    let value = value.trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace());
    (!value.is_empty()).then(|| value.to_string())
}

/// The charset a `<meta charset=...>` or `<meta http-equiv content="...;
/// charset=...">` tag names, looking only at the first 1024 bytes the way
/// browsers do. Lower-cased.
fn charset_from_meta(body: &[u8]) -> Option<String> {
    let head: String = body
        .iter()
        .take(1024)
        .map(|&b| b.to_ascii_lowercase() as char)
        .collect();
    let at = head.find("charset")? + "charset".len();
    let rest = head[at..].trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.trim_start_matches(['"', '\'']);
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')))
        .unwrap_or(rest.len());
    let label = &rest[..end];
    (!label.is_empty()).then(|| label.to_string())
}

/// The Windows code page for a lower-case charset label, or `None` for UTF-8
/// and for any label not listed (those read as UTF-8). Latin-1 and ASCII map
/// to 1252, as browsers treat them.
fn code_page_for(label: &str) -> Option<u32> {
    Some(match label {
        "iso-8859-1" | "latin1" | "latin-1" | "us-ascii" | "ascii" | "windows-1252" | "cp1252" => {
            1252
        }
        "windows-1250" => 1250,
        "windows-1251" => 1251,
        "windows-1253" => 1253,
        "windows-1254" => 1254,
        "windows-1255" => 1255,
        "windows-1256" => 1256,
        "windows-1257" => 1257,
        "windows-1258" => 1258,
        "iso-8859-2" => 28592,
        "iso-8859-3" => 28593,
        "iso-8859-4" => 28594,
        "iso-8859-5" => 28595,
        "iso-8859-6" => 28596,
        "iso-8859-7" => 28597,
        "iso-8859-8" => 28598,
        "iso-8859-9" => 28599,
        "iso-8859-15" => 28605,
        "koi8-r" => 20866,
        "koi8-u" => 21866,
        "shift_jis" | "shift-jis" | "sjis" | "x-sjis" | "windows-31j" => 932,
        "euc-jp" => 51932,
        "gbk" | "gb2312" | "x-gbk" => 936,
        "gb18030" => 54936,
        "big5" => 950,
        "euc-kr" | "ks_c_5601-1987" => 949,
        _ => return None,
    })
}

/// Decodes `body` from `code_page` with the Windows converter (no extra
/// crate). Bytes that do not fit the set become replacement characters, so
/// this does not fail on malformed input; `None` only if the API refuses.
pub(crate) fn decode_code_page(body: &[u8], code_page: u32) -> Option<String> {
    use windows::Win32::Globalization::{MultiByteToWideChar, MB_PRECOMPOSED};
    if body.is_empty() {
        return Some(String::new());
    }
    if i32::try_from(body.len()).is_err() {
        return None;
    }
    // SAFETY: both calls read `body` only and write at most `wide.len()`
    // u16s into `wide`, whose length the first call asked for.
    unsafe {
        let needed = MultiByteToWideChar(code_page, MB_PRECOMPOSED, body, None);
        if needed <= 0 {
            return None;
        }
        let mut wide = vec![0u16; needed as usize];
        let written = MultiByteToWideChar(code_page, MB_PRECOMPOSED, body, Some(&mut wide));
        if written <= 0 {
            return None;
        }
        wide.truncate(written as usize);
        Some(String::from_utf16_lossy(&wide))
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

/// The most addresses ureq's resolver result holds.
const MAX_PINNED_ADDRS: usize = 16;

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
        // ureq keeps at most 16 and its `push` does not check the capacity:
        // a 17th would panic, and a release build aborts on panic.
        for addr in self.0.iter().take(MAX_PINNED_ADDRS) {
            out.push(*addr);
        }
        Ok(out)
    }
}

impl Net for RealNet {
    fn resolve(&self, host: &str, timeout: Duration) -> Result<Vec<IpAddr>, String> {
        crate::ui_thread::blocking_call("page resolve");
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
        let pinned = addrs.iter().map(|ip| SocketAddr::new(*ip, 443)).collect();
        fetch_hop(&target.url, pinned, timeout, true)
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

/// One GET of `url` (no redirects followed), connecting only to `pinned`.
/// `https_only` is true outside the pinning test, which has no certificate
/// to offer a plain local listener.
fn fetch_hop(
    url: &str,
    pinned: Vec<SocketAddr>,
    timeout: Duration,
    https_only: bool,
) -> Result<Hop, String> {
    crate::ui_thread::blocking_call("page fetch");
    let pinned = PinnedResolver(pinned);
    let config = ureq::Agent::config_builder()
        // Redirects are followed by `fetch_with`, one checked hop at a time.
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .https_only(https_only)
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
    let response = agent.get(url).call().map_err(|e| e.to_string())?;
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
        content_encoding: header("content-encoding"),
        location: header("location"),
        body: bytes,
        body_truncated,
    })
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
        /// How long every lookup takes, and the time each request was given.
        resolve_delay: Duration,
        get_timeouts: RefCell<Vec<Duration>>,
    }

    impl FakeNet {
        fn new() -> Self {
            Self {
                dns: HashMap::new(),
                pages: HashMap::new(),
                resolved: RefCell::new(Vec::new()),
                gets: RefCell::new(Vec::new()),
                logged: RefCell::new(Vec::new()),
                resolve_delay: Duration::ZERO,
                get_timeouts: RefCell::new(Vec::new()),
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
            std::thread::sleep(self.resolve_delay);
            self.resolved.borrow_mut().push(host.to_string());
            self.dns
                .get(host)
                .cloned()
                .ok_or_else(|| format!("no such host {host}"))
        }
        fn get(&self, target: &Target, addrs: &[IpAddr], timeout: Duration) -> Result<Hop, String> {
            self.get_timeouts.borrow_mut().push(timeout);
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
    fn a_compressed_body_is_refused_with_a_clear_message() {
        for enc in ["gzip", "br", "deflate", "GZIP", "gzip, br"] {
            let net = FakeNet::new().host("example.com", PUBLIC).page(
                "https://example.com/",
                Hop {
                    status: 200,
                    content_type: Some("text/html".into()),
                    content_encoding: Some(enc.into()),
                    body: vec![0x1f, 0x8b, 0x08, 0x00],
                    ..Hop::default()
                },
            );
            let err = fetch(&net, "https://example.com/").unwrap_err();
            assert!(
                err.contains("compressed data Wingman can't read"),
                "{enc}: {err}"
            );
        }
        for enc in [None, Some("identity"), Some("")] {
            let net = FakeNet::new().host("example.com", PUBLIC).page(
                "https://example.com/",
                Hop {
                    status: 200,
                    content_type: Some("text/html".into()),
                    content_encoding: enc.map(String::from),
                    body: b"<p>ok</p>".to_vec(),
                    ..Hop::default()
                },
            );
            fetch(&net, "https://example.com/").unwrap_or_else(|e| panic!("{enc:?}: {e}"));
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

    fn page_with(content_type: Option<&str>, body: &[u8]) -> String {
        let net = FakeNet::new().host("example.com", PUBLIC).page(
            "https://example.com/",
            Hop {
                status: 200,
                content_type: content_type.map(str::to_string),
                body: body.to_vec(),
                ..Hop::default()
            },
        );
        fetch(&net, "https://example.com/").unwrap().text
    }

    #[test]
    fn other_declared_charsets_are_decoded() {
        // Russian "privet" in windows-1251 and in KOI8-R.
        assert_eq!(
            page_with(
                Some("text/html; charset=windows-1251"),
                b"<p>\xEF\xF0\xE8\xE2\xE5\xF2</p>"
            ),
            "\u{43F}\u{440}\u{438}\u{432}\u{435}\u{442}"
        );
        assert_eq!(
            page_with(
                Some("text/html; charset=koi8-r"),
                b"<p>\xD0\xD2\xC9\xD7\xC5\xD4</p>"
            ),
            "\u{43F}\u{440}\u{438}\u{432}\u{435}\u{442}"
        );
        // Japanese "nihon" in Shift_JIS.
        assert_eq!(
            page_with(
                Some("text/html; charset=\"Shift_JIS\""),
                b"<p>\x93\xFA\x96\x7B</p>"
            ),
            "\u{65E5}\u{672C}"
        );
    }

    #[test]
    fn a_meta_tag_names_the_charset_when_the_header_does_not() {
        assert_eq!(
            page_with(
                Some("text/html"),
                b"<meta charset=\"windows-1251\"><p>\xEF\xF0\xE8</p>"
            ),
            "\u{43F}\u{440}\u{438}"
        );
        assert_eq!(
            page_with(
                Some("text/html"),
                b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-2\"><p>\xB1</p>"
            ),
            "\u{105}"
        );
    }

    #[test]
    fn the_header_wins_over_a_meta_tag() {
        assert_eq!(
            page_with(
                Some("text/html; charset=iso-8859-1"),
                b"<meta charset=\"windows-1251\"><p>caf\xE9</p>"
            ),
            "caf\u{E9}"
        );
    }

    #[test]
    fn an_unknown_charset_and_malformed_bytes_never_fail() {
        let text = page_with(Some("text/html; charset=x-made-up"), b"<p>a\xFF\xFEb</p>");
        assert!(text.starts_with('a') && text.ends_with('b'), "{text}");
        // Truncated multi-byte sequence in a multi-byte set.
        let text = page_with(Some("text/html; charset=shift_jis"), b"<p>ok\x93</p>");
        assert!(text.starts_with("ok"), "{text}");
        assert_eq!(code_page_for("utf-8"), None);
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
    fn the_request_gets_the_time_left_after_the_lookup() {
        let mut net = FakeNet::new()
            .host("example.com", PUBLIC)
            .page("https://example.com/", html("<p>x</p>"));
        net.resolve_delay = Duration::from_millis(300);
        fetch(&net, "https://example.com/").unwrap();
        let given = net.get_timeouts.borrow()[0];
        assert!(
            given <= TOTAL_TIMEOUT - Duration::from_millis(250),
            "the request was given {given:?}, as if the lookup took no time"
        );
    }

    // -- the real transport's pinned resolver ----------------------------------

    fn resolve_pinned(addrs: Vec<SocketAddr>) -> Result<usize, ureq::Error> {
        use ureq::unversioned::resolver::Resolver;
        let uri: ureq::http::Uri = "https://example.com/".parse().unwrap();
        let timeout = ureq::unversioned::transport::NextTimeout {
            after: ureq::unversioned::transport::time::Duration::from_secs(5),
            reason: ureq::Timeout::Global,
        };
        PinnedResolver(addrs)
            .resolve(&uri, &ureq::config::Config::default(), timeout)
            .map(|found| found.len())
    }

    #[test]
    fn a_dns_answer_with_many_addresses_does_not_overflow_the_resolver() {
        // ureq keeps 16 and its `push` does not check: 17 would panic, and a
        // release build aborts on panic.
        let many: Vec<SocketAddr> = (1..=40u8)
            .map(|n| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, n)), 443))
            .collect();
        assert_eq!(resolve_pinned(many[..17].to_vec()).unwrap(), 16);
        assert_eq!(resolve_pinned(many).unwrap(), 16);
        assert_eq!(
            resolve_pinned(vec![SocketAddr::new(PUBLIC, 443)]).unwrap(),
            1
        );
        assert!(resolve_pinned(Vec::new()).is_err());
    }

    #[test]
    fn the_real_transport_connects_to_the_pinned_address_not_to_a_lookup() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut seen = [0u8; 512];
            let _ = std::io::Read::read(&mut conn, &mut seen);
            let body = "pinned ok";
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK
Content-Type: text/plain
Content-Length: {}
Connection: close

{body}",
                body.len()
            );
        });
        // `.invalid` can never resolve, so only the pin can reach the listener.
        let hop = fetch_hop(
            &format!("http://pin-test.invalid:{}/", addr.port()),
            vec![addr],
            Duration::from_secs(5),
            false,
        )
        .expect("the request should reach the pinned listener");
        server.join().unwrap();
        assert_eq!(hop.status, 200);
        assert_eq!(hop.body, b"pinned ok");
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
