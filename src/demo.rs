//! `tuimail --demo`: the real UI with made-up accounts and mail, kept in
//! memory. Nothing connects anywhere and nothing is sent; settings changed
//! last for the run.

use std::io::stdout;

use anyhow::Result;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;

use crate::account::{Account, Protocol};
use crate::app::{self, App};
use crate::mail::Addr;
use crate::settings::Settings;
use crate::store::{Flags, Folder, Role, Store};

pub fn accounts() -> Vec<Account> {
    vec![
        Account {
            name: "work".into(),
            email: "me@example.org".into(),
            display_name: "Sam Taylor".into(),
            host: "imap.example.org".into(),
            smtp_host: "smtp.example.org".into(),
            signature: "Sam".into(),
            ..Account::default()
        },
        Account {
            name: "home".into(),
            email: "sam@home.example".into(),
            display_name: "Sam Taylor".into(),
            protocol: Protocol::Pop3,
            host: "pop.home.example".into(),
            smtp_host: "smtp.home.example".into(),
            ..Account::default()
        },
    ]
}

/// A plain message.
fn message(from: &str, to: &str, subject: &str, ago: i64, body: &str) -> String {
    let date = chrono::DateTime::from_timestamp(app::now() - ago, 0)
        .unwrap_or_default()
        .to_rfc2822();
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: {date}\r\n\
         Message-ID: <{ago}.demo@example.org>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}",
        body.replace('\n', "\r\n")
    )
}

/// A message with an HTML body.
fn html(from: &str, to: &str, subject: &str, ago: i64, body: &str) -> String {
    message(from, to, subject, ago, "").replace(
        "Content-Type: text/plain; charset=utf-8\r\n\r\n",
        &format!("Content-Type: text/html; charset=utf-8\r\n\r\n{body}"),
    )
}

/// A message with files attached (their bytes made up).
fn with_files(
    from: &str,
    to: &str,
    subject: &str,
    ago: i64,
    body: &str,
    files: &[(&str, &str)],
) -> String {
    let head = message(from, to, subject, ago, "");
    let head = head.replace(
        "Content-Type: text/plain; charset=utf-8\r\n\r\n",
        "Content-Type: multipart/mixed; boundary=\"demo\"\r\n\r\n",
    );
    let mut out = format!(
        "{head}--demo\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n",
        body.replace('\n', "\r\n")
    );
    for (name, mime) in files {
        out.push_str(&format!(
            "--demo\r\nContent-Type: {mime}; name=\"{name}\"\r\n\
             Content-Disposition: attachment; filename=\"{name}\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\nZGVtbyBmaWxl\r\n"
        ));
    }
    out.push_str("--demo--\r\n");
    out
}

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

/// Fills the store with the demo's folders and mail.
pub fn fill(store: &mut Store, accounts: &[Account]) {
    let work = accounts[0].id();
    let folder = |name: &str, role: Option<Role>| Folder {
        name: name.into(),
        shown: name.into(),
        delimiter: Some("/".into()),
        role,
        selectable: true,
        unread: 0,
        total: 0,
    };
    let _ = store.set_folders(
        &work,
        &[
            folder("INBOX", Some(Role::Inbox)),
            folder("Drafts", Some(Role::Drafts)),
            folder("Sent", Some(Role::Sent)),
            folder("Archive", Some(Role::Archive)),
            folder("Junk", Some(Role::Junk)),
            folder("Trash", Some(Role::Trash)),
            folder("Projects", None),
            folder("Projects/Website", None),
        ],
    );
    let me = "Sam Taylor <me@example.org>";
    let unread = Flags::default();
    let read = Flags {
        seen: true,
        ..Flags::default()
    };
    let inbox: Vec<(String, Flags)> = vec![
        (
            with_files(
                "Alice Rivera <alice@example.com>",
                me,
                "Lunch tomorrow?",
                25 * 60,
                "Hi Sam,\n\nAre we still on for noon? The new ramen place opened on 5th,\n\
                 their menu is attached, and here: https://ramen.example.com/menu\n\n\
                 > Should we invite Bob?\n\nSure, ask him.\n\nAlice",
                &[("menu.pdf", "application/pdf")],
            ),
            unread,
        ),
        (
            html(
                "GitHub <notifications@github.example>",
                me,
                "[tuimail] Release v0.1.0 (#12)",
                2 * HOUR,
                "<p><b>erictran308</b> published a release.</p><h2>tuimail 0.1.0</h2>\
                 <ul><li>IMAP and POP3</li><li>Sending with SMTP</li><li>Themes</li></ul>\
                 <p><a href=\"https://github.example/erictran308/tuimail/releases\">View it on GitHub</a></p>\
                 <img src=\"https://github.example/beacon.gif\" alt=\"\">",
            ),
            unread,
        ),
        (
            message(
                "Bob Stone <bob@example.org>",
                me,
                "Quarterly numbers",
                DAY + 3 * HOUR,
                "Sam,\n\nThe numbers for Q3 are in: revenue up 12%, costs flat.\n\
                 Full sheet on the drive.\n\nOn Monday, Sam Taylor wrote:\n> Any news on Q3?\n\n-- \nBob",
            ),
            Flags {
                seen: true,
                flagged: true,
                ..Flags::default()
            },
        ),
        (
            message(
                "Priya Nair <priya@example.net>",
                me,
                "Re: Trip to Tokyo",
                3 * DAY,
                "Booked! Friday the 14th, back on the 21st.\n\n\
                 > Which dates work for you?\n>> Any week in November.\n",
            ),
            Flags {
                seen: true,
                answered: true,
                ..Flags::default()
            },
        ),
        (
            html(
                "Terminal Weekly <hello@terminal-weekly.example>",
                me,
                "10 TUI apps worth trying this fall",
                6 * DAY,
                "<h1>Terminal Weekly</h1><p>This week: <em>ten</em> apps that live in your terminal.</p>\
                 <ol><li><a href=\"https://tuigram.example\">tuigram</a>: Telegram</li>\
                 <li><a href=\"https://tuitube.example\">tuitube</a>: YouTube</li>\
                 <li><a href=\"https://tuimail.example\">tuimail</a>: your mail</li></ol>\
                 <p><code>cargo install tuimail</code></p>",
            ),
            read,
        ),
        (
            message(
                "Maya Chen <maya@example.com>",
                me,
                "Design review notes",
                12 * DAY,
                "Notes from today:\n\n1. Rounded corners everywhere\n2. Keys in the accent color\n\
                 3. Fewer popups\n\nMaya",
            ),
            read,
        ),
        (
            with_files(
                "Leo Park <leo@example.com>",
                me,
                "Photos from the hike",
                40 * DAY,
                "Two of the best ones. The sunrise was worth the 5 am start.",
                &[("sunrise.jpg", "image/jpeg"), ("summit.jpg", "image/jpeg")],
            ),
            read,
        ),
    ];
    for (raw, flags) in inbox {
        let _ = store.add_local(&work, "INBOX", raw.as_bytes(), flags, None);
    }
    previews(store, &work);
    let _ = store.add_local(
        &work,
        "Sent",
        message(
            me,
            "Priya Nair <priya@example.net>",
            "Trip to Tokyo",
            4 * DAY,
            "Which dates work for you?\n",
        )
        .as_bytes(),
        read,
        None,
    );
    let _ = store.add_local(
        &work,
        "Drafts",
        message(
            me,
            "Maya Chen <maya@example.com>",
            "Ideas for the offsite",
            5 * HOUR,
            "A few ideas:\n\n- a hike\n- ",
        )
        .as_bytes(),
        Flags {
            seen: true,
            draft: true,
            ..Flags::default()
        },
        None,
    );
    let _ = store.add_local(
        &work,
        "Projects/Website",
        message(
            "Dana Lee <dana@example.com>",
            me,
            "New landing page",
            9 * DAY,
            "Draft is up for review.",
        )
        .as_bytes(),
        read,
        None,
    );
    if let Some(home) = accounts.get(1) {
        let home = home.id();
        let _ = store.set_folders(&home, &crate::app::local_folders());
        let to = "Sam Taylor <sam@home.example>";
        let _ = store.add_local(
            &home,
            "INBOX",
            message(
                "Mom <mom@family.example>",
                to,
                "Call me when you land",
                5 * HOUR,
                "Safe travels! Call me when you land.\n\nLove, Mom",
            )
            .as_bytes(),
            unread,
            None,
        );
        let _ = store.add_local(
            &home,
            "INBOX",
            html(
                "Example Bank <statements@bank.example>",
                to,
                "Your statement is ready",
                2 * DAY,
                "<p>Your October statement is ready.</p><p><a href=\"https://bank.example/statements\">Sign in to see it</a></p>",
            )
            .as_bytes(),
            read,
            None,
        );
        previews(store, &home);
        for folder in ["INBOX", "Drafts", "Sent", "Archive", "Trash"] {
            if let Ok((u, t)) = store.count(&home, folder) {
                let _ = store.set_counts(&home, folder, u, t);
            }
        }
    }
    for folder in [
        "INBOX",
        "Drafts",
        "Sent",
        "Archive",
        "Junk",
        "Trash",
        "Projects",
        "Projects/Website",
    ] {
        if let Ok((u, t)) = store.count(&work, folder) {
            let _ = store.set_counts(&work, folder, u, t);
        }
    }
    let people = [
        ("Alice Rivera", "alice@example.com"),
        ("Bob Stone", "bob@example.org"),
        ("Priya Nair", "priya@example.net"),
        ("Maya Chen", "maya@example.com"),
        ("Leo Park", "leo@example.com"),
    ];
    let addrs: Vec<Addr> = people
        .iter()
        .map(|(name, addr)| Addr {
            name: name.to_string(),
            addr: addr.to_string(),
        })
        .collect();
    let _ = store.remember(&addrs, false, app::now());
}

/// The list's previews of the demo's mail, made here: it's made up, so it
/// lays out quickly and safely without a thread of its own.
fn previews(store: &Store, account: &str) {
    for folder in ["INBOX", "Drafts", "Sent", "Archive", "Projects/Website"] {
        for message in store.messages(account, folder, 100).unwrap_or_default() {
            if let Ok(Some(raw)) = store.body(account, folder, message.uid) {
                let _ =
                    store.set_preview(account, folder, message.uid, &crate::mail::preview_of(&raw));
            }
        }
    }
}

/// Runs the UI on the made-up mail.
pub async fn run() -> Result<()> {
    crate::need_terminal()?;
    let accounts = accounts();
    let mut store = Store::in_memory();
    fill(&mut store, &accounts);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut terminal = ratatui::init();
    crate::panic_hook();
    execute!(stdout(), EnableBracketedPaste)?;
    let mut app = App::new(store, Settings::default(), accounts, false, None, tx, true);
    app.info("The demo: made-up mail, nothing is sent or fetched. ? lists the keys, q quits.");
    let result = app.run(&mut terminal, rx).await;
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}
