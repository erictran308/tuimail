//! IMAP (RFC 3501 and the usual extensions), through async-imap: logging
//! in, listing folders, and the handful of commands tuimail sends. Every
//! command has a time limit; a connection that misses one is dropped.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_imap::types::{Fetch, Flag, NameAttribute};
use futures::TryStreamExt;

use crate::account::{Account, Auth, Security};
use crate::mail;
use crate::net::{self, Conn, Endpoint};
use crate::store::{Flags, Folder, Header, Role};
use crate::{text, utf7};

pub type Session = async_imap::Session<Conn>;

/// How long a command may take; fetching a message's text, longer.
pub const TIMEOUT: Duration = Duration::from_secs(60);
const BODY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// The biggest message downloaded to read.
pub const MAX_MESSAGE: u32 = 60 << 20;
/// The most flags one check reads back.
const MAX_FLAGS: usize = 100_000;
/// UIDs per command, so no command line gets too long.
const UIDS_PER_COMMAND: usize = 400;

/// The headers fetched for the list.
const HEADER_FIELDS: &str = "BODY.PEEK[HEADER.FIELDS (FROM TO CC SUBJECT DATE MESSAGE-ID \
                             IN-REPLY-TO REFERENCES CONTENT-TYPE)]";

/// A login the server refused: the password (or token) is wrong.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server refused the login: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// The server said no to a command (NO, BAD), or tuimail wouldn't send
/// it: the connection is still fine.
#[derive(Debug)]
pub struct Said(pub String);

impl std::fmt::Display for Said {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Said {}

async fn timed<T>(
    limit: Duration,
    what: &str,
    f: impl Future<Output = async_imap::error::Result<T>>,
) -> Result<T> {
    use async_imap::error::Error;
    match tokio::time::timeout(limit, f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e @ (Error::No(_) | Error::Bad(_) | Error::Validate(_) | Error::Append))) => {
            bail!(Said(format!(
                "{what}: {}",
                text::one_line(&e.to_string(), 300)
            )))
        }
        Ok(Err(e)) => bail!("{what}: {}", text::one_line(&e.to_string(), 300)),
        Err(_) => bail!("{what}: the server stopped answering"),
    }
}

/// What the server can do, from CAPABILITY.
#[derive(Clone, Copy, Debug, Default)]
pub struct Caps {
    pub idle: bool,
    pub move_: bool,
    pub uidplus: bool,
}

/// Connects, starts TLS (STARTTLS if asked), and logs in.
pub async fn connect(
    endpoint: &Endpoint,
    user: &str,
    secret: &str,
    auth: Auth,
) -> Result<(Session, Caps)> {
    net::check_login(user, secret)?;
    let conn = net::connect(endpoint).await?;
    let mut client = async_imap::Client::new(conn);
    let greeting = tokio::time::timeout(TIMEOUT, client.read_response())
        .await
        .map_err(|_| anyhow!("{} didn't greet", endpoint.shown()))??
        .ok_or_else(|| anyhow!("{} closed the connection", endpoint.shown()))?;
    if let async_imap::imap_proto::Response::Data {
        status: async_imap::imap_proto::Status::Bye,
        ..
    } = greeting.parsed()
    {
        bail!("{} isn't taking connections", endpoint.shown());
    }
    if endpoint.security == Security::Starttls {
        timed(
            TIMEOUT,
            "STARTTLS",
            client.run_command_and_check_ok("STARTTLS", None),
        )
        .await?;
        // Whatever the server sent before TLS is dropped with the client.
        let tcp = client.into_inner().into_plain()?;
        client = async_imap::Client::new(net::start_tls(tcp, endpoint).await?);
    }
    let login = async {
        match auth {
            Auth::Password => client.login(user, secret).await,
            Auth::Oauth2 => {
                client
                    .authenticate(
                        "XOAUTH2",
                        XOAuth2 {
                            user: user.to_string(),
                            token: secret.to_string(),
                            sent: false,
                        },
                    )
                    .await
            }
        }
    };
    let mut session = match tokio::time::timeout(TIMEOUT, login).await {
        Err(_) => bail!("{} didn't answer the login", endpoint.shown()),
        Ok(Ok(session)) => session,
        Ok(Err((
            async_imap::error::Error::No(words) | async_imap::error::Error::Bad(words),
            _,
        ))) => {
            bail!(Refused(text::one_line(&words, 200)))
        }
        Ok(Err((e, _))) => bail!("cannot log in: {}", text::one_line(&e.to_string(), 200)),
    };
    let caps = timed(TIMEOUT, "CAPABILITY", session.capabilities()).await?;
    let caps = Caps {
        idle: caps.has_str("IDLE"),
        move_: caps.has_str("MOVE"),
        uidplus: caps.has_str("UIDPLUS"),
    };
    Ok((session, caps))
}

/// XOAUTH2's one message; an error comes back as a challenge, answered
/// with nothing so the server ends the exchange.
struct XOAuth2 {
    user: String,
    token: String,
    sent: bool,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> String {
        if std::mem::replace(&mut self.sent, true) {
            return String::new();
        }
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

/// Ends the session politely.
pub async fn logout(mut session: Session) {
    let _ = tokio::time::timeout(Duration::from_secs(5), session.logout()).await;
}

/// The account's folders, with what each is for.
pub async fn folders(session: &mut Session, account: &Account) -> Result<Vec<Folder>> {
    let names: Vec<async_imap::types::Name> = timed(TIMEOUT, "LIST", async {
        session.list(Some(""), Some("*")).await?.try_collect().await
    })
    .await?;
    let mut said = Vec::new();
    let mut folders: Vec<Folder> = names
        .iter()
        .take(2000)
        .map(|name| {
            let attributes = name.attributes();
            let raw = name.name().to_string();
            let shown = utf7::decode(&raw).unwrap_or_else(|| raw.clone());
            let selectable = !attributes
                .iter()
                .any(|a| matches!(a, NameAttribute::NoSelect))
                && !attributes.iter().any(|a| {
                    matches!(a, NameAttribute::Extension(e) if e.eq_ignore_ascii_case("\\NonExistent"))
                });
            let role = role_of(&raw, &shown, name.delimiter(), attributes);
            said.push(role.map(|(_, by)| by));
            Folder {
                role: role.map(|(role, _)| role),
                shown: text::one_line(&shown, 200),
                name: raw,
                delimiter: name.delimiter().map(|d| text::one_line(d, 2)),
                selectable,
                unread: 0,
                total: 0,
            }
        })
        .collect();
    apply_overrides(&mut folders, &mut said, account);
    dedupe_roles(&mut folders, &said);
    Ok(folders)
}

/// Where a folder's role came from, the surest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RoleFrom {
    /// accounts.toml names it.
    Setting,
    /// The server marks it (SPECIAL-USE).
    Attribute,
    /// Its name is one a role usually has.
    Name,
}

/// Folders of other users, shared and public ones, by the names servers
/// give their namespaces (`shared/bob@x.y/Sent`, `Other Users/bob/Sent`):
/// never one of the account's roles, or its sent mail (Bcc and all),
/// drafts and deleted mail would go to a mailbox someone else owns.
fn other_namespace(parts: &[&str]) -> bool {
    parts.len() > 1
        && matches!(
            parts[0].to_lowercase().as_str(),
            "shared"
                | "shared folders"
                | "other users"
                | "public"
                | "public folders"
                | "#shared"
                | "#public"
                | "#users"
                | "user"
                | "users"
        )
}

/// The role of a folder: SPECIAL-USE attributes, else its name, which
/// counts only at the top or right under INBOX or [Gmail] (deeper is a
/// folder of the user's own, or someone else's).
fn role_of(
    raw: &str,
    shown: &str,
    delimiter: Option<&str>,
    attributes: &[NameAttribute],
) -> Option<(Role, RoleFrom)> {
    if raw.eq_ignore_ascii_case("INBOX") {
        return Some((Role::Inbox, RoleFrom::Attribute));
    }
    let parts: Vec<&str> = match delimiter.filter(|d| !d.is_empty()) {
        Some(d) => shown.split(d).collect(),
        None => shown.split(['/', '.']).collect(),
    };
    if other_namespace(&parts) {
        return None;
    }
    for a in attributes {
        let role = match a {
            NameAttribute::Sent => Role::Sent,
            NameAttribute::Drafts => Role::Drafts,
            NameAttribute::Trash => Role::Trash,
            NameAttribute::Junk => Role::Junk,
            NameAttribute::Archive => Role::Archive,
            NameAttribute::All => Role::All,
            NameAttribute::Flagged => Role::Flagged,
            _ => continue,
        };
        return Some((role, RoleFrom::Attribute));
    }
    // The last part of the name, for "INBOX.Sent" and "[Gmail]/Spam".
    let personal = match parts.as_slice() {
        [_] => true,
        [parent, _] => matches!(
            parent.to_lowercase().as_str(),
            "inbox" | "[gmail]" | "[google mail]"
        ),
        _ => false,
    };
    if !personal {
        return None;
    }
    let leaf = parts.last().unwrap_or(&shown).to_lowercase();
    let role = match leaf.as_str() {
        "sent" | "sent items" | "sent messages" | "sent mail" | "gesendet"
        | "gesendete elemente" | "envoyés" | "enviados" => Role::Sent,
        "drafts" | "draft" | "entwürfe" | "brouillons" | "borradores" => Role::Drafts,
        "trash" | "deleted items" | "deleted messages" | "bin" | "papierkorb" | "corbeille"
        | "papelera" => Role::Trash,
        "junk" | "spam" | "junk e-mail" | "junk email" | "bulk mail" => Role::Junk,
        "archive" | "archives" | "archiv" => Role::Archive,
        _ => return None,
    };
    Some((role, RoleFrom::Name))
}

/// Folders named in accounts.toml take their role from there.
fn apply_overrides(folders: &mut [Folder], said: &mut [Option<RoleFrom>], account: &Account) {
    let overrides = [
        (&account.sent_folder, Role::Sent),
        (&account.drafts_folder, Role::Drafts),
        (&account.trash_folder, Role::Trash),
        (&account.archive_folder, Role::Archive),
        (&account.junk_folder, Role::Junk),
    ];
    for (name, role) in overrides {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if folders.iter().any(|f| f.shown == name || f.name == name) {
            for (f, said) in folders.iter_mut().zip(said.iter_mut()) {
                if f.role == Some(role) {
                    f.role = None;
                }
                if f.shown == name || f.name == name {
                    f.role = Some(role);
                    *said = Some(RoleFrom::Setting);
                }
            }
        }
    }
}

/// One folder per role: the surest (setting, then the server's attribute,
/// then the name), the first listed of equals.
fn dedupe_roles(folders: &mut [Folder], said: &[Option<RoleFrom>]) {
    let mut best: std::collections::HashMap<Role, (RoleFrom, usize)> = Default::default();
    for (i, f) in folders.iter().enumerate() {
        if let (Some(role), Some(by)) = (f.role, said.get(i).copied().flatten()) {
            let entry = best.entry(role).or_insert((by, i));
            if by < entry.0 {
                *entry = (by, i);
            }
        }
    }
    for (i, f) in folders.iter_mut().enumerate() {
        if let Some(role) = f.role
            && best.get(&role).is_none_or(|&(_, j)| j != i)
        {
            f.role = None;
        }
    }
}

/// A folder's unread and total counts, from STATUS.
pub async fn status(session: &mut Session, folder: &str) -> Result<(u32, u32)> {
    let mailbox = timed(
        TIMEOUT,
        "STATUS",
        session.status(folder, "(MESSAGES UNSEEN)"),
    )
    .await?;
    Ok((mailbox.unseen.unwrap_or(0), mailbox.exists))
}

/// What SELECT says about a folder.
#[derive(Clone, Copy, Debug)]
pub struct Selected {
    pub uidvalidity: u32,
    pub exists: u32,
}

pub async fn select(session: &mut Session, folder: &str) -> Result<Selected> {
    drain(session);
    let mailbox = timed(TIMEOUT, "SELECT", session.select(folder)).await?;
    Ok(Selected {
        uidvalidity: mailbox.uid_validity.unwrap_or(0),
        exists: mailbox.exists,
    })
}

/// Drops what the server said that no command asked for: kept, it would
/// stay in memory as long as the session (up to 100 of any size).
fn drain(session: &mut Session) {
    while session.unsolicited_responses.try_recv().is_ok() {}
}

/// A FETCH's answers, at most `max` of them and `max_bytes` of message
/// data: a server can't make tuimail hold more than it asked for. Past
/// either, the connection is dropped (an error that isn't [`Said`]), since
/// what's still coming would be read as the next command's answer.
async fn collect(
    what: &str,
    timeout: Duration,
    stream: impl futures::Future<
        Output = async_imap::error::Result<
            impl futures::Stream<Item = async_imap::error::Result<Fetch>>,
        >,
    >,
    max: usize,
    max_bytes: usize,
) -> Result<Vec<Fetch>> {
    timed(timeout, what, async {
        let stream = stream.await?;
        futures::pin_mut!(stream);
        let mut out = Vec::new();
        let mut bytes = 0usize;
        while let Some(fetch) = stream.try_next().await? {
            bytes += fetch.body().map_or(0, <[u8]>::len) + fetch.header().map_or(0, <[u8]>::len);
            out.push(fetch);
            if out.len() > max || bytes > max_bytes {
                return Ok(Err(anyhow!("the server sent more than was asked for")));
            }
        }
        Ok(Ok(out))
    })
    .await?
}

/// The UIDs `criteria` finds, in order.
pub async fn search(session: &mut Session, criteria: &str) -> Result<Vec<u32>> {
    let found = timed(TIMEOUT, "SEARCH", session.uid_search(criteria)).await?;
    let mut uids: Vec<u32> = found.into_iter().collect();
    uids.sort_unstable();
    Ok(uids)
}

/// UIDs written as IMAP sets, `1:5,7,9:12`, a command's worth each.
pub fn uid_sets(uids: &[u32]) -> Vec<String> {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted
        .chunks(UIDS_PER_COMMAND)
        .map(|chunk| {
            let mut out = String::new();
            let mut i = 0;
            while i < chunk.len() {
                let start = chunk[i];
                let mut end = start;
                while i + 1 < chunk.len() && chunk[i + 1] == end + 1 {
                    i += 1;
                    end = chunk[i];
                }
                if !out.is_empty() {
                    out.push(',');
                }
                if start == end {
                    out.push_str(&start.to_string());
                } else {
                    out.push_str(&format!("{start}:{end}"));
                }
                i += 1;
            }
            out
        })
        .collect()
}

/// Whether a message is marked deleted: it's as good as gone, and isn't
/// listed.
fn deleted(fetch: &Fetch) -> bool {
    fetch.flags().any(|f| matches!(f, Flag::Deleted))
}

fn flags_of(fetch: &Fetch) -> Flags {
    let mut flags = Flags::default();
    for flag in fetch.flags() {
        match flag {
            Flag::Seen => flags.seen = true,
            Flag::Flagged => flags.flagged = true,
            Flag::Answered => flags.answered = true,
            Flag::Draft => flags.draft = true,
            _ => {}
        }
    }
    flags
}

/// The list's headers for these messages.
pub async fn headers(session: &mut Session, uids: &[u32]) -> Result<Vec<Header>> {
    let mut out = Vec::new();
    for set in uid_sets(uids) {
        let query = format!("(UID FLAGS RFC822.SIZE INTERNALDATE {HEADER_FIELDS})");
        let asked = set_uids(&set);
        let fetched = collect(
            "FETCH",
            TIMEOUT,
            session.uid_fetch(&set, &query),
            asked.len() + 16,
            MAX_MESSAGE as usize,
        )
        .await?;
        for fetch in &fetched {
            // Only what was asked for: a FETCH the server adds unasked
            // isn't a header to keep.
            let Some(uid) = fetch
                .uid
                .filter(|uid| !deleted(fetch) && asked.contains(uid))
            else {
                continue;
            };
            let mut summary = mail::summarize(fetch.header().unwrap_or(b""));
            if summary.date == 0 {
                summary.date = fetch.internal_date().map(|d| d.timestamp()).unwrap_or(0);
            }
            out.push(Header {
                uid,
                summary,
                flags: flags_of(fetch),
                size: fetch.size.unwrap_or(0),
            });
        }
    }
    Ok(out)
}

/// The flags of every message with a UID in `low..=high`.
pub async fn flags(session: &mut Session, low: u32, high: u32) -> Result<Vec<(u32, Flags)>> {
    if low == 0 || high < low {
        return Ok(Vec::new());
    }
    let range = (high - low) as usize + 1;
    let fetched = collect(
        "FETCH",
        TIMEOUT,
        session.uid_fetch(format!("{low}:{high}"), "(UID FLAGS)"),
        range.min(MAX_FLAGS) + 16,
        MAX_MESSAGE as usize,
    )
    .await?;
    Ok(fetched
        .iter()
        .filter(|f| !deleted(f))
        .filter_map(|f| Some((f.uid?, flags_of(f))))
        .filter(|(uid, _)| (low..=high).contains(uid))
        .collect())
}

/// A whole message, without marking it read; `None` if it's gone.
pub async fn body(session: &mut Session, uid: u32) -> Result<Option<Vec<u8>>> {
    let size = collect(
        "FETCH",
        TIMEOUT,
        session.uid_fetch(uid.to_string(), "(UID RFC822.SIZE)"),
        16,
        1 << 20,
    )
    .await?;
    let Some(size) = size.iter().find(|f| f.uid == Some(uid)).map(|f| f.size) else {
        return Ok(None);
    };
    if size.unwrap_or(0) > MAX_MESSAGE {
        bail!(Said(format!(
            "the message is {}, too big to download",
            mail::size(size.unwrap_or(0) as usize)
        )));
    }
    // The size the server gave is only its word: the bytes that come are
    // held to the cap too.
    let fetched = collect(
        "FETCH",
        BODY_TIMEOUT,
        session.uid_fetch(uid.to_string(), "(UID BODY.PEEK[])"),
        16,
        MAX_MESSAGE as usize + (1 << 20),
    )
    .await?;
    let body = fetched
        .iter()
        .find(|f| f.uid == Some(uid))
        .and_then(|f| f.body());
    if body.is_some_and(|b| b.len() > MAX_MESSAGE as usize) {
        bail!(Said("the message is too big to download".into()));
    }
    Ok(body.map(<[u8]>::to_vec))
}

/// Which flag a key changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagKind {
    Seen,
    Flagged,
    Answered,
}

impl FlagKind {
    fn imap(self) -> &'static str {
        match self {
            FlagKind::Seen => "\\Seen",
            FlagKind::Flagged => "\\Flagged",
            FlagKind::Answered => "\\Answered",
        }
    }
}

pub async fn set_flag(session: &mut Session, uids: &[u32], flag: FlagKind, on: bool) -> Result<()> {
    let sign = if on { '+' } else { '-' };
    for set in uid_sets(uids) {
        let query = format!("{sign}FLAGS.SILENT ({})", flag.imap());
        timed(TIMEOUT, "STORE", async {
            session
                .uid_store(&set, &query)
                .await?
                .try_collect::<Vec<_>>()
                .await
        })
        .await?;
    }
    Ok(())
}

/// Moves messages to `to`: MOVE where the server has it, else a copy and
/// a delete.
pub async fn move_to(session: &mut Session, caps: Caps, uids: &[u32], to: &str) -> Result<()> {
    for set in uid_sets(uids) {
        if caps.move_ {
            timed(TIMEOUT, "MOVE", session.uid_mv(&set, to)).await?;
        } else {
            timed(TIMEOUT, "COPY", session.uid_copy(&set, to)).await?;
            expunge(session, caps, &set, &set_uids(&set)).await?;
        }
    }
    Ok(())
}

/// Deletes messages for good.
/// Deletes for good; `false` if some stay marked deleted on the server
/// (without UIDPLUS, while another program's deletes are pending there).
pub async fn delete(session: &mut Session, caps: Caps, uids: &[u32]) -> Result<bool> {
    let ours: std::collections::HashSet<u32> = uids.iter().copied().collect();
    let mut all = true;
    for set in uid_sets(uids) {
        all &= expunge(session, caps, &set, &ours).await?;
    }
    Ok(all)
}

async fn expunge(
    session: &mut Session,
    caps: Caps,
    set: &str,
    ours: &std::collections::HashSet<u32>,
) -> Result<bool> {
    timed(TIMEOUT, "STORE", async {
        session
            .uid_store(set, "+FLAGS.SILENT (\\Deleted)")
            .await?
            .try_collect::<Vec<_>>()
            .await
    })
    .await?;
    if caps.uidplus {
        timed(TIMEOUT, "EXPUNGE", async {
            session
                .uid_expunge(set)
                .await?
                .try_collect::<Vec<_>>()
                .await
        })
        .await?;
    } else {
        // A plain EXPUNGE removes every message marked deleted, also ones
        // another program only marked: it's sent only if all are ours
        // (every delete tuimail still has pending in the folder).
        let marked = search(session, "DELETED").await?;
        if !marked.iter().all(|uid| ours.contains(uid)) {
            return Ok(false);
        }
        timed(TIMEOUT, "EXPUNGE", async {
            session.expunge().await?.try_collect::<Vec<_>>().await
        })
        .await?;
    }
    Ok(true)
}

/// The UIDs in a set written by [`uid_sets`].
fn set_uids(set: &str) -> std::collections::HashSet<u32> {
    let mut out = std::collections::HashSet::new();
    for part in set.split(',') {
        match part.split_once(':') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    out.extend(a.min(b)..=a.max(b));
                }
            }
            None => out.extend(part.parse::<u32>()),
        }
    }
    out
}

/// Adds a message to `folder` (Sent, Drafts).
pub async fn append(session: &mut Session, folder: &str, raw: &[u8], flags: Flags) -> Result<()> {
    let mut list = Vec::new();
    if flags.seen {
        list.push("\\Seen");
    }
    if flags.draft {
        list.push("\\Draft");
    }
    let flags = format!("({})", list.join(" "));
    timed(
        BODY_TIMEOUT,
        "APPEND",
        session.append(folder, Some(&flags), None, raw),
    )
    .await
}

/// Makes a folder (Sent or Drafts, where the server has none).
pub async fn create(session: &mut Session, folder: &str) -> Result<()> {
    timed(TIMEOUT, "CREATE", session.create(folder)).await
}

/// SEARCH criteria for messages whose headers or text have every word of
/// `query`. Words are quoted; CHARSET UTF-8 is asked for when needed.
pub fn search_criteria(query: &str) -> Option<String> {
    let words: Vec<String> = query
        .split_whitespace()
        .take(8)
        .map(|w| w.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|w| !w.is_empty())
        .map(|w| format!("TEXT \"{}\"", w.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    if words.is_empty() {
        return None;
    }
    let charset = if query.is_ascii() {
        ""
    } else {
        "CHARSET UTF-8 "
    };
    Some(format!("{charset}{}", words.join(" ")))
}

/// Waits on a connection of its own for the inbox to change (IDLE), and
/// returns when it does or after `limit`: `Ok(true)` if it changed.
pub async fn idle(session: Session, limit: Duration) -> Result<(Session, bool)> {
    let mut handle = session.idle();
    tokio::time::timeout(TIMEOUT, handle.init())
        .await
        .map_err(|_| anyhow!("IDLE: the server stopped answering"))?
        .context("IDLE")?;
    let changed = {
        let (wait, _stop) = handle.wait_with_timeout(limit);
        matches!(
            wait.await.context("IDLE")?,
            async_imap::extensions::idle::IdleResponse::NewData(_)
        )
    };
    let session = tokio::time::timeout(TIMEOUT, handle.done())
        .await
        .map_err(|_| anyhow!("IDLE: the server stopped answering"))?
        .context("IDLE")?;
    Ok((session, changed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-connection IMAP server on 127.0.0.1 that answers each
    /// command after LOGIN with `answer(tag, command)`.
    async fn scripted(answer: impl Fn(&str, &str) -> Vec<u8> + Send + 'static) -> (Session, Caps) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            write.write_all(b"* OK ready\r\n").await.unwrap();
            while let Ok(Some(line)) = lines.next_line().await {
                let (tag, command) = line.split_once(' ').unwrap_or((&line, ""));
                let reply = if command.starts_with("LOGIN") {
                    format!("{tag} OK done\r\n").into_bytes()
                } else if command.starts_with("CAPABILITY") {
                    format!("* CAPABILITY IMAP4rev1\r\n{tag} OK done\r\n").into_bytes()
                } else {
                    answer(tag, command)
                };
                if write.write_all(&reply).await.is_err() {
                    break;
                }
            }
        });
        let endpoint = Endpoint {
            host: "127.0.0.1".into(),
            port,
            security: Security::None,
            tls: net::tls_config(None).unwrap(),
        };
        connect(&endpoint, "me", "pw", Auth::Password)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_message_bigger_than_the_cap_is_refused_whatever_size_the_server_gives() {
        let (mut session, _) = scripted(|tag, command| {
            if command.contains("BODY.PEEK") {
                let big = MAX_MESSAGE as usize + (2 << 20);
                let mut out = format!("* 1 FETCH (UID 1 BODY[] {{{big}}}\r\n").into_bytes();
                out.resize(out.len() + big, b'a');
                out.extend(format!(")\r\n{tag} OK done\r\n").as_bytes());
                out
            } else {
                // No RFC822.SIZE: the size can't be checked beforehand.
                format!("* 1 FETCH (UID 1)\r\n{tag} OK done\r\n").into_bytes()
            }
        })
        .await;
        assert!(body(&mut session, 1).await.is_err());
    }

    #[tokio::test]
    async fn headers_keep_only_what_was_asked_for() {
        let (mut session, _) = scripted(|tag, _| {
            let mut out = String::new();
            for uid in [1, 2, 99] {
                out.push_str(&format!("* {uid} FETCH (UID {uid} FLAGS ())\r\n"));
            }
            out.push_str(&format!("{tag} OK done\r\n"));
            out.into_bytes()
        })
        .await;
        let got: Vec<u32> = headers(&mut session, &[1, 2])
            .await
            .unwrap()
            .iter()
            .map(|h| h.uid)
            .collect();
        assert_eq!(got, [1, 2]);
        // Many more answers than asked for: the connection is given up.
        let (mut session, _) = scripted(|tag, _| {
            let mut out = String::new();
            for uid in 1..=100 {
                out.push_str(&format!("* {uid} FETCH (UID {uid} FLAGS ())\r\n"));
            }
            out.push_str(&format!("{tag} OK done\r\n"));
            out.into_bytes()
        })
        .await;
        assert!(headers(&mut session, &[1]).await.is_err());
    }

    #[test]
    fn uids_are_written_as_ranges() {
        assert_eq!(uid_sets(&[5, 1, 2, 3, 7, 9, 10, 2]), ["1:3,5,7,9:10"]);
        let back: std::collections::BTreeSet<u32> = set_uids("1:3,5,7,9:10").into_iter().collect();
        assert_eq!(back.into_iter().collect::<Vec<_>>(), [1, 2, 3, 5, 7, 9, 10]);
        assert_eq!(uid_sets(&[]), Vec::<String>::new());
        let many: Vec<u32> = (1..=1000).step_by(2).collect();
        assert_eq!(uid_sets(&many).len(), 2);
    }

    #[test]
    fn folder_roles_come_from_attributes_then_names() {
        let role = |raw: &str, shown: &str, attributes: &[NameAttribute]| {
            role_of(raw, shown, Some("/"), attributes).map(|(role, _)| role)
        };
        assert_eq!(role("INBOX", "INBOX", &[]), Some(Role::Inbox));
        assert_eq!(role("Inbox", "Inbox", &[]), Some(Role::Inbox));
        assert_eq!(
            role(
                "[Gmail]/Sent Mail",
                "[Gmail]/Sent Mail",
                &[NameAttribute::Sent]
            ),
            Some(Role::Sent)
        );
        assert_eq!(
            role_of("INBOX.Trash", "INBOX.Trash", Some("."), &[]).map(|r| r.0),
            Some(Role::Trash)
        );
        assert_eq!(role("Entw&APw-rfe", "Entwürfe", &[]), Some(Role::Drafts));
        assert_eq!(role("Projects", "Projects", &[]), None);
        let account = Account {
            trash_folder: "Bin2".into(),
            ..Account::default()
        };
        let folder = |name: &str, role| Folder {
            name: name.into(),
            shown: name.into(),
            delimiter: None,
            role,
            selectable: true,
            unread: 0,
            total: 0,
        };
        let mut folders = vec![folder("Trash", Some(Role::Trash)), folder("Bin2", None)];
        let mut said = vec![Some(RoleFrom::Name), None];
        apply_overrides(&mut folders, &mut said, &account);
        assert_eq!(folders[0].role, None);
        assert_eq!(folders[1].role, Some(Role::Trash));
        let mut twice = vec![
            folder("Junk", Some(Role::Junk)),
            folder("Spam", Some(Role::Junk)),
        ];
        dedupe_roles(&mut twice, &[Some(RoleFrom::Name), Some(RoleFrom::Name)]);
        assert_eq!((twice[0].role, twice[1].role), (Some(Role::Junk), None));
    }

    #[test]
    fn other_peoples_folders_take_no_role() {
        let role = |name: &str| role_of(name, name, Some("/"), &[]).map(|r| r.0);
        for name in [
            "shared/bob@example.org/Archive",
            "shared/bob@example.org/Sent",
            "Other Users/bob/Trash",
            "Public/Drafts",
            "Projects/Sent",
        ] {
            assert_eq!(role(name), None, "{name}");
        }
        // Even marked, a shared folder isn't the account's.
        let marked = role_of(
            "Shared Folders/team/Sent",
            "Shared Folders/team/Sent",
            Some("/"),
            &[NameAttribute::Sent],
        );
        assert_eq!(marked, None);
        // The server's mark wins over a name guessed earlier in the list.
        let folder = |name: &str, role| Folder {
            name: name.into(),
            shown: name.into(),
            delimiter: Some("/".into()),
            role,
            selectable: true,
            unread: 0,
            total: 0,
        };
        let mut folders = vec![
            folder("Sent", Some(Role::Sent)),
            folder("Sent Items", Some(Role::Sent)),
        ];
        dedupe_roles(
            &mut folders,
            &[Some(RoleFrom::Name), Some(RoleFrom::Attribute)],
        );
        assert_eq!((folders[0].role, folders[1].role), (None, Some(Role::Sent)));
    }

    #[test]
    fn search_words_are_quoted_and_cannot_add_commands() {
        assert_eq!(
            search_criteria("invoice march").unwrap(),
            "TEXT \"invoice\" TEXT \"march\""
        );
        assert_eq!(search_criteria("a\"b\\c").unwrap(), "TEXT \"a\\\"b\\\\c\"");
        assert_eq!(
            search_criteria("Jürgen").unwrap(),
            "CHARSET UTF-8 TEXT \"Jürgen\""
        );
        assert!(
            !search_criteria("x\r\nA1 DELETE INBOX")
                .unwrap()
                .contains('\n')
        );
        assert_eq!(search_criteria("   "), None);
    }
}
