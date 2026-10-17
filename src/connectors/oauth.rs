//! OAuth 2.0 authorization code flow with PKCE over a loopback redirect
//! (issue #53), shared by the `google` and `microsoft` connectors (expansion
//! plan §10, §12). Nothing calls it yet: those connectors are later work, so
//! the module carries the same forward-wiring `allow(dead_code)` the rest of
//! `connectors` has.
//!
//! The flow, end to end ([`authorize`]):
//!
//! 1. Bind a listener on `127.0.0.1:<random port>`.
//! 2. Open the provider's consent page in the default browser, with a PKCE
//!    S256 challenge and a random `state`.
//! 3. Wait (bounded) for the browser to be redirected to the listener, check
//!    `state`, take the `code`.
//! 4. Exchange the code plus the PKCE verifier at the token endpoint.
//! 5. Keep the refresh token in Windows Credential Manager (through
//!    [`SecretStore`]); the access token only ever lives in memory.
//!
//! [`access_token`] silently refreshes, and [`revoke`] revokes and forgets.
//!
//! No token, code or verifier is ever written to config or to a log, and no
//! error string here carries one: an endpoint failure reports only the
//! provider's `error` and `error_description` fields.
//!
//! Everything that touches the world is injected so tests never open a
//! browser or reach a real provider: [`BrowserOpener`], [`TokenTransport`],
//! [`SecretStore`]. The real transport honours Offline mode and refuses
//! plain `http` to anything that is not loopback.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;
use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};

use crate::secrets::SecretStore;

/// How long [`authorize`] waits for the user to finish in the browser.
pub const DEFAULT_AUTH_TIMEOUT: Duration = Duration::from_secs(180);

/// Everything that differs between providers. The flow itself never
/// branches on which provider this is.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct OAuthProvider {
    /// Short id, used for the credential name (`Wingman/oauth-<id>`).
    pub id: &'static str,
    pub auth_url: String,
    pub token_url: String,
    /// `None` when the provider has no revocation endpoint (Microsoft's
    /// identity platform has none); revoking then only forgets the token
    /// locally.
    pub revoke_url: Option<String>,
    /// Host written into the redirect URI. The listener always binds
    /// `127.0.0.1`.
    pub redirect_host: &'static str,
    /// Extra query parameters on the consent URL (Google needs
    /// `access_type=offline` to hand out a refresh token).
    pub extra_auth_params: Vec<(&'static str, &'static str)>,
}

#[allow(dead_code)]
impl OAuthProvider {
    pub fn google() -> Self {
        Self {
            id: "google",
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            revoke_url: Some("https://oauth2.googleapis.com/revoke".into()),
            redirect_host: "127.0.0.1",
            extra_auth_params: vec![("access_type", "offline"), ("prompt", "consent")],
        }
    }

    /// The common endpoint, so personal and work accounts both work.
    /// THEORY (unverified): Microsoft only accepts `localhost` for a loopback
    /// redirect, not the `127.0.0.1` literal, so this one writes `localhost`.
    pub fn microsoft() -> Self {
        Self {
            id: "microsoft",
            auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".into(),
            token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
            revoke_url: None,
            redirect_host: "localhost",
            extra_auth_params: Vec::new(),
        }
    }

    /// The Credential Manager target the refresh token is kept under.
    pub fn credential_target(&self) -> String {
        crate::secrets::target_name(&format!("oauth-{}", self.id))
    }
}

/// The app registration: a public client id (not a secret, PKCE) and an
/// optional client secret for providers that still ask for one.
#[allow(dead_code)]
#[derive(Clone)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// A short-lived access token. `Debug` never prints the token.
#[allow(dead_code)]
#[derive(Clone)]
pub struct AccessToken {
    token: String,
    expires_at: Instant,
}

#[allow(dead_code)]
impl AccessToken {
    pub fn secret(&self) -> &str {
        &self.token
    }
    /// True when it is still good for at least `margin`.
    pub fn valid_for(&self, margin: Duration) -> bool {
        self.expires_at > Instant::now() + margin
    }
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Opens a URL in the user's browser.
#[allow(dead_code)]
pub trait BrowserOpener {
    fn open(&self, url: &str) -> Result<()>;
}

/// `ShellExecuteW("open", <https url>)`, the same call `ics.rs` uses for a
/// file. Refuses anything that is not an `https` URL.
pub struct ShellBrowser;

impl BrowserOpener for ShellBrowser {
    fn open(&self, url: &str) -> Result<()> {
        use windows::core::{w, PCWSTR};
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        if !url.starts_with("https://") {
            bail!("refusing to open a non-https sign-in URL");
        }
        let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the other
        // arguments are a static verb and `None`, as in `ics.rs`.
        let code = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if (code.0 as isize) > 32 {
            Ok(())
        } else {
            Err(anyhow!(
                "could not open the browser (ShellExecuteW code {})",
                code.0 as isize
            ))
        }
    }
}

/// POSTs a form and returns `(status, body)`. Any HTTP status is a normal
/// return; only a transport failure is an `Err`.
#[allow(dead_code)]
pub trait TokenTransport {
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String)>;
}

/// The real transport. Offline mode blocks it for any non-loopback host, and
/// plain `http` is refused unless the host is loopback.
pub struct UreqTransport;

impl TokenTransport for UreqTransport {
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String)> {
        let loopback = crate::mode::classify_host(url) == crate::mode::HostClass::Loopback;
        if !loopback && !url.starts_with("https://") {
            bail!("refusing to send credentials over plain http");
        }
        crate::provider::common::loopback_only_guard(url)?;
        let bytes_sent: usize = form.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
        let result = send_form(url, form);
        log_egress(url, bytes_sent, &result);
        result
    }
}

fn send_form(url: &str, form: &[(&str, &str)]) -> Result<(u16, String)> {
    crate::ui_thread::blocking_call("oauth send_form");
    let mut response = ureq::post(url)
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .send_form(form.iter().copied())
        .map_err(|e| anyhow!("sign-in request failed: {e}"))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .context("could not read the sign-in response")?;
    Ok((status, body))
}

/// Records one sign-in request in the egress log: host, path, status and
/// sizes only. The form and the reply carry the code, verifier and tokens, so
/// neither is ever passed in.
fn log_egress(url: &str, bytes_sent: usize, result: &Result<(u16, String)>) {
    use crate::egress::{build_entry, record, Outcome};
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host_and_path = after_scheme.split(['?', '#']).next().unwrap_or("");
    let (outcome, received, error) = match result {
        Ok((status, body)) if (200..300).contains(status) => (Outcome::Success, body.len(), None),
        Ok((status, body)) => (Outcome::Failure, body.len(), Some(format!("HTTP {status}"))),
        Err(_) => (Outcome::Failure, 0, Some("transport error".to_string())),
    };
    record(&build_entry(
        &format!("oauth {host_and_path}"),
        url,
        &Value::Null,
        false,
        bytes_sent,
        received,
        outcome,
        error.as_deref(),
    ));
}

// -- PKCE -------------------------------------------------------------------

/// A PKCE verifier and its S256 challenge (RFC 7636).
pub struct Pkce {
    verifier: String,
    challenge: String,
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    // SAFETY: `buf` is a valid writable slice for the whole call.
    unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .context("BCryptGenRandom failed")?;
    Ok(buf)
}

fn sha256(data: &[u8]) -> Result<[u8; 32]> {
    crate::knowledge::hash::sha256(data).map_err(|_| anyhow!("BCryptHash failed"))
}

/// `BASE64URL(SHA256(verifier))`, no padding.
fn s256_challenge(verifier: &str) -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(sha256(verifier.as_bytes())?))
}

impl Pkce {
    /// 32 random bytes, so a 43 character verifier.
    pub fn generate() -> Result<Self> {
        let verifier = URL_SAFE_NO_PAD.encode(random_bytes::<32>()?);
        let challenge = s256_challenge(&verifier)?;
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

fn random_state() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<24>()?))
}

// -- URL helpers --------------------------------------------------------------

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if bytes.get(i + 1..i + 3).is_some() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn build_auth_url(
    provider: &OAuthProvider,
    client: &OAuthClient,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> String {
    let scope = client.scopes.join(" ");
    let mut params: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", &client.client_id),
        ("redirect_uri", redirect_uri),
        ("scope", &scope),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ];
    params.extend(provider.extra_auth_params.iter().copied());
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", pct_encode(v)))
        .collect();
    let sep = if provider.auth_url.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{}{sep}{}", provider.auth_url, query.join("&"))
}

// -- Loopback listener --------------------------------------------------------

const MAX_REQUEST_BYTES: usize = 8 * 1024;

enum Callback {
    /// A request that is not the redirect (a favicon fetch, a probe).
    Other,
    Code {
        code: String,
        state: String,
    },
    /// `?error=...`; `state` is `None` when the request did not carry one.
    Denied {
        error: String,
        state: Option<String>,
    },
}

fn parse_callback(request_head: &str) -> Callback {
    let line = request_head.lines().next().unwrap_or("");
    let mut parts = line.split(' ');
    if parts.next() != Some("GET") {
        return Callback::Other;
    }
    let target = parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/" {
        return Callback::Other;
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(pct_decode(v)),
            "state" => state = Some(pct_decode(v)),
            "error" => error = Some(pct_decode(v)),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Callback::Denied { error, state };
    }
    match (code, state) {
        (Some(code), Some(state)) => Callback::Code { code, state },
        _ => Callback::Other,
    }
}

fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Wingman</title>\
         <body style=\"font-family:sans-serif;margin:3em\"><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// How long one connection may take to send its request line and headers,
/// in total (a client that dribbles bytes cannot stretch it).
const REQUEST_HEAD_BUDGET: Duration = Duration::from_secs(2);

fn read_request_head(stream: &mut TcpStream) -> String {
    let deadline = Instant::now() + REQUEST_HEAD_BUDGET;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < MAX_REQUEST_BYTES && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// How long past the deadline the caller waits for the watchdog's wake-up
/// connect before it gives up on the blocked accept and reports the timeout
/// itself.
const WAKE_GRACE: Duration = Duration::from_secs(2);

/// Waits for the redirect and returns the authorization code. The accept is
/// blocking, so nothing runs while the user is in the browser; a watchdog
/// thread wakes it by connecting to the port when `timeout` passes. If that
/// connect fails (security software dropping loopback, a full backlog) the
/// accept stays blocked, so it runs on its own thread and this function stops
/// waiting for it after [`WAKE_GRACE`]: the timeout still ends in a card. The
/// abandoned thread ends with the process or the next connection. Requests
/// that are not the real redirect (wrong path, wrong or missing `state`,
/// including an `error` with the wrong `state`) are answered and ignored, so
/// another page on this PC cannot abort or complete a sign-in it did not
/// start.
fn wait_for_code(
    listener: &TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    wait_for_code_with(listener, expected_state, timeout, wake_listener)
}

/// Connects to the listener's own port so a blocked `accept` returns.
/// False when the connect failed.
fn wake_listener(addr: std::net::SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

fn wait_for_code_with(
    listener: &TcpListener,
    expected_state: &str,
    timeout: Duration,
    wake: fn(std::net::SocketAddr) -> bool,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let addr = listener.local_addr()?;
    let accepting = listener.try_clone().context("loopback listener failed")?;
    let state = expected_state.to_string();
    let (result_tx, result_rx) = std::sync::mpsc::channel::<Result<String>>();
    std::thread::spawn(move || {
        let _ = result_tx.send(accept_until_redirect(&accepting, &state, deadline));
    });
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if done_rx.recv_timeout(timeout) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            let _ = wake(addr);
        }
    });
    let result = match result_rx.recv_timeout(timeout + WAKE_GRACE) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(anyhow!(
            "timed out waiting for sign-in to finish in the browser"
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(anyhow!("loopback listener failed"))
        }
    };
    drop(done_tx);
    let _ = watchdog.join();
    result
}

fn accept_until_redirect(
    listener: &TcpListener,
    expected_state: &str,
    deadline: Instant,
) -> Result<String> {
    let timed_out = || anyhow!("timed out waiting for sign-in to finish in the browser");
    let state_ok = |s: &str| constant_time_eq(s.as_bytes(), expected_state.as_bytes());
    loop {
        let (mut stream, _) = listener.accept().context("loopback listener failed")?;
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
        let head = read_request_head(&mut stream);
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
        match parse_callback(&head) {
            Callback::Other => respond(&mut stream, "404 Not Found", "Not found."),
            Callback::Denied {
                error,
                state: Some(state),
            } if state_ok(&state) => {
                respond(
                    &mut stream,
                    "200 OK",
                    "Sign-in was not completed. You can close this tab.",
                );
                bail!("sign-in was not completed ({})", sanitize(&error));
            }
            Callback::Code { code, state } if state_ok(&state) => {
                respond(
                    &mut stream,
                    "200 OK",
                    "Wingman is connected. You can close this tab.",
                );
                return Ok(code);
            }
            Callback::Denied { .. } | Callback::Code { .. } => {
                respond(&mut stream, "400 Bad Request", "Sign-in failed.")
            }
        }
    }
}
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Keeps a provider-supplied string short and printable before it goes into
/// an error message.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect::<String>()
}

// -- Token endpoint -----------------------------------------------------------

struct TokenReply {
    access: AccessToken,
    refresh: Option<String>,
}

fn parse_token_reply(status: u16, body: &str) -> Result<TokenReply> {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        return Err(anyhow::Error::new(TokenError {
            status,
            code: sanitize(v["error"].as_str().unwrap_or("unknown_error")),
            description: sanitize(v["error_description"].as_str().unwrap_or("")),
        }));
    }
    let token = v["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("token endpoint reply had no access token"))?
        .to_string();
    let expires_in = v["expires_in"].as_u64().unwrap_or(3600);
    Ok(TokenReply {
        access: AccessToken {
            token,
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        },
        refresh: v["refresh_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_string),
    })
}

fn with_client<'a>(
    client: &'a OAuthClient,
    mut form: Vec<(&'a str, &'a str)>,
) -> Vec<(&'a str, &'a str)> {
    form.push(("client_id", &client.client_id));
    if let Some(secret) = &client.client_secret {
        form.push(("client_secret", secret));
    }
    form
}

/// The token endpoint refused a request. Carries only the provider's `error`
/// code and description, never the response body.
#[derive(Debug)]
struct TokenError {
    status: u16,
    code: String,
    description: String,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "token endpoint refused the request (HTTP {}): {} {}",
            self.status, self.code, self.description
        )
    }
}

impl std::error::Error for TokenError {}

/// True only when the provider's own `error` code is `invalid_grant`, so the
/// stored refresh token is dead. Compares the parsed code, never the text.
fn is_invalid_grant(err: &anyhow::Error) -> bool {
    err.downcast_ref::<TokenError>()
        .is_some_and(|e| e.code == "invalid_grant")
}

// -- Public flow ----------------------------------------------------------------

/// Runs the whole sign-in and stores the refresh token. Blocks until the
/// user finishes, the provider refuses, or `timeout` passes.
#[allow(dead_code)]
pub fn authorize(
    provider: &OAuthProvider,
    client: &OAuthClient,
    opener: &dyn BrowserOpener,
    transport: &dyn TokenTransport,
    store: &dyn SecretStore,
    timeout: Duration,
) -> Result<AccessToken> {
    crate::ui_thread::blocking_call("oauth authorize");
    // Before anything is opened or bound: Offline and Local mode mean no sign-in (connectors are cloud services).
    crate::provider::common::loopback_only_guard(&provider.auth_url)?;
    let pkce = Pkce::generate()?;
    let state = random_state()?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("could not open a loopback port")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://{}:{port}/", provider.redirect_host);

    let url = build_auth_url(provider, client, &redirect_uri, &state, &pkce.challenge);
    opener.open(&url)?;
    let code = wait_for_code(&listener, &state, timeout)?;
    drop(listener);

    let form = with_client(
        client,
        vec![
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &pkce.verifier),
        ],
    );
    let (status, body) = transport.post_form(&provider.token_url, &form)?;
    let reply = parse_token_reply(status, &body)?;
    let refresh = reply.refresh.ok_or_else(|| {
        anyhow!("the provider returned no refresh token; sign-in cannot be remembered")
    })?;
    store.set(&provider.credential_target(), &refresh)?;
    Ok(reply.access)
}

/// True when a refresh token is stored (the account is connected).
#[allow(dead_code)]
pub fn is_connected(provider: &OAuthProvider, store: &dyn SecretStore) -> Result<bool> {
    Ok(store.get(&provider.credential_target())?.is_some())
}

/// Silent refresh: trades the stored refresh token for a new access token.
/// A provider that rotates refresh tokens has the new one stored. When the
/// provider says the grant is dead (`invalid_grant`), the stored token is
/// deleted so the next call says "not connected" instead of failing forever.
#[allow(dead_code)]
pub fn access_token(
    provider: &OAuthProvider,
    client: &OAuthClient,
    transport: &dyn TokenTransport,
    store: &dyn SecretStore,
) -> Result<AccessToken> {
    crate::provider::common::loopback_only_guard(&provider.token_url)?;
    let target = provider.credential_target();
    let refresh = store
        .get(&target)?
        .ok_or_else(|| anyhow!("{} is not connected; sign in first", provider.id))?;
    let scope = client.scopes.join(" ");
    let form = with_client(
        client,
        vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("scope", &scope),
        ],
    );
    let result = transport
        .post_form(&provider.token_url, &form)
        .and_then(|(status, body)| parse_token_reply(status, &body));
    match result {
        Ok(reply) => {
            if let Some(rotated) = reply.refresh {
                if rotated != refresh {
                    store.set(&target, &rotated)?;
                }
            }
            Ok(reply.access)
        }
        Err(e) if is_invalid_grant(&e) => {
            store.delete(&target)?;
            Err(e.context(format!("{} sign-in expired; sign in again", provider.id)))
        }
        Err(e) => Err(e),
    }
}

/// Revokes the stored token at the provider (when it has a revocation
/// endpoint) and always forgets it locally, even if the remote call fails or
/// Offline or Local mode blocks it (the call then returns that error).
#[allow(dead_code)]
pub fn revoke(
    provider: &OAuthProvider,
    transport: &dyn TokenTransport,
    store: &dyn SecretStore,
) -> Result<()> {
    let target = provider.credential_target();
    let Some(refresh) = store.get(&target)? else {
        return Ok(());
    };
    let remote = match &provider.revoke_url {
        Some(url) => crate::provider::common::loopback_only_guard(url)
            .and_then(|()| transport.post_form(url, &[("token", &refresh)]))
            .and_then(|(status, _)| {
                if (200..300).contains(&status) {
                    Ok(())
                } else {
                    Err(anyhow!("revocation endpoint answered HTTP {status}"))
                }
            }),
        None => Ok(()),
    };
    store.delete(&target)?;
    remote
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::InMemoryStore;
    use std::sync::{Arc, Mutex};

    /// Every test that reaches the Offline guard takes the shared mode lock
    /// and pins the mode, so a test in another module that sets Offline
    /// cannot leak into it.
    ///
    /// The egress-log lock comes first (the order `provider::common` uses):
    /// the real transport appends a row to the shared test log, which other
    /// tests save and restore.
    fn online() -> (
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let egress = crate::config::EGRESS_PREVIEW_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let guard = crate::mode::MODE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::mode::set_current(crate::mode::Mode::Auto);
        (egress, guard)
    }

    // -- PKCE ------------------------------------------------------------

    #[test]
    fn s256_challenge_matches_the_rfc7636_vector() {
        assert_eq!(
            s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pkce_is_43_url_safe_chars_and_differs_each_time() {
        let a = Pkce::generate().unwrap();
        let b = Pkce::generate().unwrap();
        assert_eq!(a.verifier.len(), 43);
        assert!(a
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a.verifier, b.verifier);
        assert_eq!(a.challenge, s256_challenge(&a.verifier).unwrap());
    }

    // -- URL and callback parsing ---------------------------------------------

    #[test]
    fn pct_round_trips_reserved_and_non_ascii_text() {
        let s = "a b&c=d/é~-._";
        assert_eq!(pct_decode(&pct_encode(s)), s);
        assert_eq!(pct_encode("a b"), "a%20b");
        assert_eq!(pct_decode("100%"), "100%");
        assert_eq!(pct_decode("%zz"), "%zz");
    }

    fn test_client() -> OAuthClient {
        OAuthClient {
            client_id: "cid".into(),
            client_secret: None,
            scopes: vec!["scope.one".into(), "offline_access".into()],
        }
    }

    #[test]
    fn auth_url_carries_pkce_state_redirect_and_scopes() {
        let url = build_auth_url(
            &OAuthProvider::google(),
            &test_client(),
            "http://127.0.0.1:5000/",
            "st8",
            "chal",
        );
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        for part in [
            "response_type=code",
            "client_id=cid",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A5000%2F",
            "scope=scope.one%20offline_access",
            "state=st8",
            "code_challenge=chal",
            "code_challenge_method=S256",
            "access_type=offline",
        ] {
            assert!(url.contains(part), "missing {part} in {url}");
        }
    }

    #[test]
    fn parse_callback_tells_the_redirect_from_noise() {
        assert!(matches!(
            parse_callback("GET /?code=a%2Fb&state=s HTTP/1.1\r\n\r\n"),
            Callback::Code { code, state } if code == "a/b" && state == "s"
        ));
        assert!(matches!(
            parse_callback("GET /?error=access_denied&state=s HTTP/1.1\r\n"),
            Callback::Denied { error, state: Some(s) } if error == "access_denied" && s == "s"
        ));
        assert!(matches!(
            parse_callback("GET /favicon.ico HTTP/1.1\r\n"),
            Callback::Other
        ));
        assert!(matches!(
            parse_callback("GET /?code=x HTTP/1.1\r\n"),
            Callback::Other
        ));
        assert!(matches!(
            parse_callback("POST /?code=x&state=s HTTP/1.1\r\n"),
            Callback::Other
        ));
    }

    // -- Fakes ------------------------------------------------------------------

    /// Does the HTTP GET a browser would do after consent.
    fn http_get(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    fn query_param(url: &str, key: &str) -> String {
        let q = url.split_once('?').unwrap().1;
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == key {
                    return pct_decode(v);
                }
            }
        }
        panic!("no {key} in {url}");
    }

    /// Plays the browser: records the consent URL and, depending on `how`,
    /// redirects to the listener.
    struct FakeBrowser {
        how: fn(&str) -> String,
        seen: Mutex<Vec<String>>,
        /// The response the loopback listener sent back.
        reply: Arc<Mutex<String>>,
    }

    impl FakeBrowser {
        fn new(how: fn(&str) -> String) -> Self {
            Self {
                how,
                seen: Mutex::new(Vec::new()),
                reply: Arc::new(Mutex::new(String::new())),
            }
        }
    }

    impl BrowserOpener for FakeBrowser {
        fn open(&self, url: &str) -> Result<()> {
            self.seen.lock().unwrap().push(url.to_string());
            let redirect = query_param(url, "redirect_uri");
            let port: u16 = redirect
                .trim_start_matches("http://127.0.0.1:")
                .trim_end_matches('/')
                .parse()
                .unwrap();
            let how = self.how;
            let url = url.to_string();
            let reply = self.reply.clone();
            // The listener is polled by the caller's thread, so the
            // "browser" must run on its own.
            std::thread::spawn(move || {
                let target = how(&url);
                *reply.lock().unwrap() = http_get(port, &target);
            });
            Ok(())
        }
    }

    fn approve(url: &str) -> String {
        format!(
            "/?code=AUTHCODE&state={}",
            pct_encode(&query_param(url, "state"))
        )
    }

    /// Scripted token endpoint on a real loopback socket, so the real
    /// transport is exercised too. Replies to each POST with the next
    /// scripted `(status, body)` and records `(path, form body)`.
    struct FakeEndpoint {
        base: String,
        requests: Arc<Mutex<Vec<(String, String)>>>,
    }

    fn fake_endpoint(script: Vec<(u16, String)>) -> FakeEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        std::thread::spawn(move || {
            for (status, reply) in script {
                let Ok((mut s, _)) = listener.accept() else {
                    return;
                };
                let mut raw = Vec::new();
                let mut chunk = [0u8; 2048];
                let (head_end, content_len) = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    raw.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&raw[..pos]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (pos + 4, len);
                    }
                };
                while raw.len() < head_end + content_len {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
                let path = head
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split(' ')
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let body = String::from_utf8_lossy(&raw[head_end..]).to_string();
                log.lock().unwrap().push((path, body));
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                );
            }
        });
        FakeEndpoint {
            base: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    fn form_value(body: &str, key: &str) -> Option<String> {
        body.split('&').find_map(|p| {
            let (k, v) = p.split_once('=')?;
            (k == key).then(|| pct_decode(v))
        })
    }

    fn provider_at(base: &str) -> OAuthProvider {
        OAuthProvider {
            id: "fake",
            auth_url: "https://example.invalid/auth".into(),
            token_url: format!("{base}/token"),
            revoke_url: Some(format!("{base}/revoke")),
            redirect_host: "127.0.0.1",
            extra_auth_params: Vec::new(),
        }
    }

    fn token_json(access: &str, refresh: Option<&str>) -> String {
        match refresh {
            Some(r) => format!(
                "{{\"access_token\":\"{access}\",\"refresh_token\":\"{r}\",\"expires_in\":3600}}"
            ),
            None => format!("{{\"access_token\":\"{access}\",\"expires_in\":3600}}"),
        }
    }

    // -- The flow -----------------------------------------------------------------

    #[test]
    fn authorize_exchanges_the_code_with_the_matching_verifier_and_stores_only_the_refresh_token() {
        let _mode = online();
        let ep = fake_endpoint(vec![(200, token_json("ACCESS1", Some("REFRESH1")))]);
        let provider = provider_at(&ep.base);
        let store = InMemoryStore::default();
        let browser = FakeBrowser::new(approve);

        let token = authorize(
            &provider,
            &test_client(),
            &browser,
            &UreqTransport,
            &store,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(token.secret(), "ACCESS1");
        assert!(!format!("{token:?}").contains("ACCESS1"));

        let consent = browser.seen.lock().unwrap()[0].clone();
        let reqs = ep.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        let (path, body) = &reqs[0];
        assert_eq!(path, "/token");
        assert_eq!(
            form_value(body, "grant_type").unwrap(),
            "authorization_code"
        );
        assert_eq!(form_value(body, "code").unwrap(), "AUTHCODE");
        assert_eq!(form_value(body, "client_id").unwrap(), "cid");
        assert_eq!(
            form_value(body, "redirect_uri").unwrap(),
            query_param(&consent, "redirect_uri")
        );
        // The verifier sent now must hash to the challenge sent earlier.
        let verifier = form_value(body, "code_verifier").unwrap();
        assert_eq!(
            s256_challenge(&verifier).unwrap(),
            query_param(&consent, "code_challenge")
        );

        assert_eq!(
            store.get(&provider.credential_target()).unwrap().as_deref(),
            Some("REFRESH1")
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(browser.reply.lock().unwrap().contains("200 OK"));
    }

    #[test]
    fn authorize_ignores_forged_callbacks_and_still_completes_on_the_real_one() {
        let _mode = online();
        let ep = fake_endpoint(vec![(200, token_json("A", Some("R")))]);
        let store = InMemoryStore::default();
        // A forged code, a forged denial and a denial with no state arrive
        // first; none may abort or complete the flow.
        fn forged_then_real(url: &str) -> String {
            let redirect = query_param(url, "redirect_uri");
            let port: u16 = redirect
                .trim_start_matches("http://127.0.0.1:")
                .trim_end_matches('/')
                .parse()
                .unwrap();
            for bad in [
                "/?code=EVIL&state=forged",
                "/?error=access_denied&state=forged",
                "/?error=access_denied",
                "/?code=EVIL",
            ] {
                assert!(http_get(port, bad).contains("400 Bad Request") || bad == "/?code=EVIL");
            }
            approve(url)
        }
        let token = authorize(
            &provider_at(&ep.base),
            &test_client(),
            &FakeBrowser::new(forged_then_real),
            &UreqTransport,
            &store,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(token.secret(), "A");
        let reqs = ep.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(form_value(&reqs[0].1, "code").unwrap(), "AUTHCODE");
    }

    #[test]
    fn a_slow_dribbling_client_cannot_hold_the_flow_past_the_timeout() {
        let _mode = online();
        struct Dribbler;
        impl BrowserOpener for Dribbler {
            fn open(&self, url: &str) -> Result<()> {
                let redirect = query_param(url, "redirect_uri");
                let port: u16 = redirect
                    .trim_start_matches("http://127.0.0.1:")
                    .trim_end_matches('/')
                    .parse()
                    .unwrap();
                std::thread::spawn(move || {
                    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                    for _ in 0..40 {
                        if s.write_all(b"X").is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                });
                Ok(())
            }
        }
        let started = Instant::now();
        let err = authorize(
            &provider_at("http://127.0.0.1:1"),
            &test_client(),
            &Dribbler,
            &UreqTransport,
            &InMemoryStore::default(),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "took {:?}",
            started.elapsed()
        );
    }

    struct CountingBrowser(std::sync::atomic::AtomicUsize);
    impl BrowserOpener for CountingBrowser {
        fn open(&self, _: &str) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    struct CountingTransport(std::sync::atomic::AtomicUsize);
    impl TokenTransport for CountingTransport {
        fn post_form(&self, _: &str, _: &[(&str, &str)]) -> Result<(u16, String)> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((200, "{}".into()))
        }
    }

    fn offline_google() -> (
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = online();
        crate::mode::set_current(crate::mode::Mode::Offline);
        guard
    }

    #[test]
    fn offline_mode_blocks_authorize_before_the_browser_opens() {
        let _mode = offline_google();
        let browser = CountingBrowser(Default::default());
        let result = authorize(
            &OAuthProvider::google(),
            &test_client(),
            &browser,
            &UreqTransport,
            &InMemoryStore::default(),
            Duration::from_millis(200),
        );
        crate::mode::set_current(crate::mode::Mode::Auto);
        assert!(result.unwrap_err().to_string().contains("Offline"));
        assert_eq!(browser.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// #686: Local mode keeps everything on this computer, so the sign-in,
    /// refresh and revoke requests to a cloud host are refused there too.
    #[test]
    fn local_mode_blocks_sign_in_refresh_and_revoke_before_any_request() {
        struct RestoreMode;
        impl Drop for RestoreMode {
            fn drop(&mut self) {
                crate::mode::set_current(crate::mode::Mode::Auto);
            }
        }
        let _lock = online();
        let _restore = RestoreMode;
        crate::mode::set_current(crate::mode::Mode::Local);
        let provider = OAuthProvider::google();
        let store = InMemoryStore::default();
        store.set(&provider.credential_target(), "KEEP").unwrap();

        let browser = CountingBrowser(Default::default());
        let err = authorize(
            &provider,
            &test_client(),
            &browser,
            &UreqTransport,
            &store,
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Local"), "{err}");
        assert_eq!(browser.0.load(std::sync::atomic::Ordering::SeqCst), 0);

        let transport = CountingTransport(Default::default());
        let err = access_token(&provider, &test_client(), &transport, &store).unwrap_err();
        assert!(err.to_string().contains("Local"), "{err}");
        assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(is_connected(&provider, &store).unwrap());

        assert!(revoke(&provider, &transport, &store).is_err());
        assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        // The remote revoke is skipped, but the local token is still deleted.
        assert!(!is_connected(&provider, &store).unwrap());
    }

    #[test]
    fn offline_mode_blocks_a_silent_refresh() {
        let _mode = offline_google();
        let provider = OAuthProvider::google();
        let store = InMemoryStore::default();
        store.set(&provider.credential_target(), "KEEP").unwrap();
        let transport = CountingTransport(Default::default());
        let result = access_token(&provider, &test_client(), &transport, &store);
        crate::mode::set_current(crate::mode::Mode::Auto);
        assert!(result.is_err());
        assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(is_connected(&provider, &store).unwrap());
    }

    #[test]
    fn offline_mode_skips_the_remote_revoke_but_still_forgets_locally() {
        let _mode = offline_google();
        let provider = OAuthProvider::google();
        let store = InMemoryStore::default();
        store.set(&provider.credential_target(), "BYE").unwrap();
        let transport = CountingTransport(Default::default());
        let result = revoke(&provider, &transport, &store);
        crate::mode::set_current(crate::mode::Mode::Auto);
        assert!(result.is_err());
        assert_eq!(transport.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!is_connected(&provider, &store).unwrap());
    }

    #[test]
    fn a_transient_error_that_mentions_invalid_grant_keeps_the_refresh_token() {
        let _mode = online();
        let ep = fake_endpoint(vec![(
            503,
            "{\"error\":\"temporarily_unavailable\",\"error_description\":\"not invalid_grant\"}"
                .into(),
        )]);
        let provider = provider_at(&ep.base);
        let store = InMemoryStore::default();
        store.set(&provider.credential_target(), "KEEP").unwrap();
        assert!(access_token(&provider, &test_client(), &UreqTransport, &store).is_err());
        assert!(is_connected(&provider, &store).unwrap());
    }

    #[test]
    fn only_files_that_must_use_ureq_mention_it() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root];
        let mut scanned = 0;
        let mut users = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    let text = std::fs::read_to_string(&path).unwrap().replace('\r', "");
                    scanned += 1;
                    if text.contains("ureq::") || text.contains("use ureq") {
                        users.push(path.file_name().unwrap().to_string_lossy().into_owned());
                    }
                }
            }
        }
        assert!(scanned > 10, "scanned only {scanned} files");
        users.sort();
        assert_eq!(
            users,
            ["common.rs", "oauth.rs", "web_page.rs"],
            "ureq may only be used in provider/common.rs, connectors/oauth.rs and \
             inputs/web_page.rs, which carry the Offline guard"
        );
    }
    #[test]

    fn authorize_reports_a_denied_consent_without_the_endpoint() {
        let _mode = online();
        let ep = fake_endpoint(vec![]);
        let store = InMemoryStore::default();
        let browser = FakeBrowser::new(|url| {
            format!(
                "/?error=access_denied&state={}",
                pct_encode(&query_param(url, "state"))
            )
        });
        let err = authorize(
            &provider_at(&ep.base),
            &test_client(),
            &browser,
            &UreqTransport,
            &store,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("access_denied"), "{err}");
    }

    #[test]
    fn authorize_ignores_a_stray_request_and_still_gets_the_real_redirect() {
        let _mode = online();
        let ep = fake_endpoint(vec![(200, token_json("A", Some("R")))]);
        let store = InMemoryStore::default();
        // Hits a wrong path first, then the real redirect.
        fn noisy(url: &str) -> String {
            let redirect = query_param(url, "redirect_uri");
            let port: u16 = redirect
                .trim_start_matches("http://127.0.0.1:")
                .trim_end_matches('/')
                .parse()
                .unwrap();
            let _ = http_get(port, "/favicon.ico");
            approve(url)
        }
        let browser = FakeBrowser::new(noisy);
        authorize(
            &provider_at(&ep.base),
            &test_client(),
            &browser,
            &UreqTransport,
            &store,
            Duration::from_secs(10),
        )
        .unwrap();
    }

    #[test]
    fn authorize_times_out_when_nobody_finishes_sign_in() {
        let _mode = online();
        struct Silent;
        impl BrowserOpener for Silent {
            fn open(&self, _: &str) -> Result<()> {
                Ok(())
            }
        }
        let store = InMemoryStore::default();
        let err = authorize(
            &provider_at("http://127.0.0.1:1"),
            &test_client(),
            &Silent,
            &UreqTransport,
            &store,
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[test]
    fn authorize_ends_in_a_timeout_when_the_watchdog_cannot_wake_the_accept() {
        // The wake-up connect failing (security software dropping loopback)
        // used to leave `accept` blocked past the deadline for good.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let started = Instant::now();
        let err = wait_for_code_with(&listener, "state", Duration::from_millis(200), |_| false)
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "gave up too late: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_working_wake_up_still_ends_the_wait_at_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let started = Instant::now();
        let err = wait_for_code_with(
            &listener,
            "state",
            Duration::from_millis(200),
            wake_listener,
        )
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < WAKE_GRACE, "{:?}", started.elapsed());
    }

    #[test]
    fn the_egress_row_for_sign_in_traffic_holds_no_query_form_or_reply_secret() {
        let _mode = online();
        let reply = token_json("ACCESSLEAK", Some("REFRESHLEAK"));
        let ep = fake_endpoint(vec![(200, reply), (400, "BODYLEAK".to_string())]);
        let before_rows = crate::egress::rows().unwrap().len();
        let url = format!("{}/token?client_secret=QUERYLEAK#FRAGLEAK", ep.base);
        let form = [
            ("code", "CODELEAK"),
            ("code_verifier", "VERIFIERLEAK"),
            ("refresh_token", "FORMTOKENLEAK"),
        ];
        UreqTransport.post_form(&url, &form).unwrap();
        UreqTransport.post_form(&url, &form).unwrap();
        let rows: Vec<String> = crate::egress::rows()
            .unwrap()
            .iter()
            .skip(before_rows)
            .map(|e| serde_json::to_string(e).unwrap())
            .collect();
        let host = ep.base.trim_start_matches("http://").to_string();
        let mine: Vec<&String> = rows
            .iter()
            .filter(|r| r.contains(&format!("oauth {host}/token")))
            .collect();
        assert_eq!(mine.len(), 2, "one row per request: {rows:?}");
        for row in &mine {
            for secret in [
                "QUERYLEAK",
                "FRAGLEAK",
                "CODELEAK",
                "VERIFIERLEAK",
                "FORMTOKENLEAK",
                "ACCESSLEAK",
                "REFRESHLEAK",
                "BODYLEAK",
                "client_secret",
            ] {
                assert!(!row.contains(secret), "{secret} reached the log: {row}");
            }
        }
        assert!(mine[0].contains("\"outcome\":\"success\""), "{}", mine[0]);
        assert!(mine[1].contains("HTTP 400"), "{}", mine[1]);
    }

    #[test]
    fn authorize_fails_when_the_provider_returns_no_refresh_token() {
        let _mode = online();
        let ep = fake_endpoint(vec![(200, token_json("A", None))]);
        let store = InMemoryStore::default();
        let err = authorize(
            &provider_at(&ep.base),
            &test_client(),
            &FakeBrowser::new(approve),
            &UreqTransport,
            &store,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("refresh token"), "{err}");
    }

    #[test]
    fn a_refused_exchange_reports_the_error_code_but_not_the_response_body() {
        let _mode = online();
        let ep = fake_endpoint(vec![(
            400,
            "{\"error\":\"invalid_request\",\"error_description\":\"bad\",\"access_token\":\"LEAK\"}"
                .into(),
        )]);
        let store = InMemoryStore::default();
        let err = authorize(
            &provider_at(&ep.base),
            &test_client(),
            &FakeBrowser::new(approve),
            &UreqTransport,
            &store,
            Duration::from_secs(10),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("invalid_request") && !err.contains("LEAK"),
            "{err}"
        );
    }

    // -- Refresh and revoke --------------------------------------------------------

    #[test]
    fn access_token_refreshes_silently_and_stores_a_rotated_refresh_token() {
        let _mode = online();
        let ep = fake_endpoint(vec![(200, token_json("ACCESS2", Some("REFRESH2")))]);
        let provider = provider_at(&ep.base);
        let store = InMemoryStore::default();
        store
            .set(&provider.credential_target(), "REFRESH1")
            .unwrap();

        let token = access_token(&provider, &test_client(), &UreqTransport, &store).unwrap();
        assert_eq!(token.secret(), "ACCESS2");
        assert!(token.valid_for(Duration::from_secs(60)));
        let body = ep.requests.lock().unwrap()[0].1.clone();
        assert_eq!(form_value(&body, "grant_type").unwrap(), "refresh_token");
        assert_eq!(form_value(&body, "refresh_token").unwrap(), "REFRESH1");
        assert_eq!(
            store.get(&provider.credential_target()).unwrap().as_deref(),
            Some("REFRESH2")
        );
    }

    #[test]
    fn access_token_without_a_stored_token_says_to_sign_in() {
        let _mode = online();
        let err = access_token(
            &provider_at("http://127.0.0.1:1"),
            &test_client(),
            &UreqTransport,
            &InMemoryStore::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("sign in"), "{err}");
    }

    #[test]
    fn invalid_grant_forgets_the_dead_refresh_token() {
        let _mode = online();
        let ep = fake_endpoint(vec![(400, "{\"error\":\"invalid_grant\"}".into())]);
        let provider = provider_at(&ep.base);
        let store = InMemoryStore::default();
        store.set(&provider.credential_target(), "OLD").unwrap();
        let err = access_token(&provider, &test_client(), &UreqTransport, &store).unwrap_err();
        assert!(err.to_string().contains("sign in again"), "{err}");
        assert!(!is_connected(&provider, &store).unwrap());
    }

    #[test]
    fn a_transient_refresh_failure_keeps_the_refresh_token() {
        let _mode = online();
        let ep = fake_endpoint(vec![(
            503,
            "{\"error\":\"temporarily_unavailable\"}".into(),
        )]);
        let provider = provider_at(&ep.base);
        let store = InMemoryStore::default();
        store.set(&provider.credential_target(), "KEEP").unwrap();
        assert!(access_token(&provider, &test_client(), &UreqTransport, &store).is_err());
        assert!(is_connected(&provider, &store).unwrap());
    }

    #[test]
    fn revoke_posts_the_token_and_forgets_it() {
        let _mode = online();
        let ep = fake_endpoint(vec![(200, "{}".into())]);
        let provider = provider_at(&ep.base);
        let store = InMemoryStore::default();
        store.set(&provider.credential_target(), "BYE").unwrap();
        revoke(&provider, &UreqTransport, &store).unwrap();
        let reqs = ep.requests.lock().unwrap();
        assert_eq!(reqs[0].0, "/revoke");
        assert_eq!(form_value(&reqs[0].1, "token").unwrap(), "BYE");
        assert!(!is_connected(&provider, &store).unwrap());
    }

    #[test]
    fn revoke_forgets_locally_even_when_the_provider_call_fails() {
        let _mode = online();
        let ep = fake_endpoint(vec![(500, "{}".into())]);
        let provider = provider_at(&ep.base);
        let store = InMemoryStore::default();
        store.set(&provider.credential_target(), "BYE").unwrap();
        assert!(revoke(&provider, &UreqTransport, &store).is_err());
        assert!(!is_connected(&provider, &store).unwrap());
    }

    #[test]
    fn revoke_without_a_revocation_endpoint_only_forgets_locally() {
        let _mode = online();
        let provider = OAuthProvider::microsoft();
        let store = InMemoryStore::default();
        store.set(&provider.credential_target(), "BYE").unwrap();
        // The unreachable transport would fail the test if it were called.
        struct Never;
        impl TokenTransport for Never {
            fn post_form(&self, _: &str, _: &[(&str, &str)]) -> Result<(u16, String)> {
                panic!("no network call expected");
            }
        }
        revoke(&provider, &Never, &store).unwrap();
        assert!(!is_connected(&provider, &store).unwrap());
    }

    // -- Safety rails ----------------------------------------------------------------

    #[test]
    fn real_transport_refuses_plain_http_to_a_non_loopback_host() {
        let _mode = online();
        for mode in [crate::mode::Mode::Auto, crate::mode::Mode::Offline] {
            crate::mode::set_current(mode);
            let err = UreqTransport
                .post_form("http://example.com/token", &[("a", "b")])
                .unwrap_err();
            assert!(err.to_string().contains("plain http"), "{mode:?}: {err}");
        }
        crate::mode::set_current(crate::mode::Mode::Auto);
    }
    #[test]
    fn shell_browser_refuses_a_non_https_url() {
        assert!(ShellBrowser
            .open("file:///c:/windows/system32/calc.exe")
            .is_err());
    }

    #[test]
    fn debug_output_never_shows_the_client_secret() {
        let c = OAuthClient {
            client_id: "cid".into(),
            client_secret: Some("SHHH".into()),
            scopes: vec![],
        };
        assert!(!format!("{c:?}").contains("SHHH"));
    }

    #[test]
    fn credential_targets_are_per_provider_and_never_a_bare_provider_key_name() {
        assert_eq!(
            OAuthProvider::google().credential_target(),
            "Wingman/oauth-google"
        );
        assert_eq!(
            OAuthProvider::microsoft().credential_target(),
            "Wingman/oauth-microsoft"
        );
    }
}
