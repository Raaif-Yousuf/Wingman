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
use windows::Win32::Security::Cryptography::{
    BCryptGenRandom, BCryptHash, BCRYPT_SHA256_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};

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
        let class = crate::mode::classify_host(url);
        let loopback = class == crate::mode::HostClass::Loopback;
        if !loopback && !url.starts_with("https://") {
            bail!("refusing to send credentials over plain http");
        }
        if crate::mode::is_offline_now() && !loopback {
            bail!("Offline mode blocked the sign-in request. Turn off Offline mode to connect an account.");
        }
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
    let mut out = [0u8; 32];
    // SAFETY: both slices are valid for the whole call; the algorithm handle
    // is a pseudo-handle owned by the OS.
    unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) }
        .ok()
        .context("BCryptHash failed")?;
    Ok(out)
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
    Denied {
        error: String,
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
        return Callback::Denied { error };
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

fn read_request_head(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < MAX_REQUEST_BYTES && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Waits for the redirect and returns the authorization code. Polls the
/// non-blocking listener only while a sign-in is in progress.
fn wait_for_code(
    listener: &TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let head = read_request_head(&mut stream);
                match parse_callback(&head) {
                    Callback::Other => respond(&mut stream, "404 Not Found", "Not found."),
                    Callback::Denied { error } => {
                        respond(
                            &mut stream,
                            "200 OK",
                            "Sign-in was not completed. You can close this tab.",
                        );
                        bail!("sign-in was not completed ({})", sanitize(&error));
                    }
                    Callback::Code { code, state } => {
                        if !constant_time_eq(state.as_bytes(), expected_state.as_bytes()) {
                            respond(&mut stream, "400 Bad Request", "Sign-in failed.");
                            bail!("sign-in response had the wrong state value; ignored");
                        }
                        respond(
                            &mut stream,
                            "200 OK",
                            "Wingman is connected. You can close this tab.",
                        );
                        return Ok(code);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for sign-in to finish in the browser");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e).context("loopback listener failed"),
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
        let error = v["error"].as_str().unwrap_or("unknown_error");
        let desc = v["error_description"].as_str().unwrap_or("");
        bail!(
            "token endpoint refused the request (HTTP {status}): {} {}",
            sanitize(error),
            sanitize(desc)
        );
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

/// True when a refused request means the stored refresh token is dead.
fn is_invalid_grant(err: &anyhow::Error) -> bool {
    err.to_string().contains("invalid_grant")
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
/// endpoint) and always forgets it locally, even if the remote call fails.
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
        Some(url) => transport
            .post_form(url, &[("token", &refresh)])
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
            Callback::Denied { error } if error == "access_denied"
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
    fn authorize_rejects_a_wrong_state_and_never_calls_the_token_endpoint() {
        let ep = fake_endpoint(vec![]);
        let store = InMemoryStore::default();
        let browser = FakeBrowser::new(|_| "/?code=EVIL&state=forged".to_string());
        let err = authorize(
            &provider_at(&ep.base),
            &test_client(),
            &browser,
            &UreqTransport,
            &store,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("state"), "{err}");
        assert!(ep.requests.lock().unwrap().is_empty());
        assert!(store
            .get(&provider_at(&ep.base).credential_target())
            .unwrap()
            .is_none());
    }

    #[test]
    fn authorize_reports_a_denied_consent_without_the_endpoint() {
        let ep = fake_endpoint(vec![]);
        let store = InMemoryStore::default();
        let browser = FakeBrowser::new(|_| "/?error=access_denied".to_string());
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
    fn authorize_fails_when_the_provider_returns_no_refresh_token() {
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
        let ep = fake_endpoint(vec![(500, "{}".into())]);
        let provider = provider_at(&ep.base);
        let store = InMemoryStore::default();
        store.set(&provider.credential_target(), "BYE").unwrap();
        assert!(revoke(&provider, &UreqTransport, &store).is_err());
        assert!(!is_connected(&provider, &store).unwrap());
    }

    #[test]
    fn revoke_without_a_revocation_endpoint_only_forgets_locally() {
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
        let err = UreqTransport
            .post_form("http://example.com/token", &[("a", "b")])
            .unwrap_err();
        assert!(err.to_string().contains("plain http"), "{err}");
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
