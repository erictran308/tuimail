//! Tests against a real mail server: GreenMail in Docker, which speaks
//! IMAP, POP3 and SMTP over TLS with a certificate of its own (trusted here
//! the way an account's `certificate` is). Ignored unless asked for:
//!
//! ```sh
//! docker run -d --name tuimail-greenmail -e GREENMAIL_OPTS='-Dgreenmail.setup.test.all
//!   -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.users=alice:secret@example.com,bob:secret@example.com
//!   -Dgreenmail.users.login=email' -p 3465:3465 -p 3993:3993 -p 3995:3995 greenmail/standalone:2.1.3
//! echo | openssl s_client -connect localhost:3993 2>/dev/null | openssl x509 > /tmp/greenmail.pem
//! TM_LIVE_CERT=/tmp/greenmail.pem cargo test -- --ignored live
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::account::{Account, Protocol, Security};
use crate::compose::{self, Draft};
use crate::imap::FlagKind;
use crate::secrets::{Credentials, Secret};
use crate::store::{Flags, Known, Role};
use crate::worker::{self, Endpoints, Event, Request, Worker};
use crate::{imap, net};

fn cert() -> PathBuf {
    PathBuf::from(std::env::var("TM_LIVE_CERT").expect("TM_LIVE_CERT: GreenMail's certificate"))
}

fn account(user: &str, protocol: Protocol) -> Account {
    Account {
        name: user.into(),
        email: format!("{user}@example.com"),
        display_name: format!("{user} Tester"),
        protocol,
        host: "localhost".into(),
        port: Some(if protocol == Protocol::Imap {
            3993
        } else {
            3995
        }),
        security: Security::Tls,
        smtp_host: "localhost".into(),
        smtp_port: Some(3465),
        smtp_security: Security::Tls,
        certificate: Some(cert()),
        ..Account::default()
    }
}

async fn start(account: Account) -> (Worker, UnboundedReceiver<Event>) {
    let tls = net::tls_config(account.certificate.as_deref()).unwrap();
    let endpoints = Endpoints::of(&account, tls);
    let creds = Credentials::new(Arc::new(account), false);
    creds.set(Secret::new("secret")).await;
    let (tx, rx) = unbounded_channel();
    let tx = Mutex::new(tx);
    let sink: worker::Sink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    (worker::spawn(creds, endpoints, sink), rx)
}

/// The next event `pick` takes, skipping others, within 20 s.
async fn next<T>(rx: &mut UnboundedReceiver<Event>, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let event = rx.recv().await.expect("the worker ended");
            if let Event::Failed { what, error } = &event {
                panic!("{what}: {error}");
            }
            if let Some(found) = pick(event) {
                return found;
            }
        }
    })
    .await
    .expect("no such event in time")
}

async fn send(from: &Worker, to: &str, subject: &str) {
    let draft = Draft {
        to: to.into(),
        subject: subject.into(),
        body: format!("Body of {subject}.\nhttps://example.com/{subject}\n"),
        ..Draft::default()
    };
    let account = from.creds.account().clone();
    let id = compose::new_message_id(&account.email).unwrap();
    let built =
        compose::build(&draft, &account, &id, chrono::Utc::now().timestamp(), false).unwrap();
    worker::send(
        from.creds.clone(),
        from.endpoints.smtp.clone(),
        built.from,
        built.rcpts,
        built.raw,
    )
    .await
    .unwrap();
}

fn unique(what: &str) -> String {
    format!(
        "{what}-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    )
}

#[tokio::test]
#[ignore]
async fn live_imap_sync_read_flag_move_search_and_idle() {
    let (alice, _alice_events) = start(account("alice", Protocol::Imap)).await;
    let (bob, mut events) = start(account("bob", Protocol::Imap)).await;

    // A folder to move into, made directly.
    let endpoint = bob.endpoints.incoming.clone();
    let (mut session, _) = imap::connect(
        &endpoint,
        "bob@example.com",
        "secret",
        crate::account::Auth::Password,
    )
    .await
    .unwrap();
    let _ = session.create("Archive").await;
    imap::logout(session).await;

    let subject = unique("hello");
    send(&alice, "bob@example.com", &subject).await;

    bob.tx.send(Request::Folders).unwrap();
    let folders = next(&mut events, |e| match e {
        Event::Folders(f) => Some(f),
        _ => None,
    })
    .await;
    let inbox = folders
        .iter()
        .find(|f| f.role == Some(Role::Inbox))
        .expect("an inbox");
    assert!(inbox.total >= 1);
    assert!(folders.iter().any(|f| f.name == "Archive"));

    bob.tx
        .send(Request::Sync {
            folder: "INBOX".into(),
            known: Known::default(),
            count: 100,
        })
        .unwrap();
    let synced = next(&mut events, |e| match e {
        Event::Synced(s) => Some(s),
        _ => None,
    })
    .await;
    let header = synced
        .new
        .iter()
        .find(|h| h.summary.subject == subject)
        .expect("the message sent")
        .clone();
    assert_eq!(header.summary.from.addr, "alice@example.com");
    assert_eq!(header.summary.from.name, "alice Tester");
    assert!(!header.flags.seen);
    assert!(synced.uidvalidity > 0 && synced.high >= header.uid);

    bob.tx
        .send(Request::Body {
            uidvalidity: synced.uidvalidity,
            folder: "INBOX".into(),
            uid: header.uid,
        })
        .unwrap();
    let raw = next(&mut events, |e| match e {
        Event::Body { uid, result, .. } if uid == header.uid => Some(result),
        _ => None,
    })
    .await
    .unwrap()
    .expect("the message is there");
    let parsed = crate::mail::parse(&raw).unwrap();
    assert!(
        parsed
            .text
            .unwrap()
            .contains(&format!("Body of {subject}."))
    );

    // Flags set, then seen by the next sync.
    bob.tx
        .send(Request::Flag {
            uidvalidity: synced.uidvalidity,
            folder: "INBOX".into(),
            uids: vec![header.uid],
            flag: FlagKind::Flagged,
            on: true,
        })
        .unwrap();
    bob.tx
        .send(Request::Sync {
            folder: "INBOX".into(),
            known: Known {
                uidvalidity: synced.uidvalidity,
                low: synced.low,
                high: synced.high,
            },
            count: 100,
        })
        .unwrap();
    let again = next(&mut events, |e| match e {
        Event::Synced(s) => Some(s),
        _ => None,
    })
    .await;
    let flags = &again
        .flags
        .iter()
        .find(|(u, _)| *u == header.uid)
        .unwrap()
        .1;
    assert!(flags.flagged && !flags.seen, "PEEK didn't mark it read");

    // Tags go on as keywords (one spelled in UTF-7), are read back as
    // tags, are found by `tag:`, and come off.
    let sync = |bob: &Worker| {
        bob.tx
            .send(Request::Sync {
                folder: "INBOX".into(),
                known: Known {
                    uidvalidity: synced.uidvalidity,
                    low: synced.low,
                    high: synced.high,
                },
                count: 100,
            })
            .unwrap();
    };
    let tag = |bob: &Worker, tag: &str, on: bool| {
        bob.tx
            .send(Request::Tag {
                uidvalidity: synced.uidvalidity,
                folder: "INBOX".into(),
                uids: vec![header.uid],
                tag: tag.into(),
                on,
            })
            .unwrap();
    };
    tag(&bob, "work", true);
    tag(&bob, "công_việc", true);
    sync(&bob);
    let tagged = next(&mut events, |e| match e {
        Event::Synced(s) => Some(s),
        Event::Failed { error, .. } => panic!("{error}"),
        _ => None,
    })
    .await;
    let tags = |s: &worker::Synced| {
        s.flags
            .iter()
            .find(|(u, _)| *u == header.uid)
            .unwrap()
            .1
            .tags
            .clone()
    };
    assert_eq!(tags(&tagged), ["công_việc", "work"]);
    bob.tx
        .send(Request::Search {
            folder: "INBOX".into(),
            query: "tag:work".into(),
        })
        .unwrap();
    let found = next(&mut events, |e| match e {
        Event::Searched { headers, .. } => Some(headers),
        _ => None,
    })
    .await;
    assert_eq!(
        found.iter().map(|h| h.uid).collect::<Vec<_>>(),
        [header.uid]
    );
    tag(&bob, "work", false);
    sync(&bob);
    let untagged = next(&mut events, |e| match e {
        Event::Synced(s) => Some(s),
        _ => None,
    })
    .await;
    assert_eq!(tags(&untagged), ["công_việc"]);

    bob.tx
        .send(Request::Search {
            folder: "INBOX".into(),
            query: subject.clone(),
        })
        .unwrap();
    let found = next(&mut events, |e| match e {
        Event::Searched { headers, .. } => Some(headers),
        _ => None,
    })
    .await;
    assert_eq!(found.len(), 1);

    // New mail shows through IDLE without asking.
    let pushed = unique("pushed");
    send(&alice, "bob@example.com", &pushed).await;
    next(&mut events, |e| match e {
        Event::Changed { folder } if folder == "INBOX" => Some(()),
        _ => None,
    })
    .await;

    bob.tx
        .send(Request::Move {
            uidvalidity: synced.uidvalidity,
            folder: "INBOX".into(),
            uids: vec![header.uid],
            to: "Archive".into(),
        })
        .unwrap();
    next(&mut events, |e| match e {
        Event::Done { folder } if folder == "Archive" => Some(()),
        _ => None,
    })
    .await;
    // Undone: found in Archive by its Message-ID (one that isn't there
    // is said to be missing), moved back, and moved to Archive again.
    let ids = vec![
        header.summary.message_id.clone(),
        "nowhere@example.com".into(),
    ];
    assert!(!ids[0].is_empty());
    for (from, to, found) in [("Archive", "INBOX", 1), ("INBOX", "Archive", 1)] {
        bob.tx
            .send(Request::MoveBack {
                folder: from.into(),
                message_ids: ids.clone(),
                to: to.into(),
            })
            .unwrap();
        let back = next(&mut events, |e| match e {
            Event::MovedBack { found, asked, .. } => Some((found, asked)),
            Event::Failed { error, .. } => panic!("{error}"),
            _ => None,
        })
        .await;
        assert_eq!(back, (found, 2), "from {from}");
    }
    bob.tx
        .send(Request::Append {
            folder: "Archive".into(),
            raw: raw.clone(),
            flags: Flags {
                seen: true,
                ..Flags::default()
            },
            id: 7,
            retry: false,
        })
        .unwrap();
    next(&mut events, |e| match e {
        Event::Appended {
            folder,
            id: 7,
            error: None,
        } if folder == "Archive" => Some(()),
        _ => None,
    })
    .await;
    // A folder that isn't there is made for the message.
    bob.tx
        .send(Request::Append {
            folder: "Made by tuimail".into(),
            raw: raw.clone(),
            flags: Flags::default(),
            id: 8,
            retry: false,
        })
        .unwrap();
    let error = next(&mut events, |e| match e {
        Event::Appended { id: 8, error, .. } => Some(error),
        _ => None,
    })
    .await;
    assert_eq!(error, None, "the folder was made");
    bob.tx
        .send(Request::Sync {
            folder: "Archive".into(),
            known: Known::default(),
            count: 100,
        })
        .unwrap();
    let archive = next(&mut events, |e| match e {
        Event::Synced(s) if s.folder == "Archive" => Some(s),
        _ => None,
    })
    .await;
    let copies: Vec<_> = archive
        .new
        .iter()
        .filter(|h| h.summary.subject == subject)
        .collect();
    assert_eq!(copies.len(), 2, "moved, and appended");
    assert!(copies.iter().any(|h| h.flags.seen));
    bob.tx
        .send(Request::Delete {
            uidvalidity: archive.uidvalidity,
            folder: "Archive".into(),
            uids: copies.iter().map(|h| h.uid).collect(),
        })
        .unwrap();
    bob.tx
        .send(Request::Sync {
            folder: "Archive".into(),
            known: Known {
                uidvalidity: archive.uidvalidity,
                low: archive.low,
                high: archive.high,
            },
            count: 100,
        })
        .unwrap();
    let after = next(&mut events, |e| match e {
        Event::Synced(s) if s.folder == "Archive" => Some(s),
        _ => None,
    })
    .await;
    assert!(
        copies
            .iter()
            .all(|c| after.flags.iter().all(|(u, _)| *u != c.uid)),
        "deleted for good"
    );
}

#[tokio::test]
#[ignore]
async fn live_pop3_downloads_new_mail_once_and_deletes_when_asked() {
    let (bob, _) = start(account("bob", Protocol::Imap)).await;
    let (alice, mut events) = start(account("alice", Protocol::Pop3)).await;
    let subject = unique("pop");
    send(&bob, "alice@example.com", &subject).await;

    alice
        .tx
        .send(Request::Pop {
            seen: Default::default(),
            remove: Vec::new(),
            count: 500,
        })
        .unwrap();
    let popped = next(&mut events, |e| match e {
        Event::Popped(p) => Some(p),
        _ => None,
    })
    .await;
    let (id, raw) = popped
        .new
        .iter()
        .find(|(_, raw)| crate::mail::summarize(raw).subject == subject)
        .expect("the new message")
        .clone();
    assert!(raw.len() > 100);

    let seen: std::collections::HashSet<String> = popped
        .new
        .iter()
        .map(|(i, _)| i.clone())
        .chain(popped.skipped.iter().cloned())
        .collect();
    alice
        .tx
        .send(Request::Pop {
            seen: seen.clone(),
            remove: vec![id.clone()],
            count: 500,
        })
        .unwrap();
    let second = next(&mut events, |e| match e {
        Event::Popped(p) => Some(p),
        _ => None,
    })
    .await;
    assert!(second.new.is_empty(), "nothing downloaded twice");
    assert_eq!(second.deleted, std::slice::from_ref(&id));

    alice
        .tx
        .send(Request::Pop {
            seen,
            remove: Vec::new(),
            count: 500,
        })
        .unwrap();
    let third = next(&mut events, |e| match e {
        Event::Popped(p) => Some(p),
        _ => None,
    })
    .await;
    assert!(!third.on_server.contains(&id), "gone from the server");
}

#[tokio::test]
#[ignore]
async fn live_a_wrong_password_asks_again() {
    let (bob, mut events) = start(account("bob", Protocol::Imap)).await;
    bob.creds.set(Secret::new("wrong")).await;
    bob.tx.send(Request::Folders).unwrap();
    let state = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(Event::State(s @ worker::State::NeedPassword { .. })) = events.recv().await
            {
                return s;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(state, worker::State::NeedPassword { refused: true });
}

/// A TCP proxy to GreenMail's IMAP port that can be cut (every connection
/// closed, new ones refused) and restored.
struct Proxy {
    port: u16,
    up: Arc<std::sync::atomic::AtomicBool>,
    connections: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Proxy {
    async fn start(to: u16) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let up = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let connections: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();
        let (flag, list) = (up.clone(), connections.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    return;
                };
                if !flag.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                let task = tokio::spawn(async move {
                    if let Ok(mut outbound) =
                        tokio::net::TcpStream::connect(("127.0.0.1", to)).await
                    {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                list.lock().unwrap().push(task);
            }
        });
        Self {
            port,
            up,
            connections,
        }
    }

    fn cut(&self) {
        self.up.store(false, std::sync::atomic::Ordering::SeqCst);
        for task in self.connections.lock().unwrap().drain(..) {
            task.abort();
        }
    }

    fn restore(&self) {
        self.up.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
#[ignore]
async fn live_a_change_made_offline_reaches_the_server_once_back() {
    let (alice, _alice_events) = start(account("alice", Protocol::Imap)).await;
    let proxy = Proxy::start(3993).await;
    let mut through = account("bob", Protocol::Imap);
    through.port = Some(proxy.port);
    let (bob, mut events) = start(through).await;
    let subject = unique("offline");
    send(&alice, "bob@example.com", &subject).await;
    bob.tx
        .send(Request::Sync {
            folder: "INBOX".into(),
            known: Known::default(),
            count: 500,
        })
        .unwrap();
    let synced = next(&mut events, |e| match e {
        Event::Synced(s) => Some(s),
        _ => None,
    })
    .await;
    let uid = synced
        .new
        .iter()
        .find(|h| h.summary.subject == subject)
        .unwrap()
        .uid;

    proxy.cut();
    bob.tx
        .send(Request::Flag {
            uidvalidity: synced.uidvalidity,
            folder: "INBOX".into(),
            uids: vec![uid],
            flag: FlagKind::Flagged,
            on: true,
        })
        .unwrap();
    // The connection is found gone; the change is held, not dropped.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match events.recv().await {
                Some(Event::State(worker::State::Offline(_))) => return,
                Some(Event::Failed { what, error }) => panic!("{what}: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("offline");

    proxy.restore();
    tokio::time::sleep(Duration::from_secs(32)).await;
    bob.tx
        .send(Request::Sync {
            folder: "INBOX".into(),
            known: Known {
                uidvalidity: synced.uidvalidity,
                low: synced.low,
                high: synced.high,
            },
            count: 500,
        })
        .unwrap();
    let again = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(Event::Synced(s)) = events.recv().await {
                return s;
            }
        }
    })
    .await
    .expect("a check once back");
    let flags = &again.flags.iter().find(|(u, _)| *u == uid).unwrap().1;
    assert!(flags.flagged, "the change made offline reached the server");
}
