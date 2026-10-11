//! Sending mail: an SMTP submission client (RFC 5321, STARTTLS from RFC
//! 3207, AUTH from RFC 4954), its own and small like the POP3 one, so it
//! shares tuimail's TLS settings and limits.
//!
//! It greets the server as `[127.0.0.1]`, not this computer's name, which
//! would otherwise end up in the Received header of every message sent.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::account::{self, Auth, Security};
use crate::net::{self, Conn, Endpoint};
use crate::text;

const TIMEOUT: Duration = Duration::from_secs(60);
/// How long the server may take to accept the message once it's sent.
const DATA_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_LINE: usize = 8 * 1024;
/// The most lines one reply may have.
const MAX_REPLY_LINES: usize = 200;

pub struct Smtp<S> {
    stream: BufReader<S>,
    /// The EHLO keywords, upper case.
    extensions: Vec<String>,
}

/// A login the server refused.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server refused the login: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// A reply: its code and its lines' text.
struct Reply {
    code: u16,
    text: String,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Smtp<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream: BufReader::new(stream),
            extensions: Vec::new(),
        }
    }

    async fn reply_within(&mut self, timeout: Duration) -> Result<Reply> {
        let mut text = Vec::new();
        for _ in 0..MAX_REPLY_LINES {
            let line = tokio::time::timeout(timeout, net::read_line(&mut self.stream, MAX_LINE))
                .await
                .map_err(|_| anyhow!("the SMTP server stopped answering"))??;
            let line = String::from_utf8_lossy(&line).into_owned();
            let code = line
                .get(..3)
                .and_then(|c| c.parse::<u16>().ok())
                .filter(|c| (200..600).contains(c))
                .ok_or_else(|| anyhow!("the server isn't speaking SMTP"))?;
            let more = line.as_bytes().get(3) == Some(&b'-');
            text.push(line.get(4..).unwrap_or("").to_string());
            if !more {
                return Ok(Reply {
                    code,
                    text: text.join("\n"),
                });
            }
        }
        bail!("the SMTP server's reply is too long")
    }

    async fn reply(&mut self) -> Result<Reply> {
        self.reply_within(TIMEOUT).await
    }

    async fn send(&mut self, line: &str) -> Result<()> {
        if line.contains(['\r', '\n']) {
            bail!("an SMTP command can't have a line break");
        }
        let stream = self.stream.get_mut();
        tokio::time::timeout(TIMEOUT, async {
            stream.write_all(line.as_bytes()).await?;
            stream.write_all(b"\r\n").await?;
            stream.flush().await
        })
        .await
        .map_err(|_| anyhow!("the SMTP server stopped reading"))??;
        Ok(())
    }

    /// Sends `line`; its reply must have a code in `ok`.
    async fn command(&mut self, line: &str, ok: &[u16], what: &str) -> Result<Reply> {
        self.send(line).await?;
        let reply = self.reply().await?;
        if !ok.contains(&reply.code) {
            bail!(
                "{what}: the server said {} {}",
                reply.code,
                text::one_line(&reply.text, 200)
            );
        }
        Ok(reply)
    }

    pub async fn greeting(&mut self) -> Result<()> {
        let reply = self.reply().await?;
        if reply.code != 220 {
            bail!(
                "the server isn't taking mail: {}",
                text::one_line(&reply.text, 200)
            );
        }
        Ok(())
    }

    pub async fn ehlo(&mut self) -> Result<()> {
        let reply = self.command("EHLO [127.0.0.1]", &[250], "EHLO").await?;
        self.extensions = reply
            .text
            .lines()
            .skip(1)
            .map(|l| l.trim().to_ascii_uppercase())
            .collect();
        Ok(())
    }

    fn has(&self, extension: &str) -> bool {
        self.extensions
            .iter()
            .any(|e| e == extension || e.starts_with(&format!("{extension} ")))
    }

    fn auth_mechanism(&self, mechanism: &str) -> bool {
        self.extensions.iter().any(|e| {
            e.strip_prefix("AUTH")
                .is_some_and(|m| m.split([' ', '=']).any(|m| m == mechanism))
        })
    }

    async fn starttls(&mut self) -> Result<()> {
        if !self.has("STARTTLS") {
            bail!("the server can't encrypt the connection (no STARTTLS)");
        }
        self.command("STARTTLS", &[220], "STARTTLS").await?;
        if !self.stream.buffer().is_empty() {
            bail!("the server sent more than it should before TLS started");
        }
        Ok(())
    }

    pub fn into_inner(self) -> S {
        self.stream.into_inner()
    }

    pub async fn login(&mut self, user: &str, secret: &str, auth: Auth) -> Result<()> {
        crate::net::check_login(user, secret)?;
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let refused = |reply: Reply| anyhow!(Refused(text::one_line(&reply.text, 200)));
        match auth {
            Auth::Oauth2 => {
                let token = format!("user={user}\x01auth=Bearer {secret}\x01\x01");
                self.send(&format!("AUTH XOAUTH2 {}", b64(&token))).await?;
                let mut reply = self.reply().await?;
                if reply.code == 334 {
                    // The server's error, as a challenge: an empty line
                    // ends the exchange.
                    self.send("").await?;
                    reply = self.reply().await?;
                }
                if reply.code != 235 {
                    return Err(refused(reply));
                }
            }
            Auth::Password if self.auth_mechanism("PLAIN") || !self.auth_mechanism("LOGIN") => {
                self.send(&format!(
                    "AUTH PLAIN {}",
                    b64(&format!("\0{user}\0{secret}"))
                ))
                .await?;
                let reply = self.reply().await?;
                if reply.code != 235 {
                    return Err(refused(reply));
                }
            }
            Auth::Password => {
                self.command("AUTH LOGIN", &[334], "AUTH LOGIN").await?;
                self.send(&b64(user)).await?;
                let reply = self.reply().await?;
                if reply.code != 334 {
                    return Err(refused(reply));
                }
                self.send(&b64(secret)).await?;
                let reply = self.reply().await?;
                if reply.code != 235 {
                    return Err(refused(reply));
                }
            }
        }
        Ok(())
    }

    /// Sends `message` (CRLF lines) from `from` to `to`.
    pub async fn send_mail(&mut self, from: &str, to: &[String], message: &[u8]) -> Result<()> {
        for addr in std::iter::once(from).chain(to.iter().map(String::as_str)) {
            if !account::valid_address(addr) {
                bail!("{} isn't an email address", text::one_line(addr, 80));
            }
        }
        if to.is_empty() {
            bail!("the message has no recipients");
        }
        let utf8 = !from.is_ascii() || to.iter().any(|a| !a.is_ascii());
        if utf8 && !self.has("SMTPUTF8") {
            bail!("the server can't take addresses with non-ASCII characters");
        }
        let mut mail_from = format!("MAIL FROM:<{from}>");
        if self.has("SIZE") {
            mail_from.push_str(&format!(" SIZE={}", message.len()));
        }
        if self.has("8BITMIME") && !message.is_ascii() {
            mail_from.push_str(" BODY=8BITMIME");
        }
        if utf8 {
            mail_from.push_str(" SMTPUTF8");
        }
        self.command(&mail_from, &[250], "the sender").await?;
        for rcpt in to {
            self.command(
                &format!("RCPT TO:<{rcpt}>"),
                &[250, 251],
                &text::one_line(rcpt, 80),
            )
            .await?;
        }
        self.command("DATA", &[354], "DATA").await?;
        let data = dot_stuffed(message);
        let stream = self.stream.get_mut();
        tokio::time::timeout(DATA_TIMEOUT, async {
            stream.write_all(&data).await?;
            stream.flush().await
        })
        .await
        .map_err(|_| anyhow!("sending took too long"))??;
        let reply = self.reply_within(DATA_TIMEOUT).await?;
        if reply.code != 250 {
            bail!(
                "the server didn't take the message: {} {}",
                reply.code,
                text::one_line(&reply.text, 200)
            );
        }
        Ok(())
    }

    pub async fn quit(mut self) -> Result<()> {
        self.send("QUIT").await?;
        let _ = self.reply().await;
        Ok(())
    }
}

/// `message` as DATA sends it: every line ending in CRLF (a bare CR or LF
/// made one), a `.` added before lines that start with one, and the `.`
/// line that ends it.
pub fn dot_stuffed(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + message.len() / 50 + 5);
    let mut line_start = true;
    let mut i = 0;
    while i < message.len() {
        let b = message[i];
        if line_start && b == b'.' {
            out.push(b'.');
        }
        match b {
            b'\r' => {
                out.extend_from_slice(b"\r\n");
                if message.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                line_start = true;
            }
            b'\n' => {
                out.extend_from_slice(b"\r\n");
                line_start = true;
            }
            _ => {
                out.push(b);
                line_start = false;
            }
        }
        i += 1;
    }
    if !line_start {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

/// Connects, encrypts and logs in.
async fn open(endpoint: &Endpoint, user: &str, secret: &str, auth: Auth) -> Result<Smtp<Conn>> {
    let conn = net::connect(endpoint).await?;
    let mut smtp = Smtp::new(conn);
    smtp.greeting().await?;
    smtp.ehlo().await?;
    if endpoint.security == Security::Starttls {
        smtp.starttls().await?;
        let tcp = smtp.into_inner().into_plain()?;
        smtp = Smtp::<Conn>::new(net::start_tls(tcp, endpoint).await?);
        smtp.ehlo().await?;
    }
    // A server that asks for no login gets no password: a relay on this
    // computer has no use for it.
    if smtp.extensions.iter().any(|e| e.starts_with("AUTH")) {
        smtp.login(user, secret, auth)
            .await
            .context("cannot log in to send")?;
    }
    Ok(smtp)
}

/// Logs in and out, to check an account's settings.
pub async fn check(endpoint: &Endpoint, user: &str, secret: &str, auth: Auth) -> Result<()> {
    open(endpoint, user, secret, auth).await?.quit().await
}

/// Connects, encrypts, logs in, sends `message` from `from` to every
/// address in `to`, and says goodbye.
pub async fn send(
    endpoint: &Endpoint,
    user: &str,
    secret: &str,
    auth: Auth,
    from: &str,
    to: &[String],
    message: &[u8],
) -> Result<()> {
    let mut smtp = open(endpoint, user, secret, auth).await?;
    smtp.send_mail(from, to, message).await?;
    // The message is the server's now: a goodbye that fails changes
    // nothing, and saying "not sent" would have it sent twice.
    let _ = smtp.quit().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pop3::tests::scripted;

    #[test]
    fn dots_are_stuffed_and_lines_end_in_crlf() {
        assert_eq!(dot_stuffed(b"a\r\n.b\r\n"), b"a\r\n..b\r\n.\r\n");
        assert_eq!(dot_stuffed(b".\nx"), b"..\r\nx\r\n.\r\n");
        assert_eq!(dot_stuffed(b"a\rb"), b"a\r\nb\r\n.\r\n");
        assert_eq!(dot_stuffed(b""), b".\r\n");
    }

    #[tokio::test]
    async fn a_message_is_sent_to_every_recipient() {
        let stream = scripted(vec![
            ("", "220 mx ESMTP\r\n"),
            (
                "EHLO [127.0.0.1]",
                "250-mx hello\r\n250-SIZE 1000000\r\n250-AUTH LOGIN PLAIN\r\n250 8BITMIME\r\n",
            ),
            ("AUTH PLAIN AG1lAHNlY3JldA==", "235 ok\r\n"),
            ("MAIL FROM:<me@example.org> SIZE=17", "250 ok\r\n"),
            ("RCPT TO:<a@example.com>", "250 ok\r\n"),
            ("RCPT TO:<b@example.com>", "251 forwarded\r\n"),
            ("DATA", "354 go\r\n"),
            ("Subject: hi", ""),
            ("..dot", ""),
            (".", "250 queued\r\n"),
            ("QUIT", "221 bye\r\n"),
        ]);
        let mut smtp = Smtp::new(stream);
        smtp.greeting().await.unwrap();
        smtp.ehlo().await.unwrap();
        smtp.login("me", "secret", Auth::Password).await.unwrap();
        let to = ["a@example.com".to_string(), "b@example.com".to_string()];
        smtp.send_mail("me@example.org", &to, b"Subject: hi\r\n.dot")
            .await
            .unwrap();
        smtp.quit().await.unwrap();
    }

    #[tokio::test]
    async fn auth_login_is_used_when_plain_is_not_offered() {
        let stream = scripted(vec![
            ("", "220 ready\r\n"),
            ("EHLO [127.0.0.1]", "250-hi\r\n250 AUTH LOGIN\r\n"),
            ("AUTH LOGIN", "334 VXNlcm5hbWU6\r\n"),
            ("bWU=", "334 UGFzc3dvcmQ6\r\n"),
            ("c2VjcmV0", "535 bad credentials\r\n"),
        ]);
        let mut smtp = Smtp::new(stream);
        smtp.greeting().await.unwrap();
        smtp.ehlo().await.unwrap();
        let error = smtp
            .login("me", "secret", Auth::Password)
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<Refused>().is_some(), "{error}");
    }

    #[tokio::test]
    async fn bad_addresses_and_rejected_recipients_stop_the_send() {
        let stream = scripted(vec![
            ("", "220 ready\r\n"),
            ("EHLO [127.0.0.1]", "250 hi\r\n"),
            ("MAIL FROM:<me@example.org>", "250 ok\r\n"),
            ("RCPT TO:<nobody@example.com>", "550 no such user\r\n"),
        ]);
        let mut smtp = Smtp::new(stream);
        smtp.greeting().await.unwrap();
        smtp.ehlo().await.unwrap();
        let injected = ["x@y.z>\r\nRCPT TO:<evil@example.com".to_string()];
        assert!(
            smtp.send_mail("me@example.org", &injected, b"x")
                .await
                .is_err()
        );
        let to = ["nobody@example.com".to_string()];
        let error = smtp
            .send_mail("me@example.org", &to, b"x")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("550 no such user"), "{error}");
    }
}
