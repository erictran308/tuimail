//! One background task per account: it holds the connection, runs what
//! the app asks for in order, and reports back as events. The app keeps the
//! cache; a worker never touches it. For IMAP accounts whose server can, a
//! second connection waits on the inbox (IDLE) so new mail shows at once.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use rustls::ClientConfig;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::account::{Account, Protocol};
use crate::imap::{self, Caps, FlagKind, Session};
use crate::net::Endpoint;
use crate::secrets::{Credentials, NeedPassword};
use crate::store::{Flags, Folder, Header, Known};
use crate::{pop3, smtp, tags, text};

/// After a failed connection, requests fail at once for this long instead
/// of each waiting to time out.
const OFFLINE_FOR: Duration = Duration::from_secs(20);
/// IDLE is renewed this often (servers drop it after 30 minutes).
const IDLE_FOR: Duration = Duration::from_secs(25 * 60);
/// The most folders whose counts are asked for.
const MAX_STATUS: usize = 200;
/// The most new POP3 messages downloaded in one check.
const POP_BATCH: usize = 300;
/// The biggest POP3 message downloaded.
const POP_MAX: usize = 60 << 20;
/// The most bytes one check downloads before leaving the rest for the
/// next.
const POP_BYTES: usize = 256 << 20;

/// What the app asks an account's worker to do.
#[derive(Clone, Debug)]
pub enum Request {
    /// List the folders and their counts.
    Folders,
    /// Fetch what's new in a folder, and its flags since `known`.
    Sync {
        folder: String,
        known: Known,
        count: u32,
    },
    /// Fetch up to `count` messages older than UID `below`.
    Older {
        folder: String,
        below: u32,
        count: u32,
    },
    // The requests on UIDs carry `uidvalidity`, what the UIDs meant when
    // they were taken (0: not known, no check): if the server has given
    // the folder a new one since, they mean other messages, and nothing is
    // done to them.
    Body {
        folder: String,
        uid: u32,
        uidvalidity: u32,
    },
    Flag {
        folder: String,
        uids: Vec<u32>,
        flag: FlagKind,
        on: bool,
        uidvalidity: u32,
    },
    Move {
        folder: String,
        uids: Vec<u32>,
        to: String,
        uidvalidity: u32,
    },
    Delete {
        folder: String,
        uids: Vec<u32>,
        uidvalidity: u32,
    },
    /// Adds a message to a folder, made first if it isn't there. `id`
    /// comes back with [`Event::Appended`]. With `retry`, a connection
    /// lost on the way doesn't fail it: it's made once the connection is
    /// back (a copy in Sent); without, it fails (a draft, whose composer
    /// waits).
    Append {
        folder: String,
        raw: Vec<u8>,
        flags: Flags,
        id: u64,
        retry: bool,
    },
    /// The list's headers for these messages (the server has them; the
    /// cache lost them).
    Headers {
        folder: String,
        uids: Vec<u32>,
        uidvalidity: u32,
    },
    /// Moves messages back from `folder` to `to` (an undo): found by their
    /// Message-IDs, since their UIDs in `folder` aren't known.
    MoveBack {
        folder: String,
        message_ids: Vec<String>,
        to: String,
    },
    /// Puts a tag on messages (`on`) or takes it off.
    Tag {
        folder: String,
        uids: Vec<u32>,
        tag: String,
        on: bool,
        uidvalidity: u32,
    },
    Search {
        folder: String,
        query: String,
    },
    /// POP3: download new mail (ids not in `seen`) and delete `remove`
    /// from the server.
    Pop {
        seen: HashSet<String>,
        remove: Vec<String>,
        count: u32,
    },
}

impl Request {
    /// Whether it changes mail on the server: such a request isn't dropped
    /// while the connection is down, but made once it's back.
    pub fn changes(&self) -> bool {
        matches!(
            self,
            Request::Flag { .. }
                | Request::Tag { .. }
                | Request::Move { .. }
                | Request::MoveBack { .. }
                | Request::Delete { .. }
                | Request::Append { retry: true, .. }
        )
    }

    /// What it was, for an error message.
    fn what(&self) -> &'static str {
        match self {
            Request::Folders => "listing folders",
            Request::Sync { .. } => "checking for mail",
            Request::Older { .. } => "fetching older mail",
            Request::Body { .. } => "downloading the message",
            Request::Flag { .. } => "changing flags",
            Request::Tag { .. } => "tagging",
            Request::Move { .. } | Request::MoveBack { .. } => "moving mail",
            Request::Delete { .. } => "deleting mail",
            Request::Append { .. } => "saving a copy",
            Request::Headers { .. } => "checking for mail",
            Request::Search { .. } => "searching",
            Request::Pop { .. } => "checking for mail",
        }
    }
}

/// Whether the account can be reached.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Connecting,
    Online,
    Offline(String),
    /// No password is known, or the server refused it (`refused`).
    NeedPassword {
        refused: bool,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Synced {
    pub folder: String,
    pub uidvalidity: u32,
    /// The folder's UIDs changed meaning: what's cached must go.
    pub reset: bool,
    pub new: Vec<Header>,
    /// The flags of every message from `known.low` to `known.high`; cached
    /// ones not in it were deleted elsewhere.
    pub flags: Vec<(u32, Flags)>,
    /// Flags were fetched for this range.
    pub checked: Option<(u32, u32)>,
    /// What's cached now runs from `low` to `high`.
    pub low: u32,
    pub high: u32,
    pub unread: u32,
    pub total: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Popped {
    /// New messages: their POP3 id and text.
    pub new: Vec<(String, Vec<u8>)>,
    /// Ids not downloaded (too big, or older than the first check's
    /// share), so they aren't asked about again.
    pub skipped: Vec<String>,
    /// Every id the server still has.
    pub on_server: HashSet<String>,
    /// Ids deleted from the server.
    pub deleted: Vec<String>,
    /// More new mail is waiting: check again.
    pub more: bool,
}

/// What a worker tells the app.
#[derive(Clone, Debug)]
pub enum Event {
    State(State),
    Folders(Vec<Folder>),
    Synced(Synced),
    Older {
        folder: String,
        headers: Vec<Header>,
        low: u32,
    },
    Body {
        folder: String,
        uid: u32,
        result: Result<Option<Vec<u8>>, String>,
    },
    Searched {
        folder: String,
        query: String,
        headers: Vec<Header>,
    },
    /// The server did what was asked (a move).
    Done {
        folder: String,
    },
    /// Messages moved back ([`Request::MoveBack`]): `found` of the
    /// `asked` Message-IDs were in `folder`.
    MovedBack {
        folder: String,
        to: String,
        found: usize,
        asked: usize,
    },
    /// Deletes for good the server made (`all`: and expunged).
    Deleted {
        folder: String,
        uids: Vec<u32>,
        all: bool,
    },
    /// How many changes haven't reached the server yet.
    Waiting(usize),
    /// A message was added to `folder` (`error` if it wasn't).
    Appended {
        folder: String,
        id: u64,
        error: Option<String>,
    },
    /// Headers asked for with [`Request::Headers`].
    Headers {
        folder: String,
        headers: Vec<Header>,
    },
    Failed {
        what: &'static str,
        error: String,
    },
    /// IDLE saw the inbox change.
    Changed {
        folder: String,
    },
    Popped(Popped),
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// The running worker; dropping it stops it (and its IDLE connection).
pub struct Worker {
    pub tx: UnboundedSender<Request>,
    pub creds: Credentials,
    pub endpoints: Endpoints,
    task: JoinHandle<()>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Where an account's servers are.
#[derive(Clone)]
pub struct Endpoints {
    pub incoming: Endpoint,
    pub smtp: Endpoint,
}

impl Endpoints {
    pub fn of(account: &Account, tls: Arc<ClientConfig>) -> Self {
        Self {
            incoming: Endpoint {
                host: account.host.trim().to_string(),
                port: account.port(),
                security: account.security,
                tls: tls.clone(),
            },
            smtp: Endpoint {
                host: account.smtp_host.trim().to_string(),
                port: account.smtp_port(),
                security: account.smtp_security,
                tls,
            },
        }
    }
}

pub fn spawn(creds: Credentials, endpoints: Endpoints, sink: Sink) -> Worker {
    let (tx, rx) = unbounded_channel();
    let task = match creds.account().protocol {
        Protocol::Imap => tokio::spawn(run_imap(creds.clone(), endpoints.clone(), rx, sink)),
        Protocol::Pop3 => tokio::spawn(run_pop(creds.clone(), endpoints.clone(), rx, sink)),
    };
    Worker {
        tx,
        creds,
        endpoints,
        task,
    }
}

/// A failure while connecting, sorted for the app.
fn login_failed(error: &anyhow::Error) -> State {
    if error.downcast_ref::<NeedPassword>().is_some() {
        State::NeedPassword { refused: false }
    } else if error.downcast_ref::<imap::Refused>().is_some()
        || error.downcast_ref::<pop3::Refused>().is_some()
        || error.downcast_ref::<smtp::Refused>().is_some()
    {
        State::NeedPassword { refused: true }
    } else {
        State::Offline(text::one_line(&format!("{error:#}"), 300))
    }
}

/// Logs in with the account's password; a refused one is forgotten.
async fn imap_login(creds: &Credentials, endpoint: &Endpoint) -> Result<(Session, Caps)> {
    let secret = creds.get().await?;
    let account = creds.account();
    match imap::connect(endpoint, account.username(), secret.expose(), account.auth).await {
        Ok(session) => {
            let _ = creds.remember().await;
            Ok(session)
        }
        Err(e) => {
            if e.downcast_ref::<imap::Refused>().is_some() {
                creds.refused().await;
            }
            Err(e)
        }
    }
}

/// Requests waiting together, with repeats of the same check dropped
/// (the last one's `known` wins).
fn batch(first: Request, rx: &mut UnboundedReceiver<Request>) -> VecDeque<Request> {
    let mut all = vec![first];
    while let Ok(next) = rx.try_recv() {
        all.push(next);
    }
    let mut out: VecDeque<Request> = VecDeque::new();
    for request in all {
        let duplicate = out.iter().position(|r| match (r, &request) {
            (Request::Folders, Request::Folders) => true,
            (Request::Sync { folder: a, .. }, Request::Sync { folder: b, .. }) => a == b,
            (Request::Pop { .. }, Request::Pop { .. }) => true,
            _ => false,
        });
        // The later one goes last, so a change asked for in between (a
        // flag set) is made before the check reads the flags back.
        if let Some(i) = duplicate {
            out.remove(i);
        }
        out.push_back(request);
    }
    out
}

/// Whether an error left the connection unusable (as opposed to the server
/// saying no to one command).
fn broken(error: &anyhow::Error) -> bool {
    error.downcast_ref::<imap::Said>().is_none()
}

/// The IDLE task, stopped when the worker ends, however it ends.
#[derive(Default)]
struct Watcher(Option<JoinHandle<()>>);

impl Watcher {
    fn running(&self) -> bool {
        self.0.as_ref().is_some_and(|w| !w.is_finished())
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

/// While changes are held for a lost connection, it's tried again this
/// often.
const RETRY_EVERY: Duration = Duration::from_secs(30);

/// Tells the app a request failed: an Append by its id, anything else by
/// what it was.
fn report_failure(request: &Request, error: String, sink: &Sink) {
    match request {
        Request::Append { folder, id, .. } => sink(Event::Appended {
            folder: folder.clone(),
            id: *id,
            error: Some(error),
        }),
        _ => sink(Event::Failed {
            what: request.what(),
            error,
        }),
    }
}

async fn run_imap(
    creds: Credentials,
    endpoints: Endpoints,
    mut rx: UnboundedReceiver<Request>,
    sink: Sink,
) {
    let mut session: Option<(Session, Caps)> = None;
    let mut failed_at: Option<Instant> = None;
    let mut watcher = Watcher::default();
    // Changes asked for while the connection was down, in order.
    let mut held: VecDeque<Request> = VecDeque::new();
    let mut waiting = 0;
    let mut report = |n: usize, sink: &Sink| {
        if n != waiting {
            waiting = n;
            sink(Event::Waiting(n));
        }
    };
    loop {
        let fresh = if held.is_empty() {
            match rx.recv().await {
                Some(first) => batch(first, &mut rx),
                None => break,
            }
        } else {
            tokio::select! {
                next = rx.recv() => match next {
                    Some(first) => batch(first, &mut rx),
                    None => break,
                },
                _ = tokio::time::sleep(RETRY_EVERY) => VecDeque::new(),
            }
        };
        // What was held goes first, in the order it was asked for.
        let mut requests: VecDeque<Request> = std::mem::take(&mut held);
        requests.extend(fresh);
        while let Some(request) = requests.pop_front() {
            let queued = requests.iter().filter(|r| r.changes()).count();
            report(held.len() + queued + usize::from(request.changes()), &sink);
            if session.is_none() {
                let recently = failed_at.is_some_and(|at| at.elapsed() < OFFLINE_FOR);
                let retrying = !held.is_empty();
                if (recently || retrying) && !matches!(request, Request::Folders) {
                    if request.changes() {
                        held.push_back(request);
                    } else {
                        report_failure(&request, "offline".into(), &sink);
                    }
                    continue;
                }
                sink(Event::State(State::Connecting));
                match imap_login(&creds, &endpoints.incoming).await {
                    Ok(s) => {
                        let idle = s.1.idle;
                        session = Some(s);
                        failed_at = None;
                        sink(Event::State(State::Online));
                        if idle && !watcher.running() {
                            watcher.0 = Some(tokio::spawn(watch_inbox(
                                creds.clone(),
                                endpoints.incoming.clone(),
                                sink.clone(),
                            )));
                        }
                    }
                    Err(e) => {
                        failed_at = Some(Instant::now());
                        sink(Event::State(login_failed(&e)));
                        if request.changes() {
                            held.push_back(request);
                        } else {
                            report_failure(&request, "offline".into(), &sink);
                        }
                        continue;
                    }
                }
            }
            let Some((s, caps)) = session.as_mut() else {
                continue;
            };
            if let Err(e) = handle_imap(request.clone(), s, *caps, &creds, &sink).await {
                let lost = broken(&e);
                let error = text::one_line(&format!("{e:#}"), 300);
                if lost && request.changes() {
                    // Made again once the connection is back.
                    held.push_back(request);
                } else {
                    report_failure(&request, error.clone(), &sink);
                }
                if lost {
                    session = None;
                    failed_at = Some(Instant::now());
                    sink(Event::State(State::Offline(error)));
                }
            }
        }
        report(held.len(), &sink);
    }
    if let Some((s, _)) = session {
        imap::logout(s).await;
    }
}

/// SELECTs `folder` for something done to UIDs read under `uidvalidity`
/// (0: unknown). A folder the server has made again since has other
/// messages under those UIDs: that's refused, not done.
async fn select_same(s: &mut Session, folder: &str, uidvalidity: u32) -> Result<imap::Selected> {
    let selected = imap::select(s, folder).await?;
    if uidvalidity != 0 && selected.uidvalidity != uidvalidity {
        return Err(imap::Said(format!(
            "{} was made again on the server, so its messages changed: nothing was done",
            shown_folder(folder)
        ))
        .into());
    }
    Ok(selected)
}

/// A folder's name as an error message shows it.
fn shown_folder(folder: &str) -> String {
    text::one_line(
        &crate::utf7::decode(folder).unwrap_or_else(|| folder.to_string()),
        60,
    )
}

async fn handle_imap(
    request: Request,
    s: &mut Session,
    caps: Caps,
    creds: &Credentials,
    sink: &Sink,
) -> Result<()> {
    match request {
        Request::Folders => {
            let mut folders = imap::folders(s, creds.account()).await?;
            for f in folders.iter_mut().filter(|f| f.selectable).take(MAX_STATUS) {
                if let Ok((unread, total)) = imap::status(s, &f.name).await {
                    f.unread = unread;
                    f.total = total;
                }
            }
            sink(Event::Folders(folders));
        }
        Request::Sync {
            folder,
            known,
            count,
        } => {
            let synced = sync(s, &folder, known, count).await?;
            sink(Event::Synced(synced));
        }
        Request::Older {
            folder,
            below,
            count,
        } => {
            imap::select(s, &folder).await?;
            let mut uids = if below > 1 {
                imap::search(s, &format!("UID 1:{}", below - 1)).await?
            } else {
                Vec::new()
            };
            uids.retain(|&u| u < below);
            let keep = uids.len().saturating_sub(count as usize);
            let uids = &uids[keep..];
            let headers = imap::headers(s, uids).await?;
            let low = uids.first().copied().unwrap_or(1);
            sink(Event::Older {
                folder,
                headers,
                low,
            });
        }
        Request::Body {
            folder,
            uid,
            uidvalidity,
        } => {
            let result = match select_same(s, &folder, uidvalidity).await {
                Ok(_) => imap::body(s, uid).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(raw) => sink(Event::Body {
                    folder,
                    uid,
                    result: Ok(raw),
                }),
                Err(e) if broken(&e) => return Err(e),
                Err(e) => sink(Event::Body {
                    folder,
                    uid,
                    result: Err(text::one_line(&format!("{e:#}"), 300)),
                }),
            }
        }
        Request::Flag {
            folder,
            uids,
            flag,
            on,
            uidvalidity,
        } => {
            select_same(s, &folder, uidvalidity).await?;
            imap::set_flag(s, &uids, flag, on).await?;
        }
        Request::Tag {
            folder,
            uids,
            tag,
            on,
            uidvalidity,
        } => {
            let selected = select_same(s, &folder, uidvalidity).await?;
            if on {
                let keyword = tags::keyword(&tag).unwrap_or_default();
                // A keyword the folder can't keep would be gone at the
                // next connection, with no word said.
                if !selected.keeps(&keyword) {
                    return Err(imap::Said(format!(
                        "{} doesn't keep tags on the server",
                        shown_folder(&folder)
                    ))
                    .into());
                }
                imap::set_keywords(s, &uids, &[keyword], true).await?;
            } else {
                imap::set_keywords(s, &uids, &tags::keywords(&tag), false).await?;
            }
        }
        Request::Move {
            folder,
            uids,
            to,
            uidvalidity,
        } => {
            select_same(s, &folder, uidvalidity).await?;
            imap::move_to(s, caps, &uids, &to).await?;
            sink(Event::Done { folder: to });
        }
        Request::MoveBack {
            folder,
            message_ids,
            to,
        } => {
            imap::select(s, &folder).await?;
            let (uids, found) = imap::find_message_ids(s, &message_ids).await?;
            if !uids.is_empty() {
                imap::move_to(s, caps, &uids, &to).await?;
            }
            sink(Event::MovedBack {
                folder,
                to,
                found,
                asked: message_ids.len(),
            });
        }
        Request::Delete {
            folder,
            uids,
            uidvalidity,
        } => {
            select_same(s, &folder, uidvalidity).await?;
            let all = imap::delete(s, caps, &uids).await?;
            sink(Event::Deleted { folder, uids, all });
        }
        Request::Append {
            folder,
            raw,
            flags,
            id,
            ..
        } => {
            if let Err(e) = imap::append(s, &folder, &raw, flags.clone()).await {
                // A folder that isn't there yet (Sent, Drafts) is made.
                if broken(&e) {
                    return Err(e);
                }
                imap::create(s, &folder).await?;
                imap::append(s, &folder, &raw, flags).await?;
            }
            sink(Event::Appended {
                folder,
                id,
                error: None,
            });
        }
        Request::Headers {
            folder,
            uids,
            uidvalidity,
        } => {
            select_same(s, &folder, uidvalidity).await?;
            let headers = imap::headers(s, &uids).await?;
            sink(Event::Headers { folder, headers });
        }
        Request::Search { folder, query } => {
            let Some(criteria) = imap::search_criteria(&query) else {
                return Ok(());
            };
            imap::select(s, &folder).await?;
            let uids = imap::search(s, &criteria).await?;
            // The newest matches.
            let keep = uids.len().saturating_sub(200);
            let headers = imap::headers(s, &uids[keep..]).await?;
            sink(Event::Searched {
                folder,
                query,
                headers,
            });
        }
        Request::Pop { .. } => {}
    }
    Ok(())
}

/// What's new in `folder` since `known`, and the flags of what's cached.
async fn sync(s: &mut Session, folder: &str, known: Known, count: u32) -> Result<Synced> {
    let selected = imap::select(s, folder).await?;
    let reset = known.uidvalidity != 0 && known.uidvalidity != selected.uidvalidity;
    let known = if reset || known.uidvalidity == 0 {
        Known::default()
    } else {
        known
    };
    let count = count.max(1) as usize;
    let mut new_uids: Vec<u32> = if selected.exists == 0 {
        Vec::new()
    } else if known.high == 0 {
        // The newest `count`, by their place in the folder.
        let start = selected.exists.saturating_sub(count as u32 - 1).max(1);
        let fetched: Vec<async_imap::types::Fetch> = {
            use futures::TryStreamExt;
            tokio::time::timeout(imap::TIMEOUT, async {
                s.fetch(format!("{start}:{}", selected.exists), "(UID)")
                    .await?
                    .try_collect()
                    .await
            })
            .await
            .map_err(|_| anyhow!("FETCH: the server stopped answering"))?
            .map_err(|e| anyhow!("FETCH: {e}"))?
        };
        let mut uids: Vec<u32> = fetched.iter().filter_map(|f| f.uid).collect();
        uids.sort_unstable();
        uids
    } else {
        // Past the last UID there are none (`high + 1` would wrap to 0, and
        // `0:*` is every message).
        match known.high.checked_add(1) {
            Some(next) => {
                let mut uids = imap::search(s, &format!("UID {next}:*")).await?;
                uids.retain(|&u| u > known.high);
                uids
            }
            None => Vec::new(),
        }
    };
    let gap = new_uids.len() > count;
    if gap {
        new_uids.drain(..new_uids.len() - count);
    }
    let new = imap::headers(s, &new_uids).await?;
    let (flags, checked) = if known.high > 0 && !gap {
        (
            imap::flags(s, known.low, known.high).await?,
            Some((known.low, known.high)),
        )
    } else {
        (Vec::new(), None)
    };
    let low = if known.high == 0 || gap {
        new_uids.first().copied().unwrap_or(0)
    } else {
        known.low
    };
    let high = new_uids.last().copied().unwrap_or(0).max(known.high);
    let unread = imap::search(s, "UNSEEN")
        .await
        .map(|u| u.len())
        .unwrap_or(0) as u32;
    Ok(Synced {
        folder: folder.to_string(),
        uidvalidity: selected.uidvalidity,
        reset: reset || gap,
        new,
        flags,
        checked,
        low,
        high,
        unread,
        total: selected.exists,
    })
}

/// Waits on the inbox with IDLE on a connection of its own, reporting each
/// change; reconnects after a failure, waiting longer each time.
async fn watch_inbox(creds: Credentials, endpoint: Endpoint, sink: Sink) {
    let mut wait = Duration::from_secs(15);
    loop {
        let result: Result<()> = async {
            let (mut session, _) = imap_login(&creds, &endpoint).await?;
            tokio::time::timeout(imap::TIMEOUT, session.examine("INBOX"))
                .await
                .map_err(|_| anyhow!("EXAMINE: the server stopped answering"))??;
            wait = Duration::from_secs(15);
            loop {
                let (back, changed) = imap::idle(session, IDLE_FOR).await?;
                session = back;
                if changed {
                    sink(Event::Changed {
                        folder: "INBOX".into(),
                    });
                    // Changes come in bursts (a message, then its flags).
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        .await;
        if let Err(e) = result
            && matches!(login_failed(&e), State::NeedPassword { .. })
        {
            // The worker asks for the password; this waits for it.
            wait = Duration::from_secs(60);
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(10 * 60));
    }
}

async fn run_pop(
    creds: Credentials,
    endpoints: Endpoints,
    mut rx: UnboundedReceiver<Request>,
    sink: Sink,
) {
    while let Some(first) = rx.recv().await {
        for request in batch(first, &mut rx) {
            let Request::Pop {
                seen,
                remove,
                count,
            } = request
            else {
                continue;
            };
            sink(Event::State(State::Connecting));
            match pop_check(&creds, &endpoints.incoming, &seen, &remove, count).await {
                Ok(popped) => {
                    sink(Event::State(State::Online));
                    sink(Event::Popped(popped));
                }
                Err(e) => {
                    if e.downcast_ref::<pop3::Refused>().is_some() {
                        creds.refused().await;
                    }
                    let state = login_failed(&e);
                    // The state says it all when the password is wanted.
                    let said = matches!(state, State::NeedPassword { .. });
                    sink(Event::State(state));
                    if !said {
                        sink(Event::Failed {
                            what: "checking for mail",
                            error: text::one_line(&format!("{e:#}"), 300),
                        });
                    }
                }
            }
        }
    }
}

/// One POP3 session: new mail downloaded (newest first, at most
/// [`POP_BATCH`]), `remove` deleted from the server.
async fn pop_check(
    creds: &Credentials,
    endpoint: &Endpoint,
    seen: &HashSet<String>,
    remove: &[String],
    count: u32,
) -> Result<Popped> {
    let secret = creds.get().await?;
    let account = creds.account();
    let mut pop =
        pop3::connect(endpoint, account.username(), secret.expose(), account.auth).await?;
    let _ = creds.remember().await;
    let ids = pop.uidl().await?;
    let sizes: HashMap<u32, u64> = pop.list().await?.into_iter().collect();
    let mut popped = Popped {
        on_server: ids.iter().map(|(_, id)| id.clone()).collect(),
        ..Popped::default()
    };
    let mut new: Vec<&(u32, String)> = ids.iter().filter(|(_, id)| !seen.contains(id)).collect();
    // Newest (highest number) first.
    new.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    if seen.is_empty() && new.len() > count as usize {
        // A first check takes the newest share; the rest stay on the server.
        popped
            .skipped
            .extend(new.drain(count as usize..).map(|(_, id)| id.clone()));
    }
    if new.len() > POP_BATCH {
        new.truncate(POP_BATCH);
        popped.more = true;
    }
    let mut bytes = 0;
    for (n, id) in new {
        if sizes.get(n).copied().unwrap_or(0) > POP_MAX as u64 {
            popped.skipped.push(id.clone());
            continue;
        }
        if bytes > POP_BYTES {
            // The rest at the next check, so memory stays bounded.
            popped.more = true;
            break;
        }
        match pop.retr(*n, POP_MAX).await {
            Ok(raw) => {
                bytes += raw.len();
                popped.new.push((id.clone(), raw));
            }
            Err(e) if e.downcast_ref::<pop3::TooBig>().is_some() => popped.skipped.push(id.clone()),
            Err(e) if e.downcast_ref::<pop3::BadMessage>().is_some() => {
                // The session is out of step: what came so far is kept,
                // this message skipped for good, the rest next time. No
                // QUIT, so nothing is deleted.
                popped.skipped.push(id.clone());
                popped.more = true;
                return Ok(popped);
            }
            Err(e) => return Err(e),
        }
    }
    let mut deleting = Vec::new();
    for id in remove {
        if let Some((n, _)) = ids.iter().find(|(_, i)| i == id) {
            pop.dele(*n).await?;
            deleting.push(id.clone());
        } else {
            // Already gone from the server.
            popped.deleted.push(id.clone());
        }
    }
    // Deletions happen only once QUIT is answered.
    pop.quit().await?;
    popped.deleted.extend(deleting);
    Ok(popped)
}

/// Sends `raw` through the account's SMTP server.
pub async fn send(
    creds: Credentials,
    endpoint: Endpoint,
    from: String,
    to: Vec<String>,
    raw: Vec<u8>,
) -> Result<()> {
    let secret = creds.get().await?;
    let account = creds.account();
    let result = smtp::send(
        &endpoint,
        account.smtp_username(),
        secret.expose(),
        account.auth,
        &from,
        &to,
        &raw,
    )
    .await;
    if let Err(e) = &result
        && e.chain()
            .any(|c| c.downcast_ref::<smtp::Refused>().is_some())
        && account.smtp_username() == account.username()
    {
        creds.refused().await;
    }
    result
}

/// Whether an error means the password is needed or was refused.
pub fn needs_password(error: &anyhow::Error) -> bool {
    error.chain().any(|c| {
        c.downcast_ref::<NeedPassword>().is_some()
            || c.downcast_ref::<smtp::Refused>().is_some()
            || c.downcast_ref::<imap::Refused>().is_some()
            || c.downcast_ref::<pop3::Refused>().is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_checks_waiting_together_run_once() {
        let (tx, mut rx) = unbounded_channel();
        let sync = |folder: &str, high| Request::Sync {
            folder: folder.into(),
            known: Known {
                uidvalidity: 1,
                low: 1,
                high,
            },
            count: 10,
        };
        tx.send(sync("INBOX", 5)).unwrap();
        tx.send(Request::Folders).unwrap();
        tx.send(sync("Sent", 1)).unwrap();
        tx.send(Request::Folders).unwrap();
        let first = sync("INBOX", 3);
        let all = batch(first, &mut rx);
        assert_eq!(all.len(), 3, "{all:?}");
        assert!(matches!(&all[2], Request::Folders));
        assert!(
            matches!(&all[0], Request::Sync { known, .. } if known.high == 5),
            "the latest"
        );
    }

    #[test]
    fn login_failures_are_told_apart() {
        let need = anyhow!(NeedPassword);
        assert_eq!(login_failed(&need), State::NeedPassword { refused: false });
        let refused = anyhow!(imap::Refused("no".into()));
        assert_eq!(
            login_failed(&refused),
            State::NeedPassword { refused: true }
        );
        assert!(matches!(
            login_failed(&anyhow!("timeout")),
            State::Offline(_)
        ));
        assert!(needs_password(
            &anyhow!(smtp::Refused("x".into())).context("cannot log in")
        ));
    }
}
