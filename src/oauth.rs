//! Signing in with Microsoft or Google (OAuth2) without a program of the
//! user's: the browser opens on the provider's sign-in page, the provider
//! sends it back to a listener of tuimail's on this computer with a code,
//! and tuimail trades the code for tokens. The refresh token, which lasts,
//! goes to the keychain (`secrets.rs`); access tokens, which last an hour,
//! are got with it as needed and kept in memory.
//!
//! - PKCE (RFC 7636, S256) and a random `state`: a code is only good to
//!   the tuimail that asked for it, and a callback is only taken if it
//!   carries the state.
//! - The listener is on the loopback address only, takes one sign-in, and
//!   stops after [`SIGN_IN_FOR`].
//! - Tokens come over HTTPS through `net.rs`, as mail does (rustls, the
//!   system's verifier), with a time limit and a size cap, and are checked
//!   before they're used in a login.
//!
//! Providers only sign users in for apps registered with them: the client
//! ids are tuimail's ([`MICROSOFT_CLIENT_ID`], [`GOOGLE_CLIENT_ID`]) or an
//! account's own (`oauth_client_id`).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::account::{Account, Auth, Security};
use crate::net::{self, Endpoint};
use crate::secrets::Secret;
use crate::text;

/// tuimail's registrations with Microsoft (Entra ID, a public client: no
/// secret) and Google (a desktop client, whose "secret" isn't one: Google
/// says so for installed apps). Empty until registered: then an account
/// must name its own.
pub const MICROSOFT_CLIENT_ID: &str = "";
pub const GOOGLE_CLIENT_ID: &str = "";
pub const GOOGLE_CLIENT_SECRET: &str = "";

/// How long the browser has to come back.
pub const SIGN_IN_FOR: Duration = Duration::from_secs(5 * 60);
/// Requests to the provider: their time limit and the most read.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE: usize = 256 << 10;
/// The longest token taken.
const MAX_TOKEN: usize = 16 << 10;
/// An access token is renewed this long before it runs out.
const EARLY: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Microsoft,
    Google,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::Microsoft => "Microsoft",
            Provider::Google => "Google",
        }
    }

    /// The sending servers its tokens are for: a token goes to no other.
    pub fn smtp_hosts(self) -> &'static [&'static str] {
        match self {
            Provider::Microsoft => &["smtp.office365.com", "smtp-mail.outlook.com"],
            Provider::Google => &["smtp.gmail.com"],
        }
    }

    fn authorize(self) -> &'static str {
        match self {
            Provider::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            Provider::Google => "https://accounts.google.com/o/oauth2/v2/auth",
        }
    }

    /// The token endpoint: host and path.
    fn token(self) -> (&'static str, &'static str) {
        match self {
            Provider::Microsoft => ("login.microsoftonline.com", "/common/oauth2/v2.0/token"),
            Provider::Google => ("oauth2.googleapis.com", "/token"),
        }
    }

    fn scope(self) -> &'static str {
        match self {
            Provider::Microsoft => {
                "https://outlook.office.com/IMAP.AccessAsUser.All \
                 https://outlook.office.com/POP.AccessAsUser.All \
                 https://outlook.office.com/SMTP.Send offline_access"
            }
            Provider::Google => "https://mail.google.com/",
        }
    }

    /// Where the browser is sent back: Microsoft registers `localhost`,
    /// Google the loopback address; the port is the listener's.
    fn redirect(self, port: u16) -> String {
        match self {
            Provider::Microsoft => format!("http://localhost:{port}"),
            Provider::Google => format!("http://127.0.0.1:{port}"),
        }
    }
}

/// The provider whose sign-in an account's servers take.
pub fn provider_for(account: &Account) -> Option<Provider> {
    let host = account.host.trim().to_lowercase();
    let host = host.trim_end_matches('.');
    if [
        "outlook.office365.com",
        "outlook.office.com",
        "imap-mail.outlook.com",
        "pop-mail.outlook.com",
    ]
    .contains(&host)
    {
        return Some(Provider::Microsoft);
    }
    if ["imap.gmail.com", "pop.gmail.com"].contains(&host) {
        return Some(Provider::Google);
    }
    None
}

/// An app registered with a provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    pub provider: Provider,
    pub id: String,
    /// Google's for desktop apps (not secret); none for Microsoft.
    pub secret: String,
}

/// The app an account signs in as: its own, else tuimail's; `None` if
/// there's neither, or its servers aren't Microsoft's or Google's.
pub fn client_for(account: &Account) -> Option<Client> {
    let provider = provider_for(account)?;
    let (id, secret) = if account.oauth_client_id.trim().is_empty() {
        match provider {
            Provider::Microsoft => (MICROSOFT_CLIENT_ID, ""),
            Provider::Google => (GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET),
        }
    } else {
        (
            account.oauth_client_id.trim(),
            account.oauth_client_secret.trim(),
        )
    };
    (!id.is_empty()).then(|| Client {
        provider,
        id: id.to_string(),
        secret: secret.to_string(),
    })
}

/// Whether an account signs in here (OAuth2 without a password_command).
pub fn built_in(account: &Account) -> bool {
    account.auth == Auth::Oauth2 && account.password_command.is_empty()
}

/// What a sign-in or a refresh gives.
#[derive(Clone, Debug)]
pub struct Tokens {
    pub access: Secret,
    /// A new refresh token, if one came (always after a sign-in;
    /// sometimes after a refresh: then it replaces the old one).
    pub refresh: Option<Secret>,
    /// When `access` should be renewed.
    pub renew_at: Instant,
}

impl Tokens {
    pub fn fresh(&self) -> bool {
        Instant::now() < self.renew_at
    }
}

/// A sign-in under way: the browser is to open `url`.
pub struct SignIn {
    pub url: String,
    client: Client,
    verifier: String,
    state: String,
    redirect: String,
    listeners: Vec<TcpListener>,
}

/// Both loopback addresses, one port, since `localhost` (where the browser
/// comes back) can be either: a port another program holds on `[::1]` is
/// never used, or the browser could take the sign-in's return there. With
/// no IPv6 loopback at all, `127.0.0.1` alone.
async fn loopback() -> Result<(u16, Vec<TcpListener>)> {
    for _ in 0..20 {
        let v4 = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("cannot listen for the browser")?;
        let port = v4.local_addr()?.port();
        match TcpListener::bind(("::1", port)).await {
            Ok(v6) => return Ok((port, vec![v4, v6])),
            Err(e) if no_ipv6(&e) => return Ok((port, vec![v4])),
            // Taken there: another port.
            Err(_) => continue,
        }
    }
    bail!("cannot listen for the browser: no port is free on both loopback addresses")
}

/// The computer has no IPv6 loopback to listen on.
fn no_ipv6(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::AddrNotAvailable | ErrorKind::Unsupported
    ) || e.raw_os_error() == Some(eafnosupport())
}

/// EAFNOSUPPORT: the address family isn't there.
fn eafnosupport() -> i32 {
    #[cfg(unix)]
    return libc::EAFNOSUPPORT;
    #[cfg(windows)]
    return 10047; // WSAEAFNOSUPPORT
}

impl SignIn {
    /// Starts listening for the browser's return, and makes the link that
    /// opens the provider's sign-in page (with `email` filled in).
    pub async fn start(client: Client, email: &str) -> Result<Self> {
        let (port, listeners) = loopback().await?;
        let verifier = random_token()?;
        let state = random_token()?;
        let redirect = client.provider.redirect(port);
        let mut params = vec![
            ("client_id", client.id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("scope", client.provider.scope()),
            ("state", state.as_str()),
            ("login_hint", email),
            ("code_challenge_method", "S256"),
        ];
        let challenge = challenge(&verifier);
        params.push(("code_challenge", &challenge));
        if client.provider == Provider::Google {
            // A refresh token every time, not only the first.
            params.extend([("access_type", "offline"), ("prompt", "consent")]);
        }
        let url = format!("{}?{}", client.provider.authorize(), form(&params));
        Ok(Self {
            url,
            client,
            verifier,
            state,
            redirect,
            listeners,
        })
    }

    /// Waits for the browser to come back (at most [`SIGN_IN_FOR`]), then
    /// trades its code for tokens.
    pub async fn finish(self) -> Result<Tokens> {
        let code = tokio::time::timeout(SIGN_IN_FOR, self.code())
            .await
            .map_err(|_| anyhow!("the sign-in wasn't finished in time"))??;
        let mut params = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", self.redirect.as_str()),
            ("client_id", self.client.id.as_str()),
            ("code_verifier", self.verifier.as_str()),
        ];
        if !self.client.secret.is_empty() {
            params.push(("client_secret", self.client.secret.as_str()));
        }
        let tokens = token_request(self.client.provider, &params).await?;
        if tokens.refresh.is_none() {
            bail!("{} gave no refresh token", self.client.provider.name());
        }
        Ok(tokens)
    }

    /// The code the browser brings back with our state.
    async fn code(&self) -> Result<String> {
        if self.listeners.is_empty() {
            bail!("no listener");
        }
        // Each connection on its own, a few at a time: one that sends
        // nothing (from another program that sees the port) can't keep the
        // browser's return waiting.
        let mut answers = tokio::task::JoinSet::new();
        loop {
            let accepted = tokio::select! {
                accepted = accept(&self.listeners) => accepted,
                Some(done) = answers.join_next(), if !answers.is_empty() => match done {
                    Ok(Some(answer)) => return answer,
                    // Something else (a favicon), or not ours: wait on.
                    _ => continue,
                },
            };
            let Ok((mut stream, peer)) = accepted else {
                continue;
            };
            if !peer.ip().is_loopback() || answers.len() >= 16 {
                continue;
            }
            let state = self.state.clone();
            answers.spawn(async move {
                match callback(&mut stream, &state).await {
                    Ok(Some(answer)) => Some(answer),
                    Ok(None) | Err(_) => None,
                }
            });
        }
    }
}

/// The next connection on either loopback address.
async fn accept(
    listeners: &[TcpListener],
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    match listeners {
        [v4] => v4.accept().await,
        [v4, v6, ..] => tokio::select! {
            a = v4.accept() => a,
            a = v6.accept() => a,
        },
        [] => std::future::pending().await,
    }
}

/// Reads one request from the browser. `Some` with the code (or the
/// provider's refusal) if it's the sign-in's return, with our state.
async fn callback<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    state: &str,
) -> Result<Option<Result<String>>> {
    let head = tokio::time::timeout(Duration::from_secs(10), read_head(stream, 8 << 10))
        .await
        .map_err(|_| anyhow!("too slow"))??;
    let line = head.lines().next().unwrap_or("");
    let target = match line.split(' ').collect::<Vec<_>>().as_slice() {
        ["GET", target, version] if version.starts_with("HTTP/") => *target,
        _ => bail!("not a request"),
    };
    let query = target.strip_prefix("/?").unwrap_or("");
    let params = parse_query(query);
    let get = |name: &str| {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    if get("state").as_deref() != Some(state) {
        respond(stream, "404 Not Found", "Nothing here.").await;
        return Ok(None);
    }
    if let Some(error) = get("error") {
        respond(
            stream,
            "200 OK",
            "The sign-in didn't finish. You can close this tab.",
        )
        .await;
        let said = get("error_description").unwrap_or(error);
        return Ok(Some(Err(anyhow!(
            "the sign-in was refused: {}",
            text::one_line(&said, 200)
        ))));
    }
    let Some(code) = get("code").filter(|c| !c.is_empty() && c.len() <= MAX_TOKEN) else {
        respond(stream, "400 Bad Request", "No code came back.").await;
        return Ok(None);
    };
    respond(
        stream,
        "200 OK",
        "Signed in to tuimail. You can close this tab and go back to your terminal.",
    )
    .await;
    Ok(Some(Ok(code)))
}

/// The browser's page: plain text in HTML, nothing loaded.
async fn respond<S: AsyncWrite + Unpin>(stream: &mut S, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>tuimail</title>\
         <body style=\"font:16px system-ui;margin:4em auto;max-width:32em\"><p>{message}</p>"
    );
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Reads until the end of a request's head, at most `max` bytes.
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S, max: usize) -> Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&byte[..n]);
        if head.len() > max {
            bail!("too long");
        }
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// A new access token for `refresh`.
pub async fn refresh(client: &Client, refresh: &Secret) -> Result<Tokens> {
    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.expose()),
        ("client_id", client.id.as_str()),
    ];
    if !client.secret.is_empty() {
        params.push(("client_secret", client.secret.as_str()));
    }
    token_request(client.provider, &params).await
}

/// The provider took the sign-in back (revoked, expired, password
/// changed): the user must sign in again.
#[derive(Debug)]
pub struct SignInAgain(pub String);

impl std::fmt::Display for SignInAgain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sign in again: {}", self.0)
    }
}

impl std::error::Error for SignInAgain {}

async fn token_request(provider: Provider, params: &[(&str, &str)]) -> Result<Tokens> {
    let (host, path) = provider.token();
    let endpoint = Endpoint {
        host: host.into(),
        port: 443,
        security: Security::Tls,
        tls: net::tls_config(None)?,
    };
    let mut conn = net::connect(&endpoint).await?;
    let (status, body) = tokio::time::timeout(
        HTTP_TIMEOUT,
        post_form(&mut conn, host, path, &form(params)),
    )
    .await
    .map_err(|_| anyhow!("{host} didn't answer in time"))??;
    tokens_from(provider, status, &body)
}

#[derive(Deserialize)]
struct TokenAnswer {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Tokens from the token endpoint's answer, checked.
fn tokens_from(provider: Provider, status: u16, body: &[u8]) -> Result<Tokens> {
    let answer: TokenAnswer = serde_json::from_slice(body)
        .map_err(|_| anyhow!("{}'s answer didn't read (HTTP {status})", provider.name()))?;
    if let Some(error) = answer.error {
        let said = text::one_line(answer.error_description.as_deref().unwrap_or(&error), 200);
        // The refresh token is no good any more: only a new sign-in helps.
        if error == "invalid_grant" {
            return Err(anyhow!(SignInAgain(said)));
        }
        bail!("{} refused: {said}", provider.name());
    }
    if !(200..300).contains(&status) {
        bail!("{} answered HTTP {status}", provider.name());
    }
    let usable =
        |t: &str| !t.is_empty() && t.len() <= MAX_TOKEN && t.bytes().all(|b| b.is_ascii_graphic());
    if !usable(&answer.access_token) {
        bail!("{} gave no usable token", provider.name());
    }
    let refresh = answer.refresh_token.filter(|t| usable(t)).map(Secret::new);
    let lasts = Duration::from_secs(answer.expires_in.unwrap_or(3600).clamp(60, 24 * 3600));
    Ok(Tokens {
        access: Secret::new(answer.access_token),
        refresh,
        renew_at: Instant::now() + lasts.saturating_sub(EARLY),
    })
}

/// POSTs a form and reads the answer: its status and body. HTTP/1.1, the
/// connection closed after; the answer at most [`MAX_RESPONSE`].
async fn post_form<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    path: &str,
    body: &str,
) -> Result<(u16, Vec<u8>)> {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: tuimail\r\n\
         Accept: application/json\r\nContent-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    let mut answer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = match stream.read(&mut chunk).await {
            Ok(n) => n,
            // TLS closed without its close_notify, as some servers do:
            // what came is all there is.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            break;
        }
        answer.extend_from_slice(&chunk[..n]);
        if answer.len() > MAX_RESPONSE {
            bail!("{host}'s answer is too long");
        }
    }
    parse_response(&answer)
}

/// An HTTP/1.1 answer's status and body (by its length, its chunks, or
/// all that came).
fn parse_response(answer: &[u8]) -> Result<(u16, Vec<u8>)> {
    let end = answer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("the answer has no end of its head")?;
    let head = String::from_utf8_lossy(&answer[..end]);
    let rest = &answer[end + 4..];
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.strip_prefix("HTTP/1."))
        .and_then(|l| l.get(2..5))
        .and_then(|s| s.parse::<u16>().ok())
        .context("the answer has no status")?;
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    let body = if header("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        unchunk(rest)?
    } else if let Some(length) = header("content-length").and_then(|l| l.parse::<usize>().ok()) {
        rest.get(..length)
            .context("the answer is cut short")?
            .to_vec()
    } else {
        rest.to_vec()
    };
    Ok((status, body))
}

/// A chunked body put back together.
fn unchunk(mut rest: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("a chunk has no size")?;
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).context("a chunk's size didn't read")?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(rest.get(..size).context("a chunk is cut short")?);
        rest = rest.get(size + 2..).context("a chunk is cut short")?;
    }
}

/// 32 random bytes, base64url: a PKCE verifier, or a state.
fn random_token() -> Result<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("the system's random numbers failed"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// PKCE's S256 challenge for `verifier`.
fn challenge(verifier: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref())
}

/// `params` as a query string or form body.
fn form(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encoded: all but letters, digits and `-._~`.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A query's names and values, percent-decoded (`+` as a space).
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .take(50)
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Some((decode(k)?, decode(v)?))
        })
        .collect()
}

fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_browser_comes_back_to_tuimail_on_both_loopback_addresses() {
        // Ports held on [::1] by another program: never one tuimail takes
        // on 127.0.0.1 alone.
        let mut held = Vec::new();
        for _ in 0..300 {
            match TcpListener::bind(("::1", 0)).await {
                Ok(l) => held.push(l),
                Err(_) => break,
            }
        }
        for _ in 0..30 {
            let (port, listeners) = loopback().await.unwrap();
            if held.is_empty() {
                // No IPv6 loopback here.
                assert_eq!(listeners.len(), 1);
                continue;
            }
            assert_eq!(listeners.len(), 2, "{port}");
            for l in &listeners {
                assert_eq!(l.local_addr().unwrap().port(), port);
            }
        }
    }

    #[test]
    fn the_pkce_challenge_is_rfc_7636s() {
        // RFC 7636, appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let (a, b) = (random_token().unwrap(), random_token().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
    }

    #[test]
    fn forms_and_queries_go_both_ways() {
        assert_eq!(
            form(&[("scope", "a b/c"), ("x", "é&=")]),
            "scope=a%20b%2Fc&x=%C3%A9%26%3D"
        );
        assert_eq!(
            parse_query("code=M.C1%2B2&state=abc&error_description=Not+now"),
            [
                ("code".into(), "M.C1+2".into()),
                ("state".into(), "abc".into()),
                ("error_description".into(), "Not now".into())
            ]
        );
        assert!(parse_query("bad=%zz").is_empty());
    }

    #[test]
    fn providers_and_clients_come_from_the_servers() {
        let outlook = Account {
            email: "me@outlook.com".into(),
            host: "outlook.office365.com".into(),
            auth: Auth::Oauth2,
            ..Account::default()
        };
        assert_eq!(provider_for(&outlook), Some(Provider::Microsoft));
        assert!(built_in(&outlook));
        // tuimail has none of its own yet: the account's.
        assert_eq!(
            client_for(&outlook).is_some(),
            !MICROSOFT_CLIENT_ID.is_empty()
        );
        let own = Account {
            oauth_client_id: "abc".into(),
            ..outlook.clone()
        };
        assert_eq!(client_for(&own).unwrap().id, "abc");
        let gmail = Account {
            host: "imap.gmail.com".into(),
            ..own.clone()
        };
        assert_eq!(client_for(&gmail).unwrap().provider, Provider::Google);
        let other = Account {
            host: "imap.example.org".into(),
            ..own
        };
        assert_eq!(client_for(&other), None);
        let with_command = Account {
            password_command: vec!["/bin/echo".into()],
            ..outlook
        };
        assert!(!built_in(&with_command));
    }

    #[test]
    fn answers_are_read_by_length_chunks_or_to_the_end() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello and more";
        assert_eq!(parse_response(plain).unwrap(), (200, b"hello".to_vec()));
        let chunked = b"HTTP/1.1 400 Bad Request\r\nTransfer-Encoding: chunked\r\n\r\n\
            4\r\n{\"a\"\r\n3;x=y\r\n:1}\r\n0\r\n\r\n";
        assert_eq!(
            parse_response(chunked).unwrap(),
            (400, b"{\"a\":1}".to_vec())
        );
        let open = b"HTTP/1.0 200 OK\r\n\r\nall of it";
        assert_eq!(parse_response(open).unwrap(), (200, b"all of it".to_vec()));
        for bad in [
            &b"garbage"[..],
            b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nshort",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
        ] {
            assert!(parse_response(bad).is_err());
        }
    }

    #[test]
    fn tokens_are_checked_and_a_lost_sign_in_is_said() {
        let ok = br#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#;
        let tokens = tokens_from(Provider::Microsoft, 200, ok).unwrap();
        assert_eq!(tokens.access.expose(), "at");
        assert_eq!(tokens.refresh.as_ref().unwrap().expose(), "rt");
        assert!(tokens.fresh());
        let gone = br#"{"error":"invalid_grant","error_description":"AADSTS70008: expired"}"#;
        let error = tokens_from(Provider::Microsoft, 400, gone).unwrap_err();
        assert!(error.downcast_ref::<SignInAgain>().is_some(), "{error}");
        for bad in [
            &br#"{"access_token":""}"#[..],
            br#"{"access_token":"a\r\nb"}"#,
            br#"{"error":"invalid_client"}"#,
            b"<html>",
        ] {
            assert!(tokens_from(Provider::Google, 200, bad).is_err());
        }
        assert!(tokens_from(Provider::Google, 500, br#"{"access_token":"x"}"#).is_err());
    }

    #[tokio::test]
    async fn a_form_is_posted_and_its_answer_read() {
        let (mut client, mut server) = tokio::io::duplex(64 << 10);
        let serve = tokio::spawn(async move {
            // The head and the short body come together.
            let request = read_head(&mut server, 8 << 10).await.unwrap();
            server
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .await
                .unwrap();
            drop(server);
            let (head, body) = request.split_once("\r\n\r\n").unwrap();
            (head.to_string(), body.to_string())
        });
        let answer = post_form(&mut client, "login.example", "/token", "a=1")
            .await
            .unwrap();
        assert_eq!(answer, (200, b"{}".to_vec()));
        let (head, body) = serve.await.unwrap();
        assert!(
            head.starts_with("POST /token HTTP/1.1\r\nHost: login.example\r\n"),
            "{head}"
        );
        assert!(head.contains("Content-Length: 3\r\n"));
        assert_eq!(body, "a=1");
    }

    #[tokio::test]
    async fn the_browser_comes_back_with_the_code_and_only_ours_counts() {
        let client = Client {
            provider: Provider::Google,
            id: "id".into(),
            secret: String::new(),
        };
        let sign_in = SignIn::start(client, "me@gmail.com").await.unwrap();
        assert!(
            sign_in
                .url
                .starts_with("https://accounts.google.com/o/oauth2/v2/auth?client_id=id&")
        );
        assert!(sign_in.url.contains("login_hint=me%40gmail.com"));
        assert!(sign_in.url.contains("code_challenge_method=S256"));
        let port = sign_in.listeners[0].local_addr().unwrap().port();
        let state = sign_in.state.clone();
        let browser = tokio::spawn(async move {
            let get = |target: String| async move {
                let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap();
                s.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut page = String::new();
                let _ = s.read_to_string(&mut page).await;
                page
            };
            // A favicon, and a callback with another state: not taken.
            assert!(get("/favicon.ico".into()).await.starts_with("HTTP/1.1 404"));
            assert!(
                get("/?code=evil&state=nope".into())
                    .await
                    .starts_with("HTTP/1.1 404")
            );
            get(format!("/?code=the-code&state={state}")).await
        });
        let code = sign_in.code().await.unwrap();
        assert_eq!(code, "the-code");
        let page = browser.await.unwrap();
        assert!(page.contains("Signed in to tuimail"), "{page}");
    }

    /// The real token endpoints, asked with an app that doesn't exist: each
    /// must answer (over TLS, in JSON) that it doesn't know it. Needs the
    /// internet: `cargo test -- --ignored live_oauth`.
    #[tokio::test]
    #[ignore]
    async fn live_oauth_providers_answer_a_token_request() {
        for provider in [Provider::Microsoft, Provider::Google] {
            let client = Client {
                provider,
                id: "00000000-0000-0000-0000-000000000000".into(),
                secret: "none".into(),
            };
            let error = refresh(&client, &Secret::new("not-a-token"))
                .await
                .unwrap_err()
                .to_string();
            println!("{}: {error}", provider.name());
            assert!(
                error.starts_with(&format!("{} refused: ", provider.name()))
                    || error.starts_with("sign in again: "),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn a_refused_sign_in_is_said() {
        let mut stream = tokio_test_stream(b"GET /?error=access_denied&error_description=The+user+said+no&state=s HTTP/1.1\r\n\r\n");
        let answer = callback(&mut stream, "s").await.unwrap().unwrap();
        assert!(answer.unwrap_err().to_string().contains("The user said no"));
    }

    /// A stream that reads `input` and takes whatever is written.
    fn tokio_test_stream(input: &[u8]) -> tokio::io::DuplexStream {
        let (ours, mut theirs) = tokio::io::duplex(64 << 10);
        let input = input.to_vec();
        tokio::spawn(async move {
            theirs.write_all(&input).await.unwrap();
            let mut sink = Vec::new();
            let _ = theirs.read_to_end(&mut sink).await;
        });
        ours
    }
}
