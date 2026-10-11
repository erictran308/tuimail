<div align="center">

# tuimail

**Your mail, at the speed of your keyboard.**

A terminal email client (TUI): IMAP and POP3 to read, SMTP to send, several accounts side by side.<br>
Arrow keys or vim keys. Passwords in your system's keychain. Nothing loaded from the internet when you read.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024-orange.svg)](https://www.rust-lang.org)

**[Get started](#get-started)** · **[Keys](#keys)** · **[Accounts](#accounts)** · **[Settings](#settings)** · **[Privacy](#privacy-and-security)**

<sub>A sibling of [tuigram](https://github.com/erictran308/tuigram) (Telegram) and [tuitube](https://github.com/erictran308/tuitube) (YouTube): same look, same keys, same themes.</sub>

</div>

```
╭ ✉ tuimail ───────────────╮╭ Inbox · work ─────────── 1 unread · 7 messages ╮╭────────────────────────────────────────────────╮
│ ≡ All inboxes          2 ┃│▌    + Alice Ri… Lunch tomorrow?         09:49  ││ Lunch tomorrow?                                ┃
│ @ work                   ┃│ ●     GitHub    [tuimail] Release …     08:14  ││                                                ┃
│   ↓ Inbox              1 ┃│   ★   Bob Stone Quarterly numbers   Sat 07:14  ││ From  Alice Rivera <alice@example.com>         ┃
│   ✎ Drafts             1 ┃│   ↩   Priya Na… Re: Trip to Tokyo   Thu 10:14  ││ To    Sam Taylor <me@example.org>              ┃
│   ↑ Sent                 ┃│       Terminal… 10 TUI apps worth …     5 Oct  ││ Date  Sun 11 Oct 2026 09:49                    ┃
│   ▣ Archive              ┃│       Maya Chen Design review notes    29 Sep  ││ Files + menu.pdf (9 B)                         ┃
│   ⊘ Junk                 ┃│     + Leo Park  Photos from the hi…     1 Sep  ││ ────────────────────────────────────────────── ┃
│   ✗ Trash                ┃│                                                ││ Hi Sam,                                        ┃
│   ▸ Projects             ┃│                                                ││                                                ┃
│     ▸ Website            ┃│                                                ││ Are we still on for noon? The new ramen place  ┃
│ @ home                   ┃│                                                ││ opened on 5th,                                 ┃
│   ↓ Inbox              1 ┃│                                                ││ their menu is attached, and here:              ┃
│   ✎ Drafts               ┃│                                                ││ https://ramen.example.com/menu                 ┃
│   ↑ Sent                 ┃│                                                ││                                                │
│   ▣ Archive              ││                                                ││ > Should we invite Bob?                        │
╰──────────────────────────╯╰────────────────────────────────────────────────╯╰────────────────────────────────────────────────╯
 Enter open · c write · r reply · a all · f forward · d delete · e archive · s star · / search · ? help                    demo
```

<sub>The demo's made-up mail: <code>tuimail --demo</code></sub>

---

## Why tuimail

- **IMAP, POP3 and SMTP.** IMAP accounts keep mail on the server, in its folders; POP3 accounts download the inbox and keep Sent, Drafts, Archive and Trash on your computer. Both send through SMTP, with TLS or STARTTLS.
- **New mail at once.** Where the server can (IMAP IDLE), new mail shows the moment it arrives, with a notification through your terminal. Elsewhere tuimail checks every few minutes, and `R` checks now.
- **Several accounts.** Each in the sidebar with its folders, and **All inboxes** together at the top. Write from any of them (`←` `→` on From).
- **Arrows or vim, your choice.** `←↓↑→` and `h j k l` both move, `Enter` opens, `gg` / `G`, `Ctrl-d` / `Ctrl-u`. `?` lists every key.
- **Reading made calm.** HTML mail is shown as text (bold, italics, links numbered), with nothing it points to fetched: no tracking pixels, no "read" receipts by image. Quotes are colored by depth. `H` shows the plain-text part instead, `o` lists the links, `A` the attachments.
- **Writing in place, or in your editor.** To, Cc, Bcc and Subject with addresses completed from people you've written to or heard from; the text in place, or `Ctrl-e` for `$EDITOR`. Drop a file on the window to attach it. Replies quote, forwards carry the attachments, drafts go to Drafts and open again where you left them.
- **Quick to tidy.** `d` deletes (to Trash), `e` archives, `!` marks junk, `m` moves anywhere, `s` stars, `u` marks read or unread. `Space` marks several messages for any of these at once. `U` unsubscribes from a mailing list.
- **Search.** `/` finds mail by sender, recipients, subject or text: at once in what's on your computer, then on the server, which searches whole messages.
- **Works offline.** Headers and the messages you've opened are kept in a local cache, so the list and what you've read are there without a connection.
- **Set up in a minute.** Type your address and password: the servers fill in for Gmail, iCloud, Outlook, Yahoo, Fastmail, Proton (through its Bridge), Zoho, GMX, WEB.DE, Yandex, Mail.ru, Posteo, mailbox.org and more, or are guessed for others. tuimail logs in to check before saving.
- **Make it yours.** Catppuccin, Tokyo Night, Dracula, Gruvbox, Nord and Rosé Pine themes, Nerd Font icons where your terminal has them.

## Get started

**1. Install.** With Rust and [cargo-binstall](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall tuimail
```

Or build from source (Rust 1.90 or newer, and a C compiler for SQLite):

```sh
cargo install tuimail --locked
```

Release builds for macOS, Linux and Windows (x86_64 and ARM64) are on the [releases page](https://github.com/erictran308/tuimail/releases/latest); each comes with a signed record of the commit it was built from (`gh attestation verify <file> --repo erictran308/tuimail`).

**2. Run it.**

```sh
tuimail
```

The first start asks for an account: your address and password (for Gmail, iCloud, Yahoo and others, an **app password**: see [Accounts](#accounts)). The servers fill in; `Ctrl-s` logs in to check them and saves the account, with the password in your system's keychain.

To look around first, `tuimail --demo` shows made-up accounts and mail, connecting nowhere. `tuimail --check` says where your data is and which accounts it found.

## Keys

The status bar shows the keys for where you are, and `?` lists them all.

| Key | Action |
| --- | --- |
| `←↓↑→` / `h j k l` | Move; `Enter`, `→` or `l` opens |
| `Tab` | Folders → messages → the message |
| `gg` / `G`, `Ctrl-u` / `Ctrl-d` | First / last, half a page |
| `c` | Write a message |
| `r` / `a` / `f` | Reply / reply to all / forward |
| `d` | Delete: to Trash; from Trash, for good (asking first) |
| `e` / `!` / `m` | Archive / junk / move to a folder |
| `s` / `u` | Star / read or unread |
| `Space` / `*` | Mark a message / all of them; the keys above act on the marked |
| `/` | Search; `Esc` ends it |
| `J` / `K` | Reading: the next / previous message |
| `Space` / `b` | Reading: a page down / up |
| `o` / `A` | Reading: the links (`Enter` open, `y` copy) / attachments (`Enter` open, `s` save) |
| `H` | Reading: HTML or plain text |
| `U` | Unsubscribe from a mailing list |
| `R` | Check for mail now |
| `+` / `x` / `P` | Add an account / remove it (on its row) / type its password |
| `?` / `O` | Keys / settings and themes |
| `q` | Quit |

Writing:

| Key | Action |
| --- | --- |
| `Tab` / `Shift-Tab`, `Enter` | The next / previous field; `Tab` takes a suggested address |
| `Ctrl-s` | Send (asking first, unless turned off) |
| `Ctrl-d` | Save in Drafts and close |
| `Ctrl-o` | Attach a file (or drop it on the window) |
| `Ctrl-e` | Write the text in your editor (`$VISUAL`, `$EDITOR`, or `editor` in the settings) |
| `←` `→` on From | Send from another account |
| `x` on Files | Take a file off |
| `Esc` | Close: `y` keeps it in Drafts, `n` throws it away |

## Accounts

Accounts are kept in `accounts.toml` in tuimail's data folder (`tuimail --help` says where). The setup screen (`+`) writes it; you can also edit it while tuimail isn't running:

```toml
[[account]]
name = "work"                      # what the sidebar calls it
email = "me@example.org"
display_name = "Sam Taylor"        # your name, as recipients see it
protocol = "imap"                  # or "pop3"
host = "imap.example.org"
port = 993                         # if not the usual one
security = "tls"                   # "tls", "starttls", or "none" (this computer only)
smtp_host = "smtp.example.org"
smtp_port = 465
smtp_security = "tls"
password = "keychain"              # "keychain", "ask" (each start) or "command"
signature = "Sam"
```

More keys: `username` and `smtp_username` (if not the address), `password_command` (below), `save_sent` (copy sent mail to Sent; off by default for Gmail and Outlook, which do it themselves), `certificate` (a server certificate to trust, for Proton Mail Bridge or your own server), `leave_on_server` (POP3, on by default), and `sent_folder`, `drafts_folder`, `trash_folder`, `archive_folder`, `junk_folder` when the server doesn't say which folder is which.

### App passwords

Gmail, iCloud, Yahoo, AOL, Fastmail and others don't take your normal password from mail programs: make an app password in your account's security settings and use that. For Gmail: [myaccount.google.com/apppasswords](https://myaccount.google.com/apppasswords) (it needs 2-Step Verification on).

### Passwords from a command

To keep the password in a password manager instead of the keychain:

```toml
password = "command"
password_command = ["/opt/homebrew/bin/pass", "show", "mail/work"]
```

The program is given by its full path and run without a shell; tuimail uses the first line it prints. It runs once at start with your terminal, so it can ask you to unlock something.

### OAuth2 (Outlook.com, Microsoft 365, Gmail without app passwords)

Outlook.com takes only OAuth2. tuimail logs in with XOAUTH2 using an access token printed by a command, run again for each connection so a short-lived token stays fresh. [oama](https://github.com/pdobsan/oama) and the `mutt_oauth2.py` script both do this:

```toml
auth = "oauth2"
password_command = ["/opt/homebrew/bin/oama", "access", "me@outlook.com"]
host = "outlook.office365.com"
smtp_host = "smtp-mail.outlook.com"
smtp_port = 587
smtp_security = "starttls"
```

### Proton Mail

Run [Proton Mail Bridge](https://proton.me/mail/bridge), use the password it shows for the account, and export its certificate (Bridge → Settings → Export TLS certificates), then point tuimail at it:

```toml
host = "127.0.0.1"
port = 1143
security = "starttls"
smtp_host = "127.0.0.1"
smtp_port = 1025
smtp_security = "starttls"
certificate = "/Users/you/Documents/bridge/cert.pem"
```

## Settings

`O` opens the settings: the preview beside the list, plain text or HTML, marking read, asking before deleting and sending, notifications, and the theme. They're kept in `settings.toml` beside `accounts.toml`, which also takes:

| Setting | Default | What |
| --- | --- | --- |
| `check_minutes` | 5 | How often to check for mail where the server can't say at once |
| `fetch_count` | 300 | How many of a folder's newest messages to fetch at first (older ones as you scroll) |
| `sidebar_width` | 28 | The sidebar's width in columns |
| `downloads` | your Downloads folder | Where `s` saves attachments |
| `editor` | `$VISUAL` / `$EDITOR` | The editor for `Ctrl-e`, as a list: `["/usr/bin/vim"]` |
| `notifications` | `"auto"` | `"auto"`, `"off"`, `"bell"`, `"osc9"`, `"osc777"` or `"osc99"` |
| `icons` | `"auto"` | `"auto"`, `"nerd"` or `"plain"` |

Environment: `TM_DATA_DIR` keeps everything in another folder (a full path); `TM_ICONS` picks the icons.

## Privacy and security

- **Nothing fetched to read.** HTML is turned into text on your computer; pictures are shown by their description, never downloaded, so senders can't tell when you read their mail.
- **Passwords stay out of files.** They're in your system's keychain (macOS Keychain, Windows Credential Manager, the Secret Service on Linux), asked for at each start, or printed by your own command. Never in `accounts.toml`, never on a command line.
- **TLS, checked.** Connections use TLS (or STARTTLS, which must succeed before anything is sent) and your system's trusted certificates. An unencrypted connection is allowed only to a server on your own computer.
- **Nothing about your computer in what you send.** Messages go out with a random Message-ID and a UTC date, and tuimail greets SMTP servers as `[127.0.0.1]`, so your computer's name never ends up in mail headers.
- **Text from senders can't take over your terminal.** Control characters, escape sequences and direction-changing characters are taken out of everything shown, copied or notified.
- **Attachments are handled with care.** File names are cleaned (no folders, no hidden extensions), files never overwrite one another, they're marked as downloaded from the internet, and types that can run code ask before opening. Links open only through your browser, and only `http`, `https` and `mailto`.
- **Local data is yours alone.** The data folder is made readable only by you; the cache (`tuimail.db`) overwrites what's deleted.

See [SECURITY.md](SECURITY.md) to report a problem.

## Building and testing

```sh
cargo build
cargo test                    # offline tests
cargo clippy --all-targets
tuimail --demo                # the UI with made-up mail
```

The tests against a real server run against [GreenMail](https://greenmail-mail-test.github.io/greenmail/) in Docker; see the top of `src/live.rs`.

## License

MIT
