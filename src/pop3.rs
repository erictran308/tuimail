//! A POP3 client (RFC 1939, STLS from RFC 2595, CAPA from RFC 2449, SASL
//! from RFC 5034): just what downloading an inbox needs. Its own, small,
//! with every line and message capped, rather than a crate's.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::account::{Auth, Security};
use crate::net::{self, Conn, Endpoint};
use crate::text;

/// How long the server may take to answer a command.
const TIMEOUT: Duration = Duration::from_secs(60);
/// The longest status line read.
const MAX_LINE: usize = 8 * 1024;
/// The longest line inside a message: base64 without line breaks can be
/// long, but not longer than this.
const MAX_DATA_LINE: usize = 4 << 20;
/// The most a list (UIDL, LIST) may hold.
const MAX_LIST: usize = 16 << 20;

pub struct Pop3<S> {
    stream: BufReader<S>,
    /// What CAPA said, upper case.
    capabilities: Vec<String>,
}

/// A login the server refused, so the password (or token) is wrong: told
/// apart from a failed connection so tuimail can ask again.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server refused the login: {}", self.0)
    }
}

impl std::error::Error for Refused {}

impl<S: AsyncRead + AsyncWrite + Unpin> Pop3<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream: BufReader::new(stream),
            capabilities: Vec::new(),
        }
    }

    /// The server's first line, which must be `+OK`.
    pub async fn greeting(&mut self) -> Result<()> {
        self.status().await.map(drop)
    }

    /// Reads a status line: its text after `+OK`, or an error with the
    /// server's words after `-ERR`.
    async fn status(&mut self) -> Result<String> {
        let line = tokio::time::timeout(TIMEOUT, net::read_line(&mut self.stream, MAX_LINE))
            .await
            .map_err(|_| anyhow!("the POP3 server stopped answering"))??;
        let line = String::from_utf8_lossy(&line);
        if let Some(rest) = line.strip_prefix("+OK") {
            Ok(rest.trim().to_string())
        } else if let Some(rest) = line.strip_prefix("-ERR") {
            bail!(ServerError(text::one_line(rest, 200)))
        } else {
            bail!("the server isn't speaking POP3")
        }
    }

    async fn send(&mut self, line: &str) -> Result<()> {
        if line.contains(['\r', '\n']) {
            bail!("a POP3 command can't have a line break");
        }
        let stream = self.stream.get_mut();
        tokio::time::timeout(TIMEOUT, async {
            stream.write_all(line.as_bytes()).await?;
            stream.write_all(b"\r\n").await?;
            stream.flush().await
        })
        .await
        .map_err(|_| anyhow!("the POP3 server stopped reading"))??;
        Ok(())
    }

    /// Sends `line` and reads its status line.
    async fn command(&mut self, line: &str) -> Result<String> {
        self.send(line).await?;
        self.status().await
    }

    /// The lines of a multi-line answer, up to the `.` that ends it, with
    /// dot-stuffing taken out, each ending in CRLF; at most `max` bytes.
    ///
    /// A message over `max` is read to its end without being kept, so the
    /// session stays in step, and is then [`TooBig`]. A line longer than
    /// [`MAX_DATA_LINE`] leaves the session out of step: [`BadMessage`],
    /// and the session must end.
    async fn data(&mut self, max: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut too_big = false;
        loop {
            let line =
                tokio::time::timeout(TIMEOUT, net::read_line(&mut self.stream, MAX_DATA_LINE))
                    .await
                    .map_err(|_| anyhow!("the POP3 server stopped sending"))?;
            let line = match line {
                Ok(line) => line,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => bail!(BadMessage),
                Err(e) => return Err(e.into()),
            };
            if line == b"." {
                if too_big {
                    bail!(TooBig);
                }
                return Ok(out);
            }
            let line = line.strip_prefix(b".").unwrap_or(&line);
            if too_big || out.len() + line.len() + 2 > max {
                too_big = true;
                out = Vec::new();
                continue;
            }
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
    }

    /// CAPA, remembered; servers that don't know it have none.
    pub async fn capa(&mut self) -> Result<()> {
        self.send("CAPA").await?;
        if self.status().await.is_err() {
            self.capabilities.clear();
            return Ok(());
        }
        let data = self.data(64 * 1024).await?;
        self.capabilities = String::from_utf8_lossy(&data)
            .lines()
            .map(|l| l.trim().to_ascii_uppercase())
            .collect();
        Ok(())
    }

    fn has(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|c| c == capability || c.starts_with(&format!("{capability} ")))
    }

    fn sasl(&self, mechanism: &str) -> bool {
        self.capabilities.iter().any(|c| {
            c.strip_prefix("SASL ")
                .is_some_and(|m| m.split_whitespace().any(|m| m == mechanism))
        })
    }

    /// Asks for TLS. The caller then starts TLS on [`Pop3::into_inner`].
    async fn stls(&mut self) -> Result<()> {
        self.command("STLS")
            .await
            .context("the server won't start TLS (STLS)")?;
        // Anything the server sent after its answer came before TLS, so
        // anyone on the way could have put it there.
        if !self.stream.buffer().is_empty() {
            bail!("the server sent more than it should before TLS started");
        }
        Ok(())
    }

    pub fn into_inner(self) -> S {
        self.stream.into_inner()
    }

    /// Logs in with a password or, for `Auth::Oauth2`, a token.
    pub async fn login(&mut self, user: &str, secret: &str, auth: Auth) -> Result<()> {
        if user.contains(['\r', '\n', '\0']) || secret.contains(['\r', '\n', '\0']) {
            bail!("the username or password has a line break in it");
        }
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let result = match auth {
            Auth::Oauth2 => {
                let token = format!("user={user}\x01auth=Bearer {secret}\x01\x01");
                self.command(&format!("AUTH XOAUTH2 {}", b64(&token))).await
            }
            Auth::Password if !self.has("USER") && self.sasl("PLAIN") => {
                self.command(&format!(
                    "AUTH PLAIN {}",
                    b64(&format!("\0{user}\0{secret}"))
                ))
                .await
            }
            Auth::Password => match self.command(&format!("USER {user}")).await {
                Ok(_) => self.command(&format!("PASS {secret}")).await,
                Err(e) => Err(e),
            },
        };
        result
            .map(drop)
            .map_err(|e| match e.downcast::<ServerError>() {
                Ok(ServerError(words)) => anyhow!(Refused(words)),
                Err(e) => e,
            })
    }

    /// Each message's number and unique id (UIDL).
    pub async fn uidl(&mut self) -> Result<Vec<(u32, String)>> {
        self.command("UIDL")
            .await
            .context("the server can't list its messages' ids (UIDL)")?;
        let data = self.data(MAX_LIST).await?;
        let mut out = Vec::new();
        for line in String::from_utf8_lossy(&data).lines() {
            let mut parts = line.split_whitespace();
            let (Some(n), Some(id)) = (parts.next(), parts.next()) else {
                continue;
            };
            if let (Ok(n), true) = (n.parse(), valid_uid(id)) {
                out.push((n, id.to_string()));
            }
        }
        Ok(out)
    }

    /// Each message's number and size.
    pub async fn list(&mut self) -> Result<Vec<(u32, u64)>> {
        self.command("LIST").await?;
        let data = self.data(MAX_LIST).await?;
        Ok(String::from_utf8_lossy(&data)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
            })
            .collect())
    }

    /// Message `n`, whole, at most `max` bytes.
    pub async fn retr(&mut self, n: u32, max: usize) -> Result<Vec<u8>> {
        self.command(&format!("RETR {n}")).await?;
        self.data(max).await
    }

    /// Marks message `n` to be deleted when the session ends ([`Pop3::quit`]).
    pub async fn dele(&mut self, n: u32) -> Result<()> {
        self.command(&format!("DELE {n}")).await.map(drop)
    }

    /// Ends the session: messages marked with DELE go now.
    pub async fn quit(mut self) -> Result<()> {
        self.command("QUIT").await.map(drop)
    }
}

/// A `-ERR` answer, with the server's words.
#[derive(Debug)]
pub struct ServerError(pub String);

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the POP3 server said: {}", self.0)
    }
}

impl std::error::Error for ServerError {}

/// A message bigger than tuimail takes.
#[derive(Debug)]
pub struct TooBig;

impl std::fmt::Display for TooBig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the message is too big")
    }
}

impl std::error::Error for TooBig {}

/// A message with a line too long to read: the session can't go on.
#[derive(Debug)]
pub struct BadMessage;

impl std::fmt::Display for BadMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a message has a line too long to read")
    }
}

impl std::error::Error for BadMessage {}

/// A unique id as RFC 1939 allows: 1 to 70 printable characters.
fn valid_uid(id: &str) -> bool {
    (1..=70).contains(&id.len()) && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Connects, starts TLS (STLS if asked), reads the capabilities and logs in.
pub async fn connect(
    endpoint: &Endpoint,
    user: &str,
    secret: &str,
    auth: Auth,
) -> Result<Pop3<Conn>> {
    let conn = net::connect(endpoint).await?;
    let mut pop = Pop3::new(conn);
    pop.greeting().await?;
    if endpoint.security == Security::Starttls {
        pop.stls().await?;
        let tcp = pop.into_inner().into_plain()?;
        pop = Pop3::new(net::start_tls(tcp, endpoint).await?);
    }
    pop.capa().await?;
    pop.login(user, secret, auth).await?;
    Ok(pop)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, DuplexStream};

    /// A server that answers each line it reads with the next of
    /// `script`'s answers, checking the line is what's expected.
    pub fn scripted(script: Vec<(&'static str, &'static str)>) -> DuplexStream {
        let (client, server) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = BufReader::new(read).lines();
            for (expect, answer) in script {
                if !expect.is_empty() {
                    let line = lines.next_line().await.unwrap().unwrap_or_default();
                    assert_eq!(line, expect);
                }
                write.write_all(answer.as_bytes()).await.unwrap();
            }
        });
        client
    }

    #[tokio::test]
    async fn a_mailbox_is_listed_and_downloaded_with_dots_unstuffed() {
        let stream = scripted(vec![
            ("", "+OK POP3 ready\r\n"),
            ("CAPA", "+OK\r\nUSER\r\nUIDL\r\nSASL PLAIN\r\n.\r\n"),
            ("USER me@example.org", "+OK\r\n"),
            ("PASS secret", "+OK logged in\r\n"),
            ("UIDL", "+OK\r\n1 abc\r\n2 d\x01ef\r\n3 xyz\r\n.\r\n"),
            ("LIST", "+OK\r\n1 120\r\n3 50\r\n.\r\n"),
            (
                "RETR 1",
                "+OK\r\nSubject: hi\r\n\r\n..leading dot\r\nend\r\n.\r\n",
            ),
            ("DELE 3", "+OK\r\n"),
            ("QUIT", "+OK bye\r\n"),
        ]);
        let mut pop = Pop3::new(stream);
        pop.greeting().await.unwrap();
        pop.capa().await.unwrap();
        pop.login("me@example.org", "secret", Auth::Password)
            .await
            .unwrap();
        let ids = pop.uidl().await.unwrap();
        assert_eq!(
            ids,
            [(1, "abc".into()), (3, "xyz".into())],
            "a bad id is skipped"
        );
        assert_eq!(pop.list().await.unwrap(), [(1, 120), (3, 50)]);
        let message = pop.retr(1, 1000).await.unwrap();
        assert_eq!(message, b"Subject: hi\r\n\r\n.leading dot\r\nend\r\n");
        pop.dele(3).await.unwrap();
        pop.quit().await.unwrap();
    }

    #[tokio::test]
    async fn a_refused_login_is_told_apart() {
        let stream = scripted(vec![
            ("", "+OK\r\n"),
            ("USER me", "+OK\r\n"),
            ("PASS wrong", "-ERR [AUTH] invalid\r\n"),
        ]);
        let mut pop = Pop3::new(stream);
        pop.greeting().await.unwrap();
        let error = pop.login("me", "wrong", Auth::Password).await.unwrap_err();
        assert!(error.downcast_ref::<Refused>().is_some(), "{error}");
    }

    #[tokio::test]
    async fn sasl_plain_and_xoauth2_are_used_when_needed() {
        let stream = scripted(vec![
            ("", "+OK\r\n"),
            ("CAPA", "+OK\r\nSASL PLAIN XOAUTH2\r\n.\r\n"),
            ("AUTH PLAIN AG1lAHNlY3JldA==", "+OK\r\n"),
        ]);
        let mut pop = Pop3::new(stream);
        pop.greeting().await.unwrap();
        pop.capa().await.unwrap();
        pop.login("me", "secret", Auth::Password).await.unwrap();

        let stream = scripted(vec![
            ("", "+OK\r\n"),
            (
                "AUTH XOAUTH2 dXNlcj1tZQFhdXRoPUJlYXJlciB0b2tlbgEB",
                "+OK\r\n",
            ),
        ]);
        let mut pop = Pop3::new(stream);
        pop.greeting().await.unwrap();
        pop.login("me", "token", Auth::Oauth2).await.unwrap();
    }

    #[tokio::test]
    async fn line_breaks_in_a_password_never_reach_the_server() {
        let stream = scripted(vec![("", "+OK\r\n")]);
        let mut pop = Pop3::new(stream);
        pop.greeting().await.unwrap();
        assert!(
            pop.login("me", "x\r\nDELE 1", Auth::Password)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_message_over_the_limit_is_refused() {
        let stream = scripted(vec![
            ("", "+OK\r\n"),
            ("RETR 1", "+OK\r\n0123456789\r\n0123456789\r\n.\r\n"),
            ("RETR 2", "+OK\r\nsmall\r\n.\r\n"),
        ]);
        let mut pop = Pop3::new(stream);
        pop.greeting().await.unwrap();
        let error = pop.retr(1, 15).await.unwrap_err();
        assert!(error.downcast_ref::<TooBig>().is_some());
        // Read to its end: the next message comes through whole.
        assert_eq!(pop.retr(2, 15).await.unwrap(), b"small\r\n");
    }

    #[tokio::test]
    async fn data_sent_with_the_answer_to_stls_is_refused() {
        let stream = scripted(vec![
            ("", "+OK\r\n"),
            ("STLS", "+OK begin TLS\r\n+OK injected\r\n"),
        ]);
        let mut pop = Pop3::new(stream);
        pop.greeting().await.unwrap();
        // Give the injected line time to arrive with the answer.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(pop.stls().await.is_err());
    }
}
