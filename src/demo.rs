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

/// A message with files attached: their names, types and sizes (their
/// bytes made up).
fn with_files(
    from: &str,
    to: &str,
    subject: &str,
    ago: i64,
    body: &str,
    files: &[(&str, &str, usize)],
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
    for (name, mime, size) in files {
        // `size` zero bytes, in base64's lines of 76.
        let encoded = "AAAA".repeat(size / 3);
        let lines: Vec<&str> = encoded
            .as_bytes()
            .chunks(76)
            .map(|l| std::str::from_utf8(l).unwrap_or_default())
            .collect();
        out.push_str(&format!(
            "--demo\r\nContent-Type: {mime}; name=\"{name}\"\r\n\
             Content-Disposition: attachment; filename=\"{name}\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            lines.join("\r\n")
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
                "Hi Sam,\n\nAre we still on for noon? The new ramen place on 5th Street \
                 opened last week, and the menu looks great: it's attached, and here:\n\
                 https://ramen.example.com/menu\n\n\
                 They don't take bookings, so I'd get there ten minutes early.\n\n\
                 > Should we invite Bob?\n>\n>> Lunch tomorrow? I found a new place.\n\n\
                 Sure, ask him. I'll tell him it's a numbers-free lunch.\n\nAlice",
                &[("menu.pdf", "application/pdf", 184_320)],
            ),
            unread.clone(),
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
            unread.clone(),
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
                tags: vec!["work".into()],
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
                tags: vec!["travel".into()],
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
            Flags {
                tags: vec!["newsletter".into()],
                ..read.clone()
            },
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
            read.clone(),
        ),
        (
            with_files(
                "Leo Park <leo@example.com>",
                me,
                "Photos from the hike",
                40 * DAY,
                "Two of the best ones. The sunrise was worth the 5 am start.",
                &[
                    ("sunrise.jpg", "image/jpeg", 1_258_291),
                    ("summit.jpg", "image/jpeg", 1_048_576),
                ],
            ),
            read.clone(),
        ),
    ];
    let answered = Flags {
        answered: true,
        ..read.clone()
    };
    let starred = Flags {
        flagged: true,
        ..read.clone()
    };
    // Older mail, so the inbox looks lived in.
    let older: Vec<(String, Flags)> = vec![
        (
            message(
                "Kai Müller <kai@example.de>",
                me,
                "Re: Faster search (#42)",
                13 * DAY,
                "Merged, thanks! Search over 50k messages went from 900 ms to 40 ms.\n\nKai",
            ),
            answered.clone(),
        ),
        (
            message(
                "Nora Ali <nora@example.com>",
                me,
                "Book club: chapter 7",
                15 * DAY,
                "We meet Thursday at 7. Bring your favorite line from chapter 7.\n\nNora",
            ),
            read.clone(),
        ),
        (
            with_files(
                "Skyway Air <no-reply@skyway.example>",
                me,
                "Your flight to Tokyo is booked",
                17 * DAY,
                "Booking SKY4K2. Friday 14 November, 10:40, seat 23A.\nYour itinerary is attached.",
                &[("itinerary.pdf", "application/pdf", 98_304)],
            ),
            starred.clone(),
        ),
        (
            message(
                "Tom Becker <tom@example.org>",
                me,
                "Coffee next week?",
                20 * DAY,
                "Free Tuesday or Wednesday morning? The usual place.\n\nTom",
            ),
            answered.clone(),
        ),
        (
            with_files(
                "Calendar <calendar@example.org>",
                me,
                "Invitation: Sprint planning, Mon 10:00",
                22 * DAY,
                "Maya Chen invites you to Sprint planning, Monday 10:00 to 11:00.",
                &[("invite.ics", "text/calendar", 2_048)],
            ),
            read.clone(),
        ),
        (
            with_files(
                "Hana Sato <hana@example.jp>",
                me,
                "Slides from the offsite",
                25 * DAY,
                "Here are the slides, and the notes from the afternoon.\n\nHana",
                &[("offsite.pdf", "application/pdf", 1_572_864)],
            ),
            read.clone(),
        ),
        (
            with_files(
                "Cloudbox Billing <billing@cloudbox.example>",
                me,
                "Your invoice for September",
                29 * DAY,
                "Your invoice for September is attached: 12.00 USD, paid by card.",
                &[("invoice-2026-09.pdf", "application/pdf", 65_536)],
            ),
            read.clone(),
        ),
        (
            message(
                "Omar Haddad <omar@example.com>",
                me,
                "Welcome to the team!",
                33 * DAY,
                "Welcome aboard, Sam! Your desk is by the window. Lunch is on us Friday.\n\nOmar",
            ),
            starred.clone(),
        ),
        (
            message(
                "Rust Digest <news@rust-digest.example>",
                me,
                "This month in Rust: async closures",
                36 * DAY,
                "Async closures are stable, and cargo builds got faster again.",
            ),
            read.clone(),
        ),
        (
            message(
                "Ivy Brooks <ivy@example.com>",
                me,
                "Re: Your conference talk",
                45 * DAY,
                "The talk is in! Room B, Saturday at 2. Slides due a week before.\n\nIvy",
            ),
            answered.clone(),
        ),
        (
            message(
                "Jonas Weber <jonas@example.de>",
                me,
                "Moving the standup to 9:30",
                52 * DAY,
                "From Monday the standup is at 9:30, same room.\n\nJonas",
            ),
            read.clone(),
        ),
    ];
    let inbox: Vec<(String, Flags)> = inbox.into_iter().chain(older).collect();
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
        read.clone(),
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
        read.clone(),
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
            unread.clone(),
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
            Flags {
                tags: vec!["bills".into()],
                ..read.clone()
            },
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
    // Agents try it here, on made-up mail, once the setting is on
    // (`tuimail mcp --demo`): the data folder is only looked for then.
    app.agent_socket = crate::config::Config::load()
        .ok()
        .map(|c| crate::agent::socket(&c.data_dir, true));
    app.info("The demo: made-up mail, nothing is sent or fetched. ? lists the keys, q quits.");
    let result = app.run(&mut terminal, rx).await;
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Focus, Place};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};

    fn hex(color: Color) -> String {
        match color {
            Color::Rgb(r, g, b) => format!("\"#{r:02x}{g:02x}{b:02x}\""),
            Color::White => "\"#ffffff\"".into(),
            Color::Black => "\"#000000\"".into(),
            _ => "null".into(),
        }
    }

    fn quoted(text: &str) -> String {
        let mut out = String::from("\"");
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    /// One frame of the demo for `tools/hero.py`: every cell's character,
    /// colors and style. The work inbox, with Alice's message beside the
    /// list.
    #[tokio::test]
    #[ignore = "writes target/hero/screen.json for tools/hero.py"]
    async fn export_hero_screen() {
        let (cols, rows) = (140u16, 30u16);
        let (cell_width, cell_height) = (16, 34);
        let accounts = accounts();
        let mut store = Store::in_memory();
        fill(&mut store, &accounts);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(store, Settings::default(), accounts, false, None, tx, true);
        app.test_rx = Some(rx);
        // As Ghostty shows it, whatever terminal runs the test.
        app.icons = crate::icons::NERD;
        app.open_place(Place::Folder {
            account: 0,
            folder: "INBOX".into(),
        });
        app.focus = Focus::List;
        app.selected = 0;
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        // Laid out again for the width the reader got, as the event loop
        // does after drawing.
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        crate::app::tests::wait_for_layout(&mut app).await;
        app.after_draw();
        crate::app::tests::wait_for_layout(&mut app).await;
        app.focus = Focus::List;
        app.status = None;
        // The status bar's corner says "demo" in the demo: not in a picture
        // of the app.
        app.demo = false;
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let mut cells = Vec::new();
        for y in 0..rows {
            for x in 0..cols {
                let cell = &buffer[(x, y)];
                cells.push(format!(
                    "[{},{},{},{},{},{}]",
                    quoted(cell.symbol()),
                    hex(cell.fg),
                    hex(cell.bg),
                    cell.modifier.contains(Modifier::BOLD),
                    cell.modifier.contains(Modifier::ITALIC),
                    cell.modifier.contains(Modifier::UNDERLINED),
                ));
            }
        }
        let screen = format!(
            "{{\"cols\":{cols},\"rows\":{rows},\"cell\":[{cell_width},{cell_height}],\
             \"bg\":{},\"fg\":{},\"cells\":[{}]}}",
            hex(app.colors.bg),
            hex(app.colors.text),
            cells.join(",")
        );
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/hero");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("screen.json"), screen).unwrap();
        eprintln!("wrote {}", dir.join("screen.json").display());
    }
}
