//! The app's state and everything that changes it: keys, and what the
//! accounts' workers report. All state lives in [`App`]; nothing else
//! mutates it. The cache (`Store`) is written only from here.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use ratatui_textarea::{CursorMove, TextArea};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::account::{self, Account, PasswordFrom, Protocol};
use crate::agent::{self, Call};
use crate::compose::{self, Draft};
use crate::composer::{self, Composer, Kind};
use crate::icons::Icons;
use crate::imap::FlagKind;
use crate::mail::{self, Addr, Body, Parsed};
use crate::notify::{self, Notifications, Notifier};
use crate::secrets::{Credentials, Secret};
use crate::settings::Settings;
use crate::setup::{self, Setup};
use crate::store::{self, Change, Changed, Flags, Folder, Known, Message, Role, Store, What};
use crate::theme::{self, Colors};
use crate::worker::{self, Endpoints, Request, State, Worker};
use crate::{net, tags, text, tools};

/// The longest search or path typed or pasted, in characters.
const MAX_PROMPT: usize = 4096;
/// Different keys read together, with nothing open to type in, that are
/// taken for a paste and ignored rather than run as commands.
const PASTED_KEYS: usize = 3;
/// The most messages listed from the cache at once.
const MAX_LISTED: usize = 20_000;
/// A message shown beside the list is marked read after this long.
const PREVIEW_READ_AFTER: Duration = Duration::from_millis(1500);
/// The cursor has to rest this long before the message under it is
/// fetched for the preview, so scrolling doesn't fetch each one.
const PREVIEW_AFTER: Duration = Duration::from_millis(250);
/// Older messages are fetched once the cursor is this close to the end.
const NEAR_END: usize = 30;
const STATUS_FOR: Duration = Duration::from_secs(6);
const ERROR_FOR: Duration = Duration::from_secs(12);
/// Flags changed here win over what a check brings back for this long:
/// the check may have read them before the change reached the server.
const LOCAL_FLAGS_WIN: Duration = Duration::from_secs(30);
/// A confirmation ignores `y` this long after it comes up, so a key meant
/// for something else can't answer it.
const CONFIRM_GRACE: Duration = Duration::from_millis(300);
/// An agent's question shows when another ends (answered, timed out, its
/// agent gone): a `y` meant for the one being read must not answer the
/// next, so its grace is longer.
const AGENT_GRACE: Duration = Duration::from_millis(1500);
const HELP_PAGE: isize = 5;
/// Every this many checks, each folder's counts are asked for too.
const FOLDERS_EVERY: u32 = 4;

pub enum AppEvent {
    /// An AI agent's request (`agent.rs`), answered on `reply`.
    #[cfg_attr(not(unix), allow(dead_code))]
    Agent {
        request: crate::agent::Request,
        reply: tokio::sync::oneshot::Sender<crate::agent::Answer>,
    },
    Worker {
        account: String,
        event: worker::Event,
    },
    /// The SMTP server took the message, or didn't.
    Sent {
        account: String,
        result: Result<(), String>,
        needs_password: bool,
        sent: SentCopy,
    },
    /// A message laid out for the reader, in the background.
    Rendered {
        generation: u64,
        width: u16,
        parsed: Option<Arc<Parsed>>,
        body: Option<Body>,
    },
    /// A reply, forward or saved draft made ready for the composer, by the
    /// account's id (an index could mean another once one is removed).
    Drafted {
        account: String,
        kind: Kind,
        draft: Option<Draft>,
        replying: Option<Kept>,
        draft_of: Option<Kept>,
    },
    /// An attachment taken out of its message: saved, or to open.
    Extracted {
        open: bool,
        /// The user said yes to opening it.
        confirmed: bool,
        result: Result<PathBuf, String>,
    },
    /// The start of messages' text, for the list, made in the background.
    Previews {
        account: String,
        previews: Vec<(String, u32, String)>,
    },
    /// The setup form's login check.
    Checked {
        result: Result<Option<crate::oauth::Tokens>, String>,
    },
    /// What a background task couldn't do, for the status line (keeping a
    /// new account's password in the keychain).
    Failed { error: String },
    /// A sign-in with Microsoft or Google waits for the browser: open it
    /// on `url`.
    SignInOpened { url: String },
    /// A sign-in finished, for the account with this id.
    SignedIn {
        account: String,
        result: Result<crate::oauth::Tokens, String>,
    },
    /// The terminal closed, or tuimail was asked to stop (Unix signals).
    #[cfg_attr(not(unix), allow(dead_code))]
    Quit,
}

/// A message held on to for later (the one being replied to, the saved
/// draft being replaced): its account by id, so switching From or removing
/// another account can't point it at someone else's mail, and what its
/// folder's UIDs meant when it was taken, so a folder made again on the
/// server can't point it at another message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub account: String,
    pub folder: String,
    pub uid: u32,
    pub uidvalidity: u32,
}

/// What follows a send that worked: the copy for Sent, and what to mark.
pub struct SentCopy {
    pub raw: Vec<u8>,
    pub to: Vec<Addr>,
    pub replying: Option<Kept>,
    pub draft_of: Option<Kept>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    List,
    Reader,
}

/// What the list shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    Folder {
        account: usize,
        folder: String,
    },
    /// Every account's inbox, together.
    AllInboxes,
}

/// A row of the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    AllInboxes,
    Account(usize),
    /// An account's folder (its index in the account's folders) and how
    /// deep it's nested.
    Folder(usize, usize, usize),
    Add,
}

/// A message in the list, and its account.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub account: usize,
    pub message: Message,
    /// Where it is in its conversation (threads on).
    pub thread: Thread,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Thread {
    /// The lines before it: `├─`, `└─`… (`threads::Placed::tree`).
    pub tree: String,
    /// It answers a message with the same subject.
    pub same_subject: bool,
}

impl Item {
    pub fn key(&self) -> (usize, String, u32) {
        (self.account, self.message.folder.clone(), self.message.uid)
    }
}

pub struct AccountState {
    pub account: Arc<Account>,
    pub id: String,
    pub worker: Option<Worker>,
    pub state: State,
    pub folders: Vec<Folder>,
    pub expanded: bool,
    /// Folders a check is running for.
    pub syncing: HashSet<String>,
    /// Older mail was asked for in these folders.
    older_asked: HashSet<String>,
    last_check: Option<Instant>,
    /// Checks so far: every folder's counts are asked for every
    /// [`FOLDERS_EVERY`]th.
    checks: u32,
    /// POP3: the first check is done (mail before it isn't announced).
    popped_once: bool,
    /// POP3: a check was asked for while one ran: another follows it.
    pop_again: bool,
    /// The password prompt was put off (`Esc`): it isn't opened on its own
    /// again, only with P or Enter on the account's row.
    password_put_off: bool,
    /// Changes (moves, flags, deletes) not yet made on the server.
    waiting: std::cell::Cell<usize>,
}

impl AccountState {
    /// The folder for `role`.
    pub fn folder_for(&self, role: Role) -> Option<&Folder> {
        self.folders.iter().find(|f| f.role == Some(role))
    }

    pub fn inbox(&self) -> String {
        self.folder_for(Role::Inbox)
            .map(|f| f.name.clone())
            .unwrap_or_else(|| "INBOX".into())
    }

    pub fn is_pop(&self) -> bool {
        self.account.protocol == Protocol::Pop3
    }
}

/// The message being read.
pub struct Reader {
    pub account: usize,
    pub folder: String,
    pub uid: u32,
    pub message: Message,
    pub raw: Option<Arc<Vec<u8>>>,
    pub parsed: Option<Arc<Parsed>>,
    pub body: Option<Body>,
    /// The width `body` was laid out for.
    pub width: u16,
    /// A layout is being made (its number).
    pending: Option<u64>,
    pub scroll: usize,
    pub error: Option<String>,
    /// The plain part (true) or the HTML (false), chosen with `H`.
    pub plain: Option<bool>,
    /// Opened with Enter (marked read at once), not just shown beside the
    /// list.
    opened_at: Instant,
    marked_read: bool,
}

impl Reader {
    pub fn key(&self) -> (usize, String, u32) {
        (self.account, self.folder.clone(), self.uid)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    Search,
    Password(usize),
    Attach,
    /// A new tag for [`App::tagging`].
    Tag,
}

pub struct Prompt {
    pub kind: PromptKind,
    pub input: TextArea<'static>,
}

impl Prompt {
    fn new(kind: PromptKind, text: &str) -> Self {
        let mut input = TextArea::new(vec![text.to_string()]);
        input.move_cursor(CursorMove::End);
        input.set_cursor_line_style(ratatui::style::Style::default());
        if matches!(kind, PromptKind::Password(_)) {
            input.set_mask_char('•');
        }
        Self { kind, input }
    }

    pub fn text(&self) -> String {
        self.input.lines().join("")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerKind {
    /// Where to move these messages.
    MoveTo {
        account: usize,
        folder: String,
        uids: Vec<u32>,
        uidvalidity: u32,
        /// The folders' server names, in the items' order.
        targets: Vec<String>,
    },
    Links,
    Attachments,
    /// Changes to undo, by id, in the items' order.
    Changes(Vec<i64>),
    /// A tag to put on these messages or take off, or (the last item) a
    /// new one to type.
    Tag {
        groups: Vec<Group>,
        /// The tags, in the items' order.
        tags: Vec<String>,
    },
}

/// Messages of one account and folder, with the UIDVALIDITY their UIDs
/// were read under: what a choice made later (a tag) is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub account: usize,
    pub folder: String,
    pub uids: Vec<u32>,
    pub uidvalidity: u32,
}

/// An agent's change waiting for the user's `y`.
pub struct AgentAsk {
    pub id: u64,
    /// The agent, as it calls itself.
    pub who: String,
    /// What it would do: "move 3 messages to Trash".
    pub what: String,
    /// The messages: sender and subject, a line each (a few).
    pub which: Vec<String>,
    call: Call,
    reply: tokio::sync::oneshot::Sender<agent::Answer>,
    at: Instant,
}

/// An agent's read waiting for its message to download.
struct AgentRead {
    key: (usize, String, u32),
    max: usize,
    reply: tokio::sync::oneshot::Sender<agent::Answer>,
    at: Instant,
}

/// How long a download for agents counts as on its way (past the worker's
/// time limit for one): a lost connection says nothing about it.
const AGENT_DOWNLOAD_FOR: Duration = Duration::from_secs(6 * 60);

/// An agent's change, checked: what it does to which messages.
struct Plan {
    action: Action,
    groups: Vec<Group>,
    /// Moves: where each group goes.
    targets: Vec<String>,
    /// What it does: "move 3 messages to Trash".
    what: String,
    /// How many it moves to Trash.
    trash: usize,
}

enum Action {
    Seen(bool),
    Flagged(bool),
    Tag(String, bool),
    Move,
    Undo(Box<Change>),
}

impl Plan {
    fn new(action: Action, groups: Vec<Group>, what: String) -> Self {
        Self {
            action,
            groups,
            targets: Vec::new(),
            what,
            trash: 0,
        }
    }

    /// How many messages it changes.
    fn count(&self) -> usize {
        match &self.action {
            Action::Undo(change) => change.messages.len(),
            _ => self.groups.iter().map(|g| g.uids.len()).sum(),
        }
    }
}

pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub items: Vec<String>,
    /// Links only: each one's site, from the whole URL, and what it says
    /// in the message (empty if nothing, or something like an address).
    pub labels: Vec<(String, String)>,
    pub selected: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirm {
    /// An AI agent's change ([`App::agent_asks`], by id).
    Agent {
        id: u64,
    },
    /// Delete for good (from Trash, or with no Trash).
    DeleteForever {
        account: usize,
        folder: String,
        uidvalidity: u32,
        uids: Vec<u32>,
    },
    /// Move to Trash, with `confirm_delete` on.
    Delete {
        account: usize,
        folder: String,
        uids: Vec<u32>,
        uidvalidity: u32,
    },
    /// Send the message, as it was when asked: `print` is the draft's
    /// digest with its account, and a `y` sends nothing else.
    Send {
        print: u64,
        subject: String,
        recipients: Vec<String>,
        attachments: Vec<String>,
    },
    /// Esc in a composer with something written: `y` saves a draft, `n`
    /// discards it.
    CloseComposer,
    OpenAttachment {
        index: u32,
        name: String,
    },
    RemoveAccount(usize),
    /// `U`'s web page, shown before it opens.
    OpenLink {
        url: String,
    },
    /// Quit with changes not yet made on the server.
    Quit {
        waiting: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelpTab {
    Keys,
    Settings,
}

pub struct HelpMenu {
    pub tab: HelpTab,
    pub scroll: usize,
    pub selected: usize,
    pub settings_scroll: usize,
}

impl HelpMenu {
    pub const PREVIEW: usize = 0;
    pub const PREFER_PLAIN: usize = 1;
    pub const MARK_READ: usize = 2;
    pub const THREADS: usize = 3;
    pub const CONFIRM_DELETE: usize = 4;
    pub const CONFIRM_SEND: usize = 5;
    pub const NOTIFICATIONS: usize = 6;
    pub const AGENTS: usize = 7;
    pub const CONFIRM_AGENTS: usize = 8;
    pub const THEMES: usize = 9;
    pub const LAST: usize = Self::THEMES + theme::BUILT_IN.len() - 1;

    pub fn new(tab: HelpTab) -> Self {
        Self {
            tab,
            scroll: 0,
            selected: 0,
            settings_scroll: 0,
        }
    }
}

pub struct Status {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

/// What a message being added to a folder is.
enum Append {
    /// The copy of a message sent, for Sent.
    SentCopy,
    /// A draft; the composer waits for it, then the draft it replaces goes.
    Draft { draft_of: Option<Kept> },
}

/// A reply, forward or draft waiting for its message to download.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    Reply { all: bool },
    Forward,
    EditDraft,
}

pub struct App {
    pub store: Store,
    pub settings: Settings,
    settings_path: Option<PathBuf>,
    accounts_path: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    pub colors: Colors,
    pub icons: Icons,
    pub accounts: Vec<AccountState>,
    tx: UnboundedSender<AppEvent>,
    /// The system's keychain can be used.
    keychain: bool,

    pub place: Place,
    pub items: Vec<Item>,
    pub selected: usize,
    pub list_scroll: Cell<usize>,
    /// Messages picked with Space for an action on them all.
    pub marked: HashSet<(usize, String, u32)>,
    /// The search in effect.
    pub search: Option<String>,

    pub sidebar_selected: usize,
    pub sidebar_scroll: Cell<usize>,
    pub focus: Focus,

    pub reader: Option<Reader>,
    /// The reader pane's width, from the last frame.
    pub reader_width: Cell<u16>,
    /// The window is wide enough to show the message beside the list.
    pub wide: Cell<bool>,
    /// The reader's height in rows, from the last frame (for paging).
    pub reader_rows: Cell<u16>,
    pub list_rows: Cell<u16>,
    renders: u64,
    /// The cursor moved: when, to show the message under it beside the
    /// list once it rests.
    moved_at: Option<Instant>,

    pub composer: Option<Composer>,
    pub setup: Option<Setup>,
    pub prompt: Option<Prompt>,
    /// The messages a new tag being typed is for.
    tagging: Vec<Group>,
    /// The change being made, kept once the key that makes it is done.
    change: Option<Change>,
    /// Where agents reach this tuimail (`None`: they don't).
    pub agent_socket: Option<PathBuf>,
    /// Listening for AI agents (the setting on).
    agents: Option<agent::Listener>,
    /// A sign-in waits for the browser.
    signing_in: bool,
    /// Agents' changes waiting for the user's `y`, oldest first.
    pub agent_asks: VecDeque<AgentAsk>,
    next_ask: u64,
    agent_reads: Vec<AgentRead>,
    /// Messages being downloaded for agents' reads, since when: one
    /// download each, however many reads wait for it.
    agent_downloads: HashMap<(usize, String, u32), Instant>,
    /// Agents' downloads in the last hour: when, and how big.
    agent_fetched: VecDeque<(Instant, u64)>,
    /// Agents' changes in the last hour: when, how many messages, how
    /// many to Trash (the hourly limits).
    agent_spent: VecDeque<(Instant, usize, usize)>,
    pub picker: Option<Picker>,
    pub confirm: Option<Confirm>,
    confirm_at: Instant,
    /// `d`'s questions still to ask: one each account and folder.
    deletes_waiting: VecDeque<Confirm>,
    pub help: Option<HelpMenu>,
    pub status: Option<Status>,
    pending: Option<((usize, String, u32), Pending)>,
    /// Flags changed here, and when.
    local_flags: HashMap<(usize, String, u32), Instant>,
    /// Messages moved or deleted here, and when: a check that read the
    /// folder before the server did it doesn't bring them back.
    local_removed: HashMap<(usize, String, u32), Instant>,
    /// Messages being added to a folder, by request id.
    appends: HashMap<u64, Append>,
    next_append: u64,

    notifier: Notifier,
    notifications: Notifications,
    title_unread: Option<u32>,
    pending_g: bool,
    pub quit: bool,
    pub demo: bool,
    /// The editor to open, run by the event loop (it has the terminal).
    edit_request: bool,
    clipboard: Option<arboard::Clipboard>,
    /// Tests read the app's own events here.
    #[cfg(test)]
    pub test_rx: Option<UnboundedReceiver<AppEvent>>,
    /// Tests read what was asked of the workers here, by account.
    #[cfg(test)]
    pub test_sent: std::cell::RefCell<Vec<(usize, Request)>>,
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A date for agents: RFC 3339, in local time.
fn agent_date(timestamp: i64) -> Option<String> {
    use chrono::TimeZone;
    (timestamp > 0)
        .then(|| chrono::Local.timestamp_opt(timestamp, 0).single())
        .flatten()
        .map(|d| d.to_rfc3339())
}

/// A message for an agent: `header` with who it's from and to, its
/// subject, attachments and text, links shown by their site only. Lays out
/// HTML: only on a big-stack thread (`mail::in_background`).
fn agent_text(raw: &[u8], max: usize, mut out: serde_json::Value) -> agent::Answer {
    let parsed = mail::parse(raw).ok_or("This message couldn't be read")?;
    let text = agent::hide_links(&mail::plain_text(&parsed));
    let cut = text.chars().count() > max;
    // Everything the sender wrote, links by their site only.
    let addresses = |list: &[Addr]| {
        (list.iter())
            .map(|a| agent::hide_links(&a.display()))
            .collect::<Vec<_>>()
    };
    out["from"] = serde_json::json!(addresses(&parsed.from));
    out["to"] = serde_json::json!(addresses(&parsed.to));
    out["cc"] = serde_json::json!(addresses(&parsed.cc));
    out["reply_to"] = serde_json::json!(addresses(&parsed.reply_to));
    out["subject"] = serde_json::json!(agent::hide_links(&parsed.summary.subject));
    out["attachments"] = (parsed.attachments.iter())
        .map(|a| serde_json::json!({ "name": agent::hide_links(&a.name), "size": mail::size(a.size) }))
        .collect();
    out["text"] = serde_json::json!(text::first_chars(&text, max));
    out["cut"] = serde_json::json!(cut);
    Ok(out)
}

impl App {
    pub fn new(
        store: Store,
        settings: Settings,
        accounts: Vec<Account>,
        keychain: bool,
        data_dir: Option<PathBuf>,
        tx: UnboundedSender<AppEvent>,
        demo: bool,
    ) -> Self {
        Self::build(
            store,
            settings,
            accounts,
            HashMap::new(),
            keychain,
            data_dir,
            tx,
            demo,
        )
    }

    /// The app, with the passwords password commands gave at start.
    pub fn with_passwords(
        store: Store,
        settings: Settings,
        accounts: Vec<Account>,
        passwords: HashMap<String, Secret>,
        keychain: bool,
        data_dir: Option<PathBuf>,
        tx: UnboundedSender<AppEvent>,
    ) -> Self {
        Self::build(
            store, settings, accounts, passwords, keychain, data_dir, tx, false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        store: Store,
        settings: Settings,
        accounts: Vec<Account>,
        mut passwords: HashMap<String, Secret>,
        keychain: bool,
        data_dir: Option<PathBuf>,
        tx: UnboundedSender<AppEvent>,
        demo: bool,
    ) -> Self {
        let colors = Colors::named(&settings.theme);
        let icons = Icons::pick(settings.icon_mode(), |name| std::env::var(name).ok());
        let notifications = settings
            .notifications
            .resolve(|name| std::env::var(name).ok());
        let mut app = Self {
            store,
            settings_path: data_dir.as_deref().map(crate::settings::path),
            accounts_path: data_dir.as_deref().map(account::path),
            data_dir,
            settings,
            colors,
            icons,
            accounts: Vec::new(),
            tx,
            keychain,
            place: Place::AllInboxes,
            items: Vec::new(),
            selected: 0,
            list_scroll: Cell::new(0),
            marked: HashSet::new(),
            search: None,
            sidebar_selected: 0,
            sidebar_scroll: Cell::new(0),
            focus: Focus::List,
            reader: None,
            reader_width: Cell::new(80),
            wide: Cell::new(false),
            reader_rows: Cell::new(20),
            list_rows: Cell::new(20),
            renders: 0,
            moved_at: None,
            composer: None,
            setup: None,
            prompt: None,
            tagging: Vec::new(),
            change: None,
            agent_socket: None,
            agents: None,
            signing_in: false,
            agent_asks: VecDeque::new(),
            next_ask: 0,
            agent_reads: Vec::new(),
            agent_downloads: HashMap::new(),
            agent_fetched: VecDeque::new(),
            agent_spent: VecDeque::new(),
            picker: None,
            confirm: None,
            confirm_at: Instant::now(),
            deletes_waiting: VecDeque::new(),
            help: None,
            status: None,
            pending: None,
            local_flags: HashMap::new(),
            local_removed: HashMap::new(),
            appends: HashMap::new(),
            next_append: 0,
            notifier: Notifier::default(),
            notifications,
            title_unread: None,
            pending_g: false,
            quit: false,
            demo,
            edit_request: false,
            clipboard: None,
            #[cfg(test)]
            test_rx: None,
            #[cfg(test)]
            test_sent: Default::default(),
        };
        for account in accounts {
            let password = passwords.remove(&account.id());
            app.add_account(account, password);
        }
        if app.accounts.is_empty() {
            app.setup = Some(Setup::default());
        } else {
            let inbox = app.accounts[0].inbox();
            app.place = Place::Folder {
                account: 0,
                folder: inbox,
            };
            app.sync_sidebar();
        }
        app.reload();
        app
    }

    /// Starts an account: its folders from the cache, its worker.
    fn add_account(&mut self, account: Account, password: Option<Secret>) -> usize {
        let id = account.id();
        let account = Arc::new(account);
        if account.protocol == Protocol::Pop3 {
            let _ = self.store.set_folders(&id, &local_folders());
        }
        let folders = self.store.folders(&id).unwrap_or_default();
        let worker = if self.demo {
            None
        } else {
            match net::tls_config(account.certificate.as_deref()) {
                Ok(tls) => {
                    let creds = Credentials::new(account.clone(), self.keychain);
                    if let Some(secret) = password {
                        let creds = creds.clone();
                        // Set before the worker can ask for it.
                        futures::executor::block_on(creds.set(secret));
                    }
                    let endpoints = Endpoints::of(&account, tls);
                    let tx = Mutex::new(self.tx.clone());
                    let account_id = id.clone();
                    let sink: worker::Sink = Arc::new(move |event| {
                        if let Ok(tx) = tx.lock() {
                            let _ = tx.send(AppEvent::Worker {
                                account: account_id.clone(),
                                event,
                            });
                        }
                    });
                    Some(worker::spawn(creds, endpoints, sink))
                }
                Err(e) => {
                    self.error(format!("{}: {e:#}", account.shown_name()));
                    None
                }
            }
        };
        // With no worker nothing connects: said, so nothing waits on it.
        let state = match &worker {
            None if !self.demo => State::Offline("can't connect: see the error at start".into()),
            _ => State::Connecting,
        };
        self.accounts.push(AccountState {
            account,
            id,
            worker,
            state,
            folders,
            expanded: true,
            syncing: HashSet::new(),
            older_asked: HashSet::new(),
            last_check: None,
            checks: 0,
            popped_once: false,
            pop_again: false,
            password_put_off: false,
            waiting: Default::default(),
        });
        self.accounts.len() - 1
    }

    pub async fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        mut rx: UnboundedReceiver<AppEvent>,
    ) -> Result<()> {
        let mut keys = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        #[cfg(unix)]
        quit_on_hangup(self.tx.clone())?;
        notify::send(notify::SAVE_TITLE);
        self.listen_for_agents();
        for i in 0..self.accounts.len() {
            self.redo_deletes(i);
            self.check(i, true);
        }
        loop {
            terminal.draw(|frame| crate::ui::draw(frame, self))?;
            self.after_draw();
            if self.quit {
                break;
            }
            if std::mem::take(&mut self.edit_request) {
                // The editor gets the terminal: key reading stops, the screen
                // goes back to normal, and both come back after.
                drop(keys);
                self.run_editor();
                *terminal = ratatui::init();
                let _ =
                    crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
                terminal.clear()?;
                keys = EventStream::new();
                continue;
            }
            tokio::select! {
                event = keys.next() => match event {
                    Some(Ok(event)) => {
                        // Whatever else is already waiting is read with it:
                        // a paste that comes as keystrokes arrives at once.
                        let mut events = vec![event];
                        while events.len() < MAX_PROMPT
                            && crossterm::event::poll(Duration::ZERO).unwrap_or(false)
                        {
                            match crossterm::event::read() {
                                Ok(event) => events.push(event),
                                Err(_) => break,
                            }
                        }
                        self.on_terminal_events(events);
                    }
                    Some(Err(_)) | None => self.quit = true,
                },
                Some(event) = rx.recv() => {
                    self.on_event(event);
                    while let Ok(event) = rx.try_recv() {
                        self.on_event(event);
                    }
                }
                _ = tick.tick() => self.on_tick(),
            }
        }
        notify::send(notify::RESTORE_TITLE);
        Ok(())
    }

    /// After a frame: a message laid out for another width is laid out
    /// again for this one.
    pub(crate) fn after_draw(&mut self) {
        let width = self.reader_width.get();
        if let Some(reader) = &self.reader
            && reader.raw.is_some()
            && reader.pending.is_none()
            && reader.width != width
        {
            self.render();
        }
    }

    // Messages.

    pub fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            error: false,
            at: Instant::now(),
        });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            error: true,
            at: Instant::now(),
        });
    }

    fn on_tick(&mut self) {
        // Agents turned off in settings.toml (`agents_allowed`): stopped
        // here, once what was refused has been answered.
        if !self.settings.agents && self.agents.is_some() {
            self.listen_for_agents();
        }
        self.ask_agents();
        if let Some(status) = &self.status {
            let keep = if status.error { ERROR_FOR } else { STATUS_FOR };
            if status.at.elapsed() >= keep {
                self.status = None;
            }
        }
        // The message under a resting cursor shows beside the list.
        if let Some(at) = self.moved_at
            && at.elapsed() >= PREVIEW_AFTER
        {
            self.moved_at = None;
            if self.preview_shown() {
                self.open_selected(false);
            }
        }
        // …and is marked read once it has been shown a moment.
        if let Some(reader) = &self.reader
            && !reader.marked_read
            && reader.body.is_some()
            && reader.opened_at.elapsed() >= PREVIEW_READ_AFTER
        {
            self.mark_reader_read();
        }
        if let Some(alert) = self.notifier.due(tokio::time::Instant::now())
            && let Some(code) =
                notify::escape(self.notifications, &alert, now() as u64, notify::in_tmux())
        {
            notify::send(&code);
        }
        let every = Duration::from_secs(self.settings.check_every());
        for i in 0..self.accounts.len() {
            if self.accounts[i]
                .last_check
                .is_some_and(|at| at.elapsed() >= every)
            {
                self.check(i, false);
            }
        }
        let unread = self.unread_inboxes();
        if self.title_unread != Some(unread) {
            self.title_unread = Some(unread);
            notify::send(&notify::title(unread));
        }
    }

    /// Unread mail in every inbox, for the window's title.
    pub fn unread_inboxes(&self) -> u32 {
        self.accounts
            .iter()
            .filter_map(|a| a.folder_for(Role::Inbox))
            .map(|f| f.unread)
            .sum()
    }

    /// Looks for new mail in account `i`: its inbox, the folder open, and
    /// the folders' counts (`all`, or now and then).
    fn check(&mut self, i: usize, all: bool) {
        if self.demo {
            return;
        }
        self.accounts[i].last_check = Some(Instant::now());
        if self.accounts[i].is_pop() {
            self.pop_check(i);
            return;
        }
        let inbox = self.accounts[i].inbox();
        // Every folder's counts take a command each: not every time.
        if all || self.accounts[i].checks.is_multiple_of(FOLDERS_EVERY) {
            self.send(i, Request::Folders);
        }
        self.accounts[i].checks = self.accounts[i].checks.wrapping_add(1);
        self.sync_folder(i, &inbox);
        if let Place::Folder { account, folder } = self.place.clone()
            && account == i
            && folder != inbox
        {
            self.sync_folder(i, &folder);
        }
    }

    fn pop_check(&mut self, i: usize) {
        if self.accounts[i].syncing.contains("INBOX") {
            // One at a time: two would download the same mail.
            self.accounts[i].pop_again = true;
            return;
        }
        let id = self.accounts[i].id.clone();
        let seen = self.store.pop_seen(&id).unwrap_or_default();
        let remove = self.store.pop_removals(&id).unwrap_or_default();
        let count = self.settings.fetch_count();
        // Unsure, it's not a first check: that would leave mail behind.
        let first_check = !self.store.pop_checked(&id).unwrap_or(true);
        self.accounts[i].syncing.insert("INBOX".into());
        self.send(
            i,
            Request::Pop {
                seen,
                remove,
                count,
                first_check,
            },
        );
    }

    fn sync_folder(&mut self, i: usize, folder: &str) {
        if self.demo || self.accounts[i].is_pop() {
            return;
        }
        let known = self
            .store
            .known(&self.accounts[i].id, folder)
            .unwrap_or_default();
        self.accounts[i].syncing.insert(folder.to_string());
        let count = self.settings.fetch_count();
        self.send(
            i,
            Request::Sync {
                folder: folder.to_string(),
                known,
                count,
            },
        );
    }

    fn send(&self, i: usize, request: Request) {
        #[cfg(test)]
        self.test_sent.borrow_mut().push((i, request.clone()));
        if let Some(worker) = &self.accounts[i].worker {
            // Counted at once, until the worker says how many wait: one
            // still on its way to a worker busy with a long download would
            // otherwise not count when quitting asks.
            if request.changes() && !self.accounts[i].is_pop() {
                let waiting = &self.accounts[i].waiting;
                waiting.set(waiting.get() + 1);
            }
            let _ = worker.tx.send(request);
        }
    }

    fn account_index(&self, id: &str) -> Option<usize> {
        self.accounts.iter().position(|a| a.id == id)
    }

    // Events.

    fn on_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Agent { request, reply } => {
                // Answered only while agents are let in: turned off in
                // another tuimail, or in settings.toml, counts too.
                if self.agents_allowed() {
                    self.on_agent(request, reply);
                } else {
                    let _ = reply.send(Err("The user turned AI agents off".into()));
                }
            }
            AppEvent::Worker { account, event } => {
                if let Some(i) = self.account_index(&account) {
                    self.on_worker(i, event);
                }
            }
            AppEvent::Sent {
                account,
                result,
                needs_password,
                sent,
            } => self.on_sent(&account, result, needs_password, sent),
            AppEvent::Rendered {
                generation,
                width,
                parsed,
                body,
            } => {
                let Some(reader) = self.reader.as_mut() else {
                    return;
                };
                if reader.pending != Some(generation) {
                    return;
                }
                reader.pending = None;
                reader.width = width;
                match (parsed, body) {
                    (Some(parsed), Some(body)) => {
                        reader.parsed = Some(parsed);
                        reader.body = Some(body);
                        reader.error = None;
                    }
                    _ => {
                        reader.error = Some("This message can't be read".into());
                        reader.width = width;
                    }
                }
            }
            AppEvent::Drafted {
                account,
                kind,
                draft,
                replying,
                draft_of,
            } => match (draft, self.account_index(&account)) {
                // A reply made ready late never takes the place of a
                // message being written, or of the one a question is about.
                (Some(_), _) if self.composer.is_some() || self.confirm.is_some() => {
                    self.info("A message is open: finish it, then start the reply again")
                }
                (Some(draft), Some(account)) => {
                    let mut composer = Composer::new(account, kind, draft);
                    composer.replying = replying;
                    composer.draft_of = draft_of;
                    self.composer = Some(composer);
                }
                _ => self.error("That message can't be read"),
            },
            AppEvent::Extracted {
                open,
                confirmed,
                result,
            } => match result {
                // What's opened without a yes is checked again, by the name
                // it was saved under.
                Ok(path)
                    if open
                        && !confirmed
                        && !path
                            .file_name()
                            .is_some_and(|n| tools::safe_to_open(&n.to_string_lossy())) =>
                {
                    self.error(format!(
                        "Not opened: {} is a kind of file that asks first",
                        crate::config::shown(&path)
                    ))
                }
                Ok(path) if open => {
                    if let Err(e) = tools::open_externally(&path.to_string_lossy()) {
                        self.error(format!("Couldn't open it: {e}"));
                    }
                }
                Ok(path) => self.info(format!("Saved {}", crate::config::shown(&path))),
                Err(e) => self.error(e),
            },
            AppEvent::Previews { account, previews } => {
                let Some(i) = self.account_index(&account) else {
                    return;
                };
                for (folder, uid, preview) in previews {
                    let _ = self.store.set_preview(&account, &folder, uid, &preview);
                    if let Some(item) = self
                        .items
                        .iter_mut()
                        .find(|it| it.key() == (i, folder.clone(), uid))
                    {
                        item.message.preview = text::one_line(&preview, 200);
                    }
                }
            }
            AppEvent::Checked { result } => self.on_checked(result),
            AppEvent::Failed { error } => self.error(error),
            AppEvent::SignInOpened { url } => self.open_sign_in(&url),
            AppEvent::SignedIn { account, result } => self.on_signed_in(&account, result),
            AppEvent::Quit => self.quit = true,
        }
    }

    fn on_worker(&mut self, i: usize, event: worker::Event) {
        use worker::Event as E;
        match event {
            E::State(state) => {
                let name = self.account_name(i);
                if let State::NeedPassword { refused } = &state {
                    let refused = *refused;
                    let signs_in = crate::oauth::built_in(&self.accounts[i].account);
                    let from_command = self.accounts[i].account.password == PasswordFrom::Command
                        || self.accounts[i].account.auth == account::Auth::Oauth2;
                    if signs_in {
                        self.error(format!(
                            "{name} needs you to sign in: P on its row opens your browser"
                        ));
                    } else if from_command {
                        self.error(format!(
                            "{name}: the server refused password_command's password; R tries again"
                        ));
                    } else if self.prompt.is_none()
                        && self.composer.is_none()
                        && self.setup.is_none()
                        && !self.accounts[i].password_put_off
                    {
                        self.prompt = Some(Prompt::new(PromptKind::Password(i), ""));
                        if refused {
                            self.error(format!("{name}: the server refused the password"));
                        }
                    }
                }
                if !matches!(state, State::Online | State::Connecting) {
                    self.accounts[i].syncing.clear();
                }
                self.accounts[i].state = state;
            }
            E::Folders(folders) => {
                let id = self.accounts[i].id.clone();
                if self.store.set_folders(&id, &folders).is_ok() {
                    for f in &folders {
                        let _ = self.store.set_counts(&id, &f.name, f.unread, f.total);
                    }
                }
                self.accounts[i].folders = self.store.folders(&id).unwrap_or_default();
                // Deletes waiting in a folder the server no longer has went
                // with it: not asked for again at every start.
                for (folder, _, uids) in self.store.pending_deletes(&id).unwrap_or_default() {
                    if !folders.iter().any(|f| f.name == folder) {
                        let _ = self.store.done_deletes(&id, &folder, &uids);
                    }
                }
                if let Place::Folder { account, folder } = &self.place
                    && *account == i
                    && !self.accounts[i].folders.iter().any(|f| &f.name == folder)
                {
                    let inbox = self.accounts[i].inbox();
                    self.place = Place::Folder {
                        account: i,
                        folder: inbox,
                    };
                    self.reload();
                }
            }
            E::Synced(synced) => self.on_synced(i, synced),
            E::Older {
                folder,
                headers,
                low,
            } => {
                let id = self.accounts[i].id.clone();
                let _ = self.store.add_headers(&id, &folder, &headers);
                let mut known = self.store.known(&id, &folder).unwrap_or_default();
                if known.high > 0 && low < known.low {
                    known.low = low;
                    let _ = self.store.set_known(&id, &folder, known);
                }
                // Asked again only if more came back.
                if !headers.is_empty() {
                    self.accounts[i].older_asked.remove(&folder);
                }
                if self.shows(i, &folder) {
                    self.reload();
                }
            }
            E::Body {
                folder,
                uid,
                result,
            } => {
                let id = self.accounts[i].id.clone();
                let key = (i, folder.clone(), uid);
                self.agent_downloads.remove(&key);
                match result {
                    Ok(Some(raw)) => {
                        let _ = self.store.save_body(&id, &folder, uid, &raw);
                        for read in self.agent_reads_for(&key) {
                            self.agent_render(read.key, raw.clone(), read.max, read.reply);
                        }
                        let raw = Arc::new(raw);
                        self.make_previews(i, vec![(folder.clone(), uid, raw.clone())]);
                        if self.reader.as_ref().is_some_and(|r| r.key() == key) {
                            if let Some(reader) = self.reader.as_mut() {
                                reader.raw = Some(raw.clone());
                            }
                            self.render();
                        }
                        if let Some((pending_key, _)) = &self.pending
                            && *pending_key == key
                            && let Some((_, pending)) = self.pending.take()
                        {
                            self.draft_from(key.clone(), raw, pending);
                        }
                    }
                    Ok(None) => {
                        let gone = "This message is gone from the server";
                        for read in self.agent_reads_for(&key) {
                            let _ = read.reply.send(Err(gone.into()));
                        }
                        if let Some(reader) = self.reader.as_mut().filter(|r| r.key() == key) {
                            reader.error = Some(gone.into());
                        }
                        self.drop_pending(&key, gone);
                    }
                    Err(e) => {
                        for read in self.agent_reads_for(&key) {
                            let _ = read.reply.send(Err(e.clone()));
                        }
                        if let Some(reader) = self.reader.as_mut().filter(|r| r.key() == key) {
                            reader.error = Some(e.clone());
                        }
                        self.drop_pending(&key, &e);
                    }
                }
            }
            E::Searched {
                folder,
                query,
                headers,
            } => {
                let id = self.accounts[i].id.clone();
                let _ = self.store.add_headers(&id, &folder, &headers);
                if self.search.as_deref() == Some(query.as_str()) {
                    self.reload();
                    self.info(format!(
                        "The server found {} in {}",
                        plural(headers.len(), "message"),
                        self.folder_name(i, &folder)
                    ));
                }
            }
            E::Done { folder } => self.folder_changed(i, &folder),
            E::Moved {
                folder,
                uidvalidity,
                uids,
                to,
                to_uidvalidity,
                floor,
            } => {
                // Where the move put them, for undoing it.
                if floor != 0 {
                    let id = self.accounts[i].id.clone();
                    let _ = (self.store).set_moved(
                        &id,
                        &folder,
                        uidvalidity,
                        &uids,
                        &to,
                        to_uidvalidity,
                        floor,
                    );
                }
            }
            E::MovedBack {
                folder,
                to,
                moved,
                asked,
            } => {
                // Off the list here: the next check of `to` brings them
                // back there.
                let id = self.accounts[i].id.clone();
                let unread = self.unread_among(i, &folder, &moved);
                let _ = self.store.remove(&id, &folder, &moved);
                let keys: Vec<_> = (moved.iter()).map(|u| (i, folder.clone(), *u)).collect();
                for key in &keys {
                    self.local_removed.insert(key.clone(), Instant::now());
                }
                self.note_removed(i, &folder, moved.len(), unread);
                self.take_off_list(&keys);
                self.folder_changed(i, &folder);
                self.folder_changed(i, &to);
                if moved.len() < asked {
                    self.error(format!(
                        "{} of them weren't in {} any more, so stay where they are",
                        asked - moved.len(),
                        self.folder_name(i, &folder)
                    ));
                }
            }
            E::Waiting(n) => self.accounts[i].waiting.set(n),
            E::Deleted { folder, uids, all } => {
                let id = self.accounts[i].id.clone();
                if all {
                    let _ = self.store.done_deletes(&id, &folder, &uids);
                } else {
                    self.info(
                        "Deleted here; on the server they stay marked until another program's deletions there are done",
                    );
                }
            }
            E::Appended { folder, id, error } => {
                match (self.appends.remove(&id), error) {
                    (Some(Append::Draft { draft_of }), None) => {
                        if self.composer.as_ref().is_some_and(|c| c.saving == Some(id)) {
                            self.composer = None;
                        }
                        self.replace_draft(draft_of);
                        self.info("Saved in Drafts");
                    }
                    (Some(Append::Draft { .. }), Some(e)) => {
                        if let Some(composer) =
                            self.composer.as_mut().filter(|c| c.saving == Some(id))
                        {
                            composer.saving = None;
                            composer.sending = false;
                        }
                        self.error(format!("Not saved in Drafts: {e}"));
                    }
                    (Some(Append::SentCopy), Some(e)) => {
                        self.error(format!("Sent, but the copy in Sent wasn't kept: {e}"));
                    }
                    _ => {}
                }
                self.folder_changed(i, &folder);
            }
            E::Headers { folder, headers } => {
                let id = self.accounts[i].id.clone();
                let _ = self.store.add_headers(&id, &folder, &headers);
                if self.shows(i, &folder) {
                    self.reload();
                }
            }
            E::Failed { what, error } => {
                self.accounts[i].syncing.clear();
                if std::mem::take(&mut self.accounts[i].pop_again) {
                    self.pop_check(i);
                }
                if error == "offline" {
                    return;
                }
                let name = self.account_name(i);
                self.error(format!("{name}: {what} failed: {error}"));
            }
            E::Changed { folder } => self.sync_folder(i, &folder),
            E::Popped(popped) => self.on_popped(i, popped),
        }
    }

    /// A reply, forward or draft waiting for this message's text, which
    /// didn't come: dropped, saying why, so it doesn't open much later.
    fn drop_pending(&mut self, key: &(usize, String, u32), why: &str) {
        if self.pending.as_ref().is_some_and(|(k, _)| k == key) {
            self.pending = None;
            self.error(format!("Not opened in the composer: {why}"));
        }
    }

    /// The server changed `folder` (mail moved or added there).
    fn folder_changed(&mut self, i: usize, folder: &str) {
        if !self.accounts[i].folders.iter().any(|f| f.name == folder) {
            // A folder just made: list them again.
            self.send(i, Request::Folders);
        } else if self.shows(i, folder) {
            self.sync_folder(i, folder);
        }
    }

    /// Makes the list's previews of these messages in the background.
    fn make_previews(&self, i: usize, messages: Vec<(String, u32, Arc<Vec<u8>>)>) {
        if messages.is_empty() {
            return;
        }
        let account = self.accounts[i].id.clone();
        let tx = self.tx.clone();
        mail::in_background(
            move || {
                messages
                    .into_iter()
                    .map(|(folder, uid, raw)| (folder, uid, mail::preview_of(&raw)))
                    .collect::<Vec<_>>()
            },
            move |previews| {
                if let Some(previews) = previews {
                    let _ = tx.send(AppEvent::Previews { account, previews });
                }
            },
        );
    }

    fn on_synced(&mut self, i: usize, s: worker::Synced) {
        let id = self.accounts[i].id.clone();
        self.accounts[i].syncing.remove(&s.folder);
        let before = self.store.known(&id, &s.folder).unwrap_or_default();
        if s.reset {
            let _ = self.store.reset_folder(&id, &s.folder);
            // Its UIDs mean other messages now: nothing may act on the old.
            let stale = |key: &(usize, String, u32)| key.0 == i && key.1 == s.folder;
            if self.reader.as_ref().is_some_and(|r| stale(&r.key())) {
                self.reader = None;
                if self.focus == Focus::Reader {
                    self.focus = Focus::List;
                }
            }
            self.marked.retain(|k| !stale(k));
            if self.pending.as_ref().is_some_and(|(k, _)| stale(k)) {
                self.pending = None;
            }
        }
        let _ = self.store.add_headers(&id, &s.folder, &s.new);
        // People who write to you are offered when you write.
        let senders: Vec<Addr> = s.new.iter().map(|h| h.summary.from.clone()).collect();
        let _ = self.store.remember(&senders, false, now());
        if let Some((low, high)) = s.checked {
            let present: HashSet<u32> = s.flags.iter().map(|(u, _)| *u).collect();
            let cached = self
                .store
                .uids(&id, &s.folder, low, high)
                .unwrap_or_default();
            let cached_set: HashSet<u32> = cached.iter().copied().collect();
            let gone: Vec<u32> = cached
                .into_iter()
                .filter(|u| !present.contains(u))
                .collect();
            let _ = self.store.remove(&id, &s.folder, &gone);
            // Messages the server has that the cache lost (a move or delete
            // the server refused) come back, unless moved away here just now.
            self.local_removed
                .retain(|_, at| at.elapsed() < LOCAL_FLAGS_WIN);
            let mut missing: Vec<u32> = present
                .iter()
                .copied()
                .filter(|u| !cached_set.contains(u))
                .filter(|u| !self.local_removed.contains_key(&(i, s.folder.clone(), *u)))
                .collect();
            missing.sort_unstable();
            missing.truncate(1000);
            if !missing.is_empty() {
                self.send(
                    i,
                    Request::Headers {
                        folder: s.folder.clone(),
                        uids: missing,
                        uidvalidity: s.uidvalidity,
                    },
                );
            }
            // Flags changed here a moment ago stay as they are.
            self.local_flags
                .retain(|_, at| at.elapsed() < LOCAL_FLAGS_WIN);
            let flags: Vec<(u32, Flags)> = s
                .flags
                .iter()
                .filter(|(uid, _)| !self.local_flags.contains_key(&(i, s.folder.clone(), *uid)))
                .cloned()
                .collect();
            let _ = self.store.set_flags(&id, &s.folder, &flags);
        }
        let _ = self.store.set_known(
            &id,
            &s.folder,
            Known {
                uidvalidity: s.uidvalidity,
                low: s.low,
                high: s.high,
            },
        );
        let _ = self.store.set_counts(&id, &s.folder, s.unread, s.total);
        if let Some(f) = self.accounts[i]
            .folders
            .iter_mut()
            .find(|f| f.name == s.folder)
        {
            f.unread = s.unread;
            f.total = s.total;
        }
        // New unread mail in an inbox checked before is announced.
        let inbox = self.accounts[i].inbox() == s.folder;
        if inbox && before.high > 0 && !s.reset {
            let fresh: Vec<&store::Header> = s
                .new
                .iter()
                .filter(|h| !h.flags.seen && h.uid > before.high)
                .collect();
            self.announce(i, fresh.iter().map(|h| &h.summary));
        }
        if self.shows(i, &s.folder) {
            self.reload();
        }
    }

    fn on_popped(&mut self, i: usize, popped: worker::Popped) {
        let id = self.accounts[i].id.clone();
        self.accounts[i].syncing.clear();
        let first = !self.accounts[i].popped_once && !self.store.pop_checked(&id).unwrap_or(true);
        let mut summaries = Vec::new();
        let mut added = Vec::new();
        let seen = self.store.pop_seen(&id).unwrap_or_default();
        let mut previews = Vec::new();
        for (pop_id, raw) in popped.new {
            // Downloaded already, by a check that ran at the same time.
            if seen.contains(&pop_id) {
                continue;
            }
            if let Ok(uid) =
                self.store
                    .add_local(&id, "INBOX", &raw, Flags::default(), Some(&pop_id))
            {
                added.push(uid);
                summaries.push(mail::summarize(&raw));
                previews.push(("INBOX".to_string(), uid, Arc::new(raw)));
            }
        }
        self.make_previews(i, previews);
        let senders: Vec<Addr> = summaries.iter().map(|s| s.from.clone()).collect();
        let _ = self.store.remember(&senders, false, now());
        let _ = self.store.pop_skip_all(&id, &popped.skipped);
        let mut keep = popped.on_server.clone();
        for gone in &popped.deleted {
            keep.remove(gone);
        }
        let _ = self.store.pop_forget(&id, &keep);
        let _ = self.store.set_pop_checked(&id);
        self.accounts[i].popped_once = true;
        if !first {
            self.announce(i, summaries.iter());
        }
        let mut again = popped.more || std::mem::take(&mut self.accounts[i].pop_again);
        if !self.accounts[i].account.leave_on_server && !added.is_empty() {
            // Taken off the server at the next check, now that they're kept.
            let _ = self.store.mark_pop_removal(&id, "INBOX", &added);
            again = true;
        }
        self.refresh_counts(i, "INBOX");
        if self.shows(i, "INBOX") {
            self.reload();
        }
        if again {
            self.pop_check(i);
        }
    }

    /// Tells the user about new mail, if notifications are on.
    fn announce<'a>(&mut self, i: usize, summaries: impl Iterator<Item = &'a mail::Summary>) {
        let account = if self.accounts.len() > 1 {
            self.account_name(i)
        } else {
            String::new()
        };
        for s in summaries {
            self.notifier.add(notify::Note {
                account: account.clone(),
                from: s.from.short().to_string(),
                subject: s.subject.clone(),
            });
        }
    }

    /// Counts a folder kept here (POP3) again.
    fn refresh_counts(&mut self, i: usize, folder: &str) {
        let id = self.accounts[i].id.clone();
        if let Ok((unread, total)) = self.store.count(&id, folder) {
            let _ = self.store.set_counts(&id, folder, unread, total);
            if let Some(f) = self.accounts[i]
                .folders
                .iter_mut()
                .find(|f| f.name == folder)
            {
                f.unread = unread;
                f.total = total;
            }
        }
    }

    /// Whether the list shows account `i`'s `folder`.
    fn shows(&self, i: usize, folder: &str) -> bool {
        match &self.place {
            Place::Folder { account, folder: f } => *account == i && f == folder,
            Place::AllInboxes => self.accounts[i].inbox() == folder,
        }
    }

    /// Deletes for good the server hadn't confirmed when tuimail last
    /// quit: asked for again, unless their folder was made again since
    /// (then those messages are gone anyway).
    fn redo_deletes(&mut self, i: usize) {
        if self.demo || self.accounts[i].is_pop() || self.accounts[i].worker.is_none() {
            return;
        }
        let id = self.accounts[i].id.clone();
        for (folder, uidvalidity, uids) in self.store.pending_deletes(&id).unwrap_or_default() {
            let now = self.uidvalidity(i, &folder);
            let gone = !self.accounts[i].folders.iter().any(|f| f.name == folder);
            if gone || (now != 0 && uidvalidity != 0 && now != uidvalidity) {
                let _ = self.store.done_deletes(&id, &folder, &uids);
                continue;
            }
            self.send(
                i,
                Request::Delete {
                    folder,
                    uids,
                    uidvalidity,
                },
            );
        }
    }

    /// `q` or Ctrl-C: quits, asking first if changes haven't reached the
    /// server yet (they would be lost).
    fn ask_quit(&mut self) {
        let waiting: usize =
            self.accounts.iter().map(|a| a.waiting.get()).sum::<usize>() + self.appends.len();
        if waiting > 0 {
            self.confirm(Confirm::Quit { waiting });
        } else {
            self.quit = true;
        }
    }

    /// What `folder`'s UIDs mean: the UIDVALIDITY the cache knows it by. 0
    /// for local folders and folders not checked yet.
    fn uidvalidity(&self, account: usize, folder: &str) -> u32 {
        self.accounts.get(account).map_or(0, |a| {
            self.store.known(&a.id, folder).map_or(0, |k| k.uidvalidity)
        })
    }

    /// Whether UIDs read in `folder` under `uidvalidity` still mean the
    /// same messages; if not, says so (nothing is done to them).
    fn still_means(&mut self, account: usize, folder: &str, uidvalidity: u32) -> bool {
        let now = self.uidvalidity(account, folder);
        let same = uidvalidity == 0 || now == 0 || now == uidvalidity;
        if !same {
            self.error("That folder was made again on the server: nothing was done");
        }
        same
    }

    /// A message to hold on to for later.
    fn kept(&self, key: &(usize, String, u32)) -> Kept {
        Kept {
            account: self.accounts[key.0].id.clone(),
            folder: key.1.clone(),
            uid: key.2,
            uidvalidity: self.uidvalidity(key.0, &key.1),
        }
    }

    /// A held message as a key, if its account is still here and its UID
    /// still means it.
    fn resolve(&mut self, kept: &Kept) -> Option<(usize, String, u32)> {
        let i = self.account_index(&kept.account)?;
        self.still_means(i, &kept.folder, kept.uidvalidity)
            .then(|| (i, kept.folder.clone(), kept.uid))
    }

    /// Deletes the saved draft a sent or saved message replaces: in its own
    /// account, only if it's still that message.
    fn replace_draft(&mut self, draft_of: Option<Kept>) {
        if let Some((i, folder, uid)) = draft_of.and_then(|k| self.resolve(&k)) {
            self.delete_forever(i, &folder, &[uid]);
        }
    }

    /// What the sidebar, the list's title and messages call account `i`:
    /// its name, or its address when another account goes by the same name
    /// (two accounts both called "gmail.com").
    pub fn account_name(&self, i: usize) -> String {
        let Some(a) = self.accounts.get(i) else {
            return String::new();
        };
        let name = a.account.shown_name();
        let shared = self
            .accounts
            .iter()
            .enumerate()
            .any(|(j, b)| j != i && b.account.shown_name() == name);
        if shared {
            text::one_line(&a.account.email, 60)
        } else {
            name
        }
    }

    /// The list's `[tag]` for account `i` in All inboxes, where there's
    /// little room: its short name, or its whole name where another
    /// account's short name is the same.
    pub fn account_tag(&self, i: usize) -> String {
        let Some(a) = self.accounts.get(i) else {
            return String::new();
        };
        let short = a.account.short_name();
        let shared = self
            .accounts
            .iter()
            .enumerate()
            .any(|(j, b)| j != i && b.account.short_name() == short);
        if shared { self.account_name(i) } else { short }
    }

    pub fn folder_name(&self, i: usize, folder: &str) -> String {
        self.accounts
            .get(i)
            .and_then(|a| a.folders.iter().find(|f| f.name == folder))
            .map(folder_label)
            .unwrap_or_else(|| {
                text::one_line(
                    &crate::utf7::decode(folder).unwrap_or_else(|| folder.to_string()),
                    60,
                )
            })
    }

    // The list.

    /// Fills the list from the cache for the place shown (or the search),
    /// keeping the cursor on the same message.
    pub fn reload(&mut self) {
        // The cursor stays on its message, except at the top with nothing
        // open: it stays at the top, on the newest mail.
        let current = if self.selected == 0 && self.reader.is_none() {
            None
        } else {
            self.items.get(self.selected).map(Item::key)
        };
        let mut items = Vec::new();
        let targets: Vec<(usize, Option<String>)> = match &self.place {
            Place::Folder { account, folder } => {
                if self.search.is_some() {
                    vec![(*account, None)]
                } else {
                    vec![(*account, Some(folder.clone()))]
                }
            }
            Place::AllInboxes => (0..self.accounts.len())
                .map(|i| (i, Some(self.accounts[i].inbox())))
                .collect(),
        };
        for (i, folder) in targets {
            let id = &self.accounts[i].id;
            let found = match (&self.search, folder) {
                (Some(query), Some(folder)) => {
                    self.store.search(id, Some(&folder), query, MAX_LISTED)
                }
                (Some(query), None) => self.store.search(id, None, query, MAX_LISTED),
                (None, Some(folder)) => self.store.messages(id, &folder, MAX_LISTED),
                (None, None) => Ok(Vec::new()),
            };
            items.extend(found.unwrap_or_default().into_iter().map(|message| Item {
                account: i,
                message,
                thread: Thread::default(),
            }));
        }
        if matches!(self.place, Place::AllInboxes) {
            items.sort_by_key(|it| std::cmp::Reverse(it.message.summary.date));
        }
        // In conversations; what a search finds stays newest first.
        if self.settings.threads && self.search.is_none() {
            let summaries: Vec<&mail::Summary> =
                items.iter().map(|it| &it.message.summary).collect();
            // Where each is (its account and folder, numbered), and its UID.
            let mut places: Vec<(usize, &str)> = Vec::new();
            let arrived: Vec<(usize, u32)> = (items.iter())
                .map(|it| {
                    let place = (it.account, it.message.folder.as_str());
                    let n = places.iter().position(|p| *p == place).unwrap_or_else(|| {
                        places.push(place);
                        places.len() - 1
                    });
                    (n, it.message.uid)
                })
                .collect();
            let order = crate::threads::arrange_arrived(&summaries, &arrived);
            let mut slots: Vec<Option<Item>> = items.into_iter().map(Some).collect();
            items = order
                .into_iter()
                .filter_map(|placed| {
                    let mut item = slots.get_mut(placed.index)?.take()?;
                    item.thread = Thread {
                        tree: placed.tree,
                        same_subject: placed.same_subject,
                    };
                    Some(item)
                })
                .collect();
        }
        self.items = items;
        self.selected = current
            .and_then(|key| self.items.iter().position(|it| it.key() == key))
            .unwrap_or(self.selected)
            .min(self.items.len().saturating_sub(1));
        if !self.marked.is_empty() {
            // Looked up in one set: thousands marked in a list of thousands.
            let listed: HashSet<_> = self.items.iter().map(Item::key).collect();
            self.marked.retain(|key| listed.contains(key));
        }
    }

    /// Opens a folder (or every inbox) in the list.
    pub fn open_place(&mut self, place: Place) {
        let same = self.place == place;
        self.place = place.clone();
        self.search = None;
        if !same {
            self.selected = 0;
            self.list_scroll.set(0);
            self.marked.clear();
        }
        self.reload();
        match place {
            Place::Folder { account, folder } => {
                if self.accounts[account].is_pop() {
                    return;
                }
                self.sync_folder(account, &folder);
            }
            Place::AllInboxes => {
                for i in 0..self.accounts.len() {
                    let inbox = self.accounts[i].inbox();
                    self.sync_folder(i, &inbox);
                }
            }
        }
    }

    pub fn selected_item(&self) -> Option<&Item> {
        self.items.get(self.selected)
    }

    fn select(&mut self, i: usize) {
        let i = i.min(self.items.len().saturating_sub(1));
        if i != self.selected {
            self.selected = i;
            self.moved_at = Some(Instant::now());
        }
        self.load_older_if_near_end();
    }

    /// Fetches older mail when the cursor nears the end of what's cached.
    fn load_older_if_near_end(&mut self) {
        if self.search.is_some() || self.selected + NEAR_END < self.items.len() {
            return;
        }
        let Place::Folder { account, folder } = self.place.clone() else {
            return;
        };
        if self.accounts[account].is_pop() || self.accounts[account].older_asked.contains(&folder) {
            return;
        }
        let known = self
            .store
            .known(&self.accounts[account].id, &folder)
            .unwrap_or_default();
        if known.low > 1 {
            self.accounts[account].older_asked.insert(folder.clone());
            let count = self.settings.fetch_count();
            self.send(
                account,
                Request::Older {
                    folder,
                    below: known.low,
                    count,
                },
            );
        }
    }

    /// Whether the message under the cursor shows beside the list.
    pub fn preview_shown(&self) -> bool {
        self.settings.preview && self.wide.get()
    }

    /// Opens the message under the cursor in the reader; `enter` when asked
    /// for (marked read at once), not just previewed.
    fn open_selected(&mut self, enter: bool) {
        let Some(item) = self.selected_item().cloned() else {
            return;
        };
        let key = item.key();
        // A draft opens in the composer.
        if enter && self.role_of(item.account, &item.message.folder) == Some(Role::Drafts) {
            return self.start_from(key, Pending::EditDraft);
        }
        // Open already; Enter downloads again one that failed.
        if let Some(reader) = &mut self.reader
            && reader.key() == key
            && !(enter && reader.raw.is_none() && reader.error.is_some())
        {
            if enter {
                self.focus = Focus::Reader;
                self.mark_reader_read();
            }
            return;
        }
        let id = self.accounts[item.account].id.clone();
        let raw = self
            .store
            .body(&id, &item.message.folder, item.message.uid)
            .ok()
            .flatten()
            .map(Arc::new);
        let have = raw.is_some();
        self.reader = Some(Reader {
            account: item.account,
            folder: item.message.folder.clone(),
            uid: item.message.uid,
            message: item.message.clone(),
            raw,
            parsed: None,
            body: None,
            width: 0,
            pending: None,
            scroll: 0,
            error: None,
            plain: None,
            opened_at: if enter {
                Instant::now() - PREVIEW_READ_AFTER
            } else {
                Instant::now()
            },
            marked_read: item.message.flags.seen,
        });
        if enter {
            self.focus = Focus::Reader;
            self.mark_reader_read();
        }
        if have {
            self.render();
        } else if self.demo || self.accounts[item.account].is_pop() {
            if let Some(reader) = self.reader.as_mut() {
                reader.error = Some("This message's text isn't on this computer".into());
            }
        } else {
            self.send(
                item.account,
                Request::Body {
                    folder: item.message.folder.clone(),
                    uid: item.message.uid,
                    uidvalidity: self.uidvalidity(item.account, &item.message.folder),
                },
            );
        }
    }

    /// Lays out the reader's message for the reader's width, in the
    /// background.
    fn render(&mut self) {
        let width = self.reader_width.get().max(20);
        let prefer_plain = self.settings.prefer_plain;
        self.renders += 1;
        let generation = self.renders;
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(raw) = reader.raw.clone() else {
            return;
        };
        reader.pending = Some(generation);
        let plain = reader.plain.unwrap_or(prefer_plain);
        let parsed_before = reader.parsed.clone();
        let tx = self.tx.clone();
        mail::in_background(
            move || {
                let parsed = parsed_before.or_else(|| mail::parse(&raw).map(Arc::new))?;
                let body = mail::render(&parsed, usize::from(width), plain);
                Some((parsed, body))
            },
            move |result| {
                let (parsed, body) = match result.flatten() {
                    Some((p, b)) => (Some(p), Some(b)),
                    None => (None, None),
                };
                let _ = tx.send(AppEvent::Rendered {
                    generation,
                    width,
                    parsed,
                    body,
                });
            },
        );
    }

    fn mark_reader_read(&mut self) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        reader.marked_read = true;
        if !self.settings.mark_read || reader.message.flags.seen {
            return;
        }
        let key = reader.key();
        self.set_flag(&[key], FlagKind::Seen, true);
    }

    fn role_of(&self, account: usize, folder: &str) -> Option<Role> {
        self.accounts
            .get(account)?
            .folders
            .iter()
            .find(|f| f.name == folder)?
            .role
    }

    /// The messages an action applies to: those marked with Space, else
    /// the one being read (in the reader), else the one under the cursor.
    fn targets(&self) -> Vec<(usize, String, u32)> {
        if self.focus == Focus::Reader
            && let Some(reader) = &self.reader
        {
            return vec![reader.key()];
        }
        if !self.marked.is_empty() {
            let mut marked: Vec<_> = self.marked.iter().cloned().collect();
            marked.sort();
            return marked;
        }
        self.selected_item().map(Item::key).into_iter().collect()
    }

    /// `targets` grouped by account and folder.
    fn grouped(targets: &[(usize, String, u32)]) -> Vec<(usize, String, Vec<u32>)> {
        let mut groups: Vec<(usize, String, Vec<u32>)> = Vec::new();
        for (account, folder, uid) in targets {
            match groups
                .iter_mut()
                .find(|(a, f, _)| a == account && f == folder)
            {
                Some(group) => group.2.push(*uid),
                None => groups.push((*account, folder.clone(), vec![*uid])),
            }
        }
        groups
    }

    // Changing mail.

    /// Changes messages' flags with `change` here (cache, list and
    /// reader), where the next check won't undo it for a moment; how many
    /// were changed. The server is told by the caller.
    fn change_here(
        &mut self,
        account: usize,
        folder: &str,
        uids: &[u32],
        change: impl Fn(&mut Flags),
    ) -> usize {
        let id = self.accounts[account].id.clone();
        // Where each is in the list, found once rather than for each.
        let listed: HashMap<u32, usize> = (self.items.iter().enumerate())
            .filter(|(_, it)| it.account == account && it.message.folder == folder)
            .map(|(i, it)| (it.message.uid, i))
            .collect();
        let mut changed = Vec::new();
        for uid in uids {
            let Ok(Some(message)) = self.store.message(&id, folder, *uid) else {
                continue;
            };
            let mut flags = message.flags;
            change(&mut flags);
            let key = (account, folder.to_string(), *uid);
            self.local_flags.insert(key.clone(), Instant::now());
            if let Some(&i) = listed.get(uid) {
                self.items[i].message.flags = flags.clone();
            }
            if let Some(reader) = self.reader.as_mut().filter(|r| r.key() == key) {
                reader.message.flags = flags.clone();
            }
            changed.push((*uid, flags));
        }
        let _ = self.store.set_flags(&id, folder, &changed);
        changed.len()
    }

    /// Sets a flag here (cache and list) and on the server.
    fn set_flag(&mut self, keys: &[(usize, String, u32)], flag: FlagKind, on: bool) {
        for (account, folder, uids) in Self::grouped(keys) {
            let id = self.accounts[account].id.clone();
            // The count moves by those whose flag changes, not all of them.
            let unread = match flag {
                FlagKind::Seen => self.unread_among(account, &folder, &uids),
                _ => 0,
            };
            let found = self.change_here(account, &folder, &uids, |flags| match flag {
                FlagKind::Seen => flags.seen = on,
                FlagKind::Flagged => flags.flagged = on,
                FlagKind::Answered => flags.answered = on,
            });
            if flag == FlagKind::Seen {
                let delta = if on {
                    i64::from(unread)
                } else {
                    found as i64 - i64::from(unread)
                };
                if let Some(f) = self.accounts[account]
                    .folders
                    .iter_mut()
                    .find(|f| f.name == folder)
                {
                    f.unread = if on {
                        (i64::from(f.unread) - delta).max(0) as u32
                    } else {
                        (i64::from(f.unread) + delta).min(i64::from(f.total.max(f.unread))) as u32
                    };
                    let _ = self.store.set_counts(&id, &folder, f.unread, f.total);
                }
            }
            if !self.accounts[account].is_pop() {
                self.send(
                    account,
                    Request::Flag {
                        uidvalidity: self.uidvalidity(account, &folder),
                        folder,
                        uids,
                        flag,
                        on,
                    },
                );
            }
        }
    }

    fn toggle_flag(&mut self, flag: FlagKind) {
        let targets = self.targets();
        let Some(first) = targets.first() else {
            return;
        };
        let id = self.accounts[first.0].id.clone();
        let on = match self.store.message(&id, &first.1, first.2) {
            Ok(Some(m)) => match flag {
                FlagKind::Seen => !m.flags.seen,
                FlagKind::Flagged => !m.flags.flagged,
                FlagKind::Answered => !m.flags.answered,
            },
            _ => return,
        };
        self.change_flag(&targets, flag, on);
        if flag == FlagKind::Seen
            && let Some(reader) = self.reader.as_mut()
        {
            // Marked unread by hand: the reader doesn't mark it read again.
            reader.marked_read = true;
        }
        self.marked.clear();
    }

    /// Sets a flag as a change that can be undone (read or star).
    fn change_flag(&mut self, keys: &[(usize, String, u32)], flag: FlagKind, on: bool) {
        let what = match flag {
            FlagKind::Seen => What::Seen(on),
            FlagKind::Flagged => What::Flagged(on),
            FlagKind::Answered => return,
        };
        let changed = keys
            .iter()
            .filter_map(|key| {
                let mut c = self.changed(key)?;
                c.had = match flag {
                    FlagKind::Seen => self.flags_of(key)?.seen,
                    _ => self.flags_of(key)?.flagged,
                };
                Some(c)
            })
            .collect();
        self.note_change(what, changed);
        self.set_flag(keys, flag, on);
    }

    /// `t`: picks a tag to put on the messages or take off, or a new one.
    fn pick_tag(&mut self) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let groups: Vec<Group> = Self::grouped(&targets)
            .into_iter()
            .map(|(account, folder, uids)| Group {
                uidvalidity: self.uidvalidity(account, &folder),
                account,
                folder,
                uids,
            })
            .collect();
        let on = self.tags_of(&targets);
        // Tags of these messages' accounts: a server's keywords are its own.
        let mut accounts: Vec<&str> = (groups.iter())
            .map(|g| self.accounts[g.account].id.as_str())
            .collect();
        accounts.dedup();
        let mut known = self.store.all_tags(&accounts).unwrap_or_default();
        known.extend(on.iter().map(|(tag, _)| tag.clone()));
        known.sort();
        known.dedup();
        if known.is_empty() {
            self.tagging = groups;
            self.prompt = Some(Prompt::new(PromptKind::Tag, ""));
            return;
        }
        let mut items: Vec<String> = known
            .iter()
            .map(|tag| {
                let mark = match on.iter().find(|(t, _)| t == tag) {
                    Some((_, n)) if *n == targets.len() => '✓',
                    Some(_) => '~',
                    None => ' ',
                };
                format!("{mark} {tag}")
            })
            .collect();
        items.push("+ New tag…".into());
        self.picker = Some(Picker {
            kind: PickerKind::Tag {
                groups,
                tags: known,
            },
            title: format!(" Tag {} ", plural(targets.len(), "message")),
            items,
            labels: Vec::new(),
            selected: 0,
        });
    }

    /// The tags on these messages, each with how many have it.
    fn tags_of(&self, keys: &[(usize, String, u32)]) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for (account, folder, uid) in keys {
            let Some(a) = self.accounts.get(*account) else {
                continue;
            };
            let Ok(Some(message)) = self.store.message(&a.id, folder, *uid) else {
                continue;
            };
            for tag in message.flags.tags {
                match counts.iter_mut().find(|(t, _)| *t == tag) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((tag, 1)),
                }
            }
        }
        counts
    }

    /// Puts `tag` on messages (`on`) or takes it off, here and on the
    /// server, as a change that can be undone.
    fn tag(&mut self, groups: Vec<Group>, tag: &str, on: bool) {
        let count = self.change_tag(groups, tag, on);
        if count > 0 {
            self.info(if on {
                format!("{} tagged {tag}", plural(count, "message"))
            } else {
                format!("{tag} taken off {}", plural(count, "message"))
            });
        }
        self.marked.clear();
    }

    /// Puts `tag` on messages or takes it off as a change that can be
    /// undone; how many.
    fn change_tag(&mut self, groups: Vec<Group>, tag: &str, on: bool) -> usize {
        let changed = groups
            .iter()
            .filter(|g| {
                self.uidvalidity(g.account, &g.folder) == g.uidvalidity || g.uidvalidity == 0
            })
            .flat_map(|g| g.uids.iter().map(|u| (g.account, g.folder.clone(), *u)))
            .filter_map(|key| {
                let mut c = self.changed(&key)?;
                c.had = self.flags_of(&key)?.tags.iter().any(|t| t == tag);
                Some(c)
            })
            .collect();
        self.note_change(What::Tag(tag.to_string(), on), changed);
        self.apply_tag(groups, tag, on)
    }

    /// Puts `tag` on messages or takes it off, here and on the server; how
    /// many.
    fn apply_tag(&mut self, groups: Vec<Group>, tag: &str, on: bool) -> usize {
        let mut count = 0;
        for g in groups {
            if g.account >= self.accounts.len()
                || !self.still_means(g.account, &g.folder, g.uidvalidity)
            {
                continue;
            }
            count += self.change_here(g.account, &g.folder, &g.uids, |flags| {
                flags.tags.retain(|t| t != tag);
                if on {
                    flags.tags.push(tag.to_string());
                }
                tags::tidy(&mut flags.tags);
            });
            if !self.accounts[g.account].is_pop() {
                self.send(
                    g.account,
                    Request::Tag {
                        folder: g.folder,
                        uids: g.uids,
                        tag: tag.to_string(),
                        on,
                        uidvalidity: g.uidvalidity,
                    },
                );
            }
        }
        count
    }

    // Changes: kept so they can be listed and undone.

    /// A message as a change keeps it (before the change).
    fn changed(&self, key: &(usize, String, u32)) -> Option<Changed> {
        let account = self.accounts.get(key.0)?;
        let message = self.store.message(&account.id, &key.1, key.2).ok()??;
        Some(Changed {
            account: account.id.clone(),
            folder: key.1.clone(),
            uidvalidity: self.uidvalidity(key.0, &key.1),
            uid: key.2,
            message_id: message.summary.message_id,
            size: message.size,
            ..Changed::default()
        })
    }

    fn flags_of(&self, key: &(usize, String, u32)) -> Option<Flags> {
        let account = self.accounts.get(key.0)?;
        let message = self.store.message(&account.id, &key.1, key.2).ok()??;
        Some(message.flags)
    }

    /// Adds to the change being made (one key's, which can act on several
    /// folders), or starts another.
    fn note_change(&mut self, what: What, messages: Vec<Changed>) {
        if messages.is_empty() {
            return;
        }
        match self.change.as_mut() {
            Some(change) if change.what == what => change.messages.extend(messages),
            _ => {
                let _ = self.keep_change();
                self.change = Some(Change {
                    id: 0,
                    at: now(),
                    who: String::new(),
                    what,
                    messages,
                    undone: false,
                });
            }
        }
    }

    /// Keeps the change being made, if any.
    fn keep_change(&mut self) -> Option<i64> {
        let change = self.change.take()?;
        self.store.add_change(&change, now()).ok()
    }

    /// What a change did, as the status line says it.
    pub fn describe(&self, change: &Change) -> String {
        let n = plural(change.messages.len(), "message");
        match &change.what {
            What::Seen(true) => format!("{n} marked read"),
            What::Seen(false) => format!("{n} marked unread"),
            What::Flagged(true) => format!("{n} starred"),
            What::Flagged(false) => format!("{n} unstarred"),
            What::Tag(tag, true) => format!("{n} tagged {tag}"),
            What::Tag(tag, false) => format!("{tag} taken off {n}"),
            What::Move => {
                let to = change
                    .messages
                    .first()
                    .and_then(|m| {
                        Some(self.folder_name(self.account_index(&m.account)?, &m.moved_to))
                    })
                    .unwrap_or_default();
                format!("{n} moved to {to}")
            }
        }
    }

    /// `z`: undoes the newest change not undone yet.
    fn undo_last(&mut self) {
        let _ = self.keep_change();
        let last = self
            .store
            .changes(50)
            .unwrap_or_default()
            .into_iter()
            .find(|c| !c.undone);
        match last {
            Some(change) => {
                let _ = self.undo(change);
            }
            None => self.info("Nothing to undo"),
        }
    }

    /// `Z`: picks a change to undo, newest first.
    fn pick_change(&mut self) {
        let _ = self.keep_change();
        let changes = self.store.changes(100).unwrap_or_default();
        if changes.is_empty() {
            return self.info("No changes yet");
        }
        let now = now();
        let items = changes
            .iter()
            .map(|c| {
                // An agent's name is its own: it could call itself "you".
                let who = if c.who.is_empty() {
                    "you".to_string()
                } else {
                    format!("AI agent {}", c.who)
                };
                let undone = if c.undone { " (undone)" } else { "" };
                format!(
                    "{:>9}  {who}: {}{undone}",
                    mail::short_date(c.at, now),
                    self.describe(c)
                )
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Changes(changes.iter().map(|c| c.id).collect()),
            title: " Changes ".into(),
            items,
            labels: Vec::new(),
            selected: 0,
        });
    }

    /// Undoes a change: flags and tags set back on the messages it changed
    /// (those that were already so are left), moved messages moved back.
    /// Shows why when it can't, and says so.
    fn undo(&mut self, change: Change) -> Result<(), &'static str> {
        if change.undone {
            let why = "That was undone already";
            self.info(why);
            return Err(why);
        }
        let what = self.describe(&change);
        // By account and folder, and for moves where they went.
        let mut groups: Vec<(usize, String, u32, String, Vec<Changed>)> = Vec::new();
        for m in change.messages {
            let Some(account) = self.account_index(&m.account) else {
                continue;
            };
            match groups.iter_mut().find(|g| {
                g.0 == account && g.1 == m.folder && g.2 == m.uidvalidity && g.3 == m.moved_to
            }) {
                Some(g) => g.4.push(m),
                None => groups.push((
                    account,
                    m.folder.clone(),
                    m.uidvalidity,
                    m.moved_to.clone(),
                    vec![m],
                )),
            }
        }
        // A move on the server is undone by where it put the mail: known
        // once the server has made it.
        let unplaced =
            |(account, _, _, _, messages): &(usize, String, u32, String, Vec<Changed>)| {
                let local = self.accounts[*account].is_pop() || self.demo;
                !local
                    && messages
                        .iter()
                        .any(|m| m.moved_floor == 0 || m.moved_uidvalidity == 0)
            };
        if change.what == What::Move && groups.iter().any(unplaced) {
            let why = "That move hasn't reached the server yet, or where it put the mail isn't known: undo it once it has, or move them back by hand";
            self.error(why);
            return Err(why);
        }
        let _ = self.store.set_undone(change.id);
        self.info(format!("Undone: {what}"));
        for (account, folder, uidvalidity, moved_to, messages) in groups {
            match &change.what {
                What::Seen(on) | What::Flagged(on) => {
                    if !self.still_means(account, &folder, uidvalidity) {
                        continue;
                    }
                    let flag = if matches!(change.what, What::Seen(_)) {
                        FlagKind::Seen
                    } else {
                        FlagKind::Flagged
                    };
                    let keys: Vec<_> = messages
                        .iter()
                        .filter(|m| m.had != *on)
                        .map(|m| (account, folder.clone(), m.uid))
                        .collect();
                    self.set_flag(&keys, flag, !on);
                }
                What::Tag(tag, on) => {
                    let uids = messages
                        .iter()
                        .filter(|m| m.had != *on)
                        .map(|m| m.uid)
                        .collect();
                    let group = Group {
                        account,
                        folder,
                        uids,
                        uidvalidity,
                    };
                    self.apply_tag(vec![group], tag, !on);
                }
                What::Move => self.move_back(account, &moved_to, &folder, &messages),
            }
        }
        Ok(())
    }

    /// Moves messages back from `from`, where a change moved them, to
    /// `to`: exactly those it moved, never another message with the same
    /// Message-ID (which the sender writes, and needn't be unique). In
    /// folders kept here, by the UID the move gave them (never given to
    /// another message), if the message there is still that one; on the
    /// server, by where the move put them (`Changed::moved_floor`).
    fn move_back(&mut self, account: usize, from: &str, to: &str, messages: &[Changed]) {
        let id = self.accounts[account].id.clone();
        let local = self.accounts[account].is_pop() || self.demo;
        if local {
            let mut uids: Vec<u32> = (messages.iter())
                .filter(|m| m.new_uid != 0)
                .filter(|m| {
                    (self.store.message(&id, from, m.new_uid).ok().flatten())
                        .is_some_and(|x| x.summary.message_id == m.message_id)
                })
                .map(|m| m.new_uid)
                .collect();
            uids.sort_unstable();
            uids.dedup();
            let keys: Vec<_> = uids
                .iter()
                .map(|u| (account, from.to_string(), *u))
                .collect();
            let _ = self.store.move_local(&id, from, &uids, to);
            self.refresh_counts(account, from);
            self.refresh_counts(account, to);
            let missing = messages.len().saturating_sub(uids.len());
            if missing > 0 {
                self.error(format!(
                    "{} of them weren't in {} any more, so stay where they are",
                    missing,
                    self.folder_name(account, from)
                ));
            }
            self.take_off_list(&keys);
        } else {
            // One request per move (each has its own place there); off the
            // list when the server says which it moved (`Event::MovedBack`).
            // By where each move put them: (UIDVALIDITY, floor, each one's
            // Message-ID and size).
            type Moves = Vec<(u32, u32, Vec<(String, u32)>)>;
            let mut moves: Moves = Vec::new();
            let mut nameless = 0;
            for m in messages {
                if m.message_id.is_empty() {
                    nameless += 1;
                    continue;
                }
                let one = (m.message_id.clone(), m.size);
                match moves
                    .iter_mut()
                    .find(|(v, f, _)| *v == m.moved_uidvalidity && *f == m.moved_floor)
                {
                    Some(group) => group.2.push(one),
                    None => moves.push((m.moved_uidvalidity, m.moved_floor, vec![one])),
                }
            }
            for (uidvalidity, floor, moved) in moves {
                self.send(
                    account,
                    Request::MoveBack {
                        folder: from.to_string(),
                        uidvalidity,
                        floor,
                        moved,
                        to: to.to_string(),
                    },
                );
            }
            if nameless > 0 {
                self.error(format!(
                    "{} without a Message-ID can't be found again: look in {}",
                    plural(nameless, "message"),
                    self.folder_name(account, from)
                ));
            }
        }
        if self.shows(account, to) {
            self.reload();
        }
    }

    // Signing in with Microsoft or Google (`oauth.rs`).

    /// Signs account `i` in again, in the browser.
    fn sign_in(&mut self, i: usize) {
        if self.signing_in {
            return self.info("A sign-in is waiting in your browser");
        }
        let account = self.accounts[i].account.clone();
        let Some(client) = crate::oauth::client_for(&account) else {
            return self.error(format!(
                "{} can't sign in: see OAuth2 in tuimail's README",
                self.account_name(i)
            ));
        };
        // Made-up accounts: nothing is listened for or sent.
        if self.demo {
            return self.info("The demo doesn't sign in");
        }
        self.signing_in = true;
        let (tx, id) = (self.tx.clone(), self.accounts[i].id.clone());
        tokio::spawn(async move {
            let result = sign_in(client, &account.email, &tx).await;
            let _ = tx.send(AppEvent::SignedIn {
                account: id,
                result: result.map_err(|e| text::one_line(&format!("{e:#}"), 300)),
            });
        });
    }

    /// The browser opens on the provider's sign-in page; the link is
    /// copied too, for when it doesn't.
    fn open_sign_in(&mut self, url: &str) {
        self.signing_in = true;
        if self.demo {
            return;
        }
        let opened = tools::open_externally(url).is_ok();
        self.copy_quietly(url);
        self.info(if opened {
            "Sign in in your browser (the link is copied, if it didn't open)"
        } else {
            "Paste the copied link in your browser to sign in"
        });
    }

    fn on_signed_in(&mut self, id: &str, result: Result<crate::oauth::Tokens, String>) {
        self.signing_in = false;
        let Some(i) = self.account_index(id) else {
            return;
        };
        match result {
            Ok(tokens) => {
                if let Some(worker) = &self.accounts[i].worker {
                    worker.creds.set_tokens(tokens);
                }
                self.accounts[i].state = State::Connecting;
                self.check(i, true);
                self.info(format!("{}: signed in", self.account_name(i)));
            }
            Err(e) => self.error(format!("{}: {e}", self.account_name(i))),
        }
    }

    // AI agents (`agent.rs`): what they ask, answered here.

    /// Listens for agents while the setting is on, or stops.
    fn listen_for_agents(&mut self) {
        let wanted = self.settings.agents && self.agent_socket.is_some();
        if wanted && self.agents.is_none() {
            let Some(socket) = self.agent_socket.clone() else {
                return;
            };
            match agent::listen(&socket, self.tx.clone()) {
                Ok(listener) => self.agents = Some(listener),
                Err(e) => self.error(format!("AI agents: {e:#}")),
            }
        } else if !wanted && self.agents.is_some() {
            self.agents = None;
            for ask in std::mem::take(&mut self.agent_asks) {
                let _ = ask.reply.send(Err("The user turned AI agents off".into()));
            }
            for read in std::mem::take(&mut self.agent_reads) {
                let _ = read.reply.send(Err("The user turned AI agents off".into()));
            }
            if matches!(self.confirm, Some(Confirm::Agent { .. })) {
                self.confirm = None;
            }
        }
    }

    /// Whether agents are let in now: listening, and the setting still on
    /// in settings.toml (another tuimail, or a hand edit, may have turned
    /// it off since). If it's off there, it's off here too, and listening
    /// stops at the next tick (after this refusal is answered).
    fn agents_allowed(&mut self) -> bool {
        if let Some(path) = &self.settings_path
            && let Ok((on_disk, _)) = Settings::load_checked(path)
            && !on_disk.agents
        {
            self.settings.agents = false;
        }
        self.settings.agents && self.agents.is_some()
    }

    fn on_agent(
        &mut self,
        request: agent::Request,
        reply: tokio::sync::oneshot::Sender<agent::Answer>,
    ) {
        let who = agent::shown_client(&request.client);
        let call = request.call;
        if call.changes() {
            self.agent_spent
                .retain(|(at, ..)| at.elapsed() < Duration::from_secs(3600));
            match self.agent_plan(&call) {
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
                Ok(plan) if self.settings.confirm_agents => {
                    self.next_ask += 1;
                    let which = self.plan_lines(&plan);
                    self.notifier.add(notify::Note {
                        account: String::new(),
                        // Not to pass for a sender's mail.
                        from: format!("AI agent {who}"),
                        subject: format!("asks to {}: answer in tuimail", plan.what),
                    });
                    self.agent_asks.push_back(AgentAsk {
                        id: self.next_ask,
                        who,
                        what: plan.what,
                        which,
                        call,
                        reply,
                        at: Instant::now(),
                    });
                    self.ask_agents();
                }
                Ok(plan) => {
                    let answer = self.agent_apply(&who, plan);
                    if let Ok(done) = &answer {
                        self.notifier.add(notify::Note {
                            account: String::new(),
                            from: who,
                            subject: done["done"].as_str().unwrap_or_default().to_string(),
                        });
                    }
                    let _ = reply.send(answer);
                }
            }
            return;
        }
        let answer = match call {
            Call::ListAccounts(_) => Ok(self.agent_accounts()),
            Call::ListFolders(args) => self.agent_folders(&args.account),
            Call::SearchMessages(args) => self.agent_search(&args),
            Call::ListChanges(args) => Ok(self.agent_changes(args.limit)),
            Call::ReadMessage(args) => return self.agent_read(args, reply),
            _ => Err("That isn't something tuimail does".into()),
        };
        let _ = reply.send(answer);
    }

    /// Answers what nobody waits for any more, or the user didn't answer
    /// in time, and asks about the next change when nothing else is open.
    fn ask_agents(&mut self) {
        let mut i = 0;
        while i < self.agent_asks.len() {
            let ask = &self.agent_asks[i];
            if ask.reply.is_closed() || ask.at.elapsed() >= agent::ASK_FOR {
                let ask = self.agent_asks.remove(i).expect("it's there");
                let _ = ask.reply.send(Err("The user didn't answer in time".into()));
                if self.confirm == Some(Confirm::Agent { id: ask.id }) {
                    self.confirm = None;
                }
            } else {
                i += 1;
            }
        }
        for read in std::mem::take(&mut self.agent_reads) {
            if read.at.elapsed() < Duration::from_secs(60) && !read.reply.is_closed() {
                self.agent_reads.push(read);
            } else {
                let _ = read.reply.send(Err(
                    "The message didn't download in time: tuimail may be offline".into(),
                ));
            }
        }
        let free = self.confirm.is_none()
            && self.composer.is_none()
            && self.setup.is_none()
            && self.prompt.is_none()
            && self.picker.is_none()
            && self.help.is_none();
        if free && let Some(ask) = self.agent_asks.front() {
            let id = ask.id;
            self.confirm(Confirm::Agent { id });
        }
    }

    /// The user's answer to an agent's change: made if `yes` (checked
    /// again: mail may have changed while it waited).
    fn agent_answer(&mut self, id: u64, yes: bool) {
        let Some(n) = self.agent_asks.iter().position(|a| a.id == id) else {
            return;
        };
        let ask = self.agent_asks.remove(n).expect("it's there");
        let answer = if yes {
            self.agent_spent
                .retain(|(at, ..)| at.elapsed() < Duration::from_secs(3600));
            self.agent_plan(&ask.call)
                .and_then(|plan| self.agent_apply(&ask.who, plan))
        } else {
            self.info(format!("{} wasn't let {}", ask.who, ask.what));
            Err(
                "The user said no. Don't do it another way, or ask again unless they want it."
                    .into(),
            )
        };
        let _ = ask.reply.send(answer);
    }

    /// An agent's change, checked against the rules: which messages (ids
    /// still meaning them), where to, never deleting, within the limits.
    fn agent_plan(&self, call: &Call) -> Result<Plan, String> {
        let n = |groups: &[Group]| plural(groups.iter().map(|g| g.uids.len()).sum(), "message");
        let mut plan = match call {
            Call::MarkRead(args) => {
                let groups = self.agent_groups(&args.ids)?;
                let what = format!(
                    "mark {} {}",
                    n(&groups),
                    if args.read { "read" } else { "unread" }
                );
                Plan::new(Action::Seen(args.read), groups, what)
            }
            Call::Star(args) => {
                let groups = self.agent_groups(&args.ids)?;
                let what = if args.starred {
                    format!("star {}", n(&groups))
                } else {
                    format!("take the star off {}", n(&groups))
                };
                Plan::new(Action::Flagged(args.starred), groups, what)
            }
            Call::SetTag(args) => {
                let tag = tags::normalize(&args.tag).ok_or_else(|| {
                    format!(
                        "A tag is one word of letters, digits, - _ or . (at most {} characters)",
                        tags::MAX_TAG
                    )
                })?;
                let groups = self.agent_groups(&args.ids)?;
                let what = if args.on {
                    format!("tag {} {tag}", n(&groups))
                } else {
                    format!("take {tag} off {}", n(&groups))
                };
                Plan::new(Action::Tag(tag, args.on), groups, what)
            }
            Call::MoveMessages(args) => {
                let groups = self.agent_groups(&args.ids)?;
                let mut targets = Vec::new();
                for g in &groups {
                    let to = self.agent_folder(g.account, &args.folder)?;
                    match self.role_of(g.account, &to) {
                        Some(Role::Trash) => return Err("Use move_to_trash for Trash".into()),
                        Some(Role::Junk) => return Err("Use mark_spam for Junk".into()),
                        // What's there is the user's own writing: a message
                        // there opens as their draft (and saving it deletes
                        // it), or shows as sent by them.
                        Some(Role::Drafts | Role::Sent) => {
                            return Err(
                                "Drafts and Sent hold what the user wrote: agents don't move mail there"
                                    .into(),
                            );
                        }
                        _ if to == g.folder => {
                            return Err(format!(
                                "They're in {} already",
                                self.folder_name(g.account, &to)
                            ));
                        }
                        _ => targets.push(to),
                    }
                }
                // Every place it goes, by its whole name: whose folder it is
                // shows.
                let mut to: Vec<String> = Vec::new();
                for (g, t) in groups.iter().zip(&targets) {
                    let name = self.folder_path(g.account, t);
                    if !to.contains(&name) {
                        to.push(name);
                    }
                }
                let what = format!("move {} to {}", n(&groups), to.join(" and "));
                Plan {
                    targets,
                    ..Plan::new(Action::Move, groups, what)
                }
            }
            Call::MarkSpam(args) => {
                let groups = self.agent_groups(&args.ids)?;
                let mut targets = Vec::new();
                for g in &groups {
                    let account = &self.accounts[g.account];
                    let junk = account.folder_for(Role::Junk).map(|f| f.name.clone());
                    let Some(junk) = junk else {
                        return Err(format!(
                            "{} has no Junk folder",
                            account.account.shown_name()
                        ));
                    };
                    if args.spam && g.folder == junk {
                        return Err("They're in Junk already".into());
                    }
                    if !args.spam && g.folder != junk {
                        return Err("Only mail in Junk can be marked not spam".into());
                    }
                    targets.push(if args.spam { junk } else { account.inbox() });
                }
                let what = if args.spam {
                    format!("mark {} as spam (to Junk)", n(&groups))
                } else {
                    format!("mark {} as not spam (back to the inbox)", n(&groups))
                };
                Plan {
                    targets,
                    ..Plan::new(Action::Move, groups, what)
                }
            }
            Call::MoveToTrash(args) => {
                let groups = self.agent_groups(&args.ids)?;
                let mut targets = Vec::new();
                for g in &groups {
                    let account = &self.accounts[g.account];
                    // Without a Trash, `d` deletes for good: never for agents.
                    let Some(trash) = account.folder_for(Role::Trash).map(|f| f.name.clone())
                    else {
                        return Err(format!(
                            "{} has no Trash folder, so nothing can be moved there",
                            account.account.shown_name()
                        ));
                    };
                    // From Trash, `d` deletes for good: only the user does.
                    if g.folder == trash {
                        return Err(format!(
                            "{} in Trash already: only the user deletes mail for good",
                            plural(g.uids.len(), "message")
                        ));
                    }
                    targets.push(trash);
                }
                let what = format!("move {} to Trash", n(&groups));
                let trash = groups.iter().map(|g| g.uids.len()).sum();
                Plan {
                    targets,
                    trash,
                    ..Plan::new(Action::Move, groups, what)
                }
            }
            Call::UndoChange(args) => {
                let change = self
                    .store
                    .change(args.change)
                    .ok()
                    .flatten()
                    .ok_or("No change has that id (they're kept 30 days)")?;
                if change.undone {
                    return Err("That change was undone already".into());
                }
                if change.who.is_empty() {
                    return Err("That change is the user's: only they undo it".into());
                }
                let what = format!("undo this: {}", self.describe(&change));
                // Undoing a move out of Trash or Junk puts mail back there.
                let trash = if change.what == What::Move {
                    (change.messages.iter())
                        .filter(|m| {
                            self.account_index(&m.account).is_some_and(|i| {
                                matches!(self.role_of(i, &m.folder), Some(Role::Trash | Role::Junk))
                            })
                        })
                        .count()
                } else {
                    0
                };
                Plan {
                    trash,
                    ..Plan::new(Action::Undo(Box::new(change)), Vec::new(), what)
                }
            }
            _ => return Err("That doesn't change mail".into()),
        };
        // Moves to Trash by way of another tool count as Trash too, and so
        // do moves to Junk (which servers empty on their own).
        if plan.trash == 0 && matches!(plan.action, Action::Move) {
            plan.trash = (plan.groups.iter().zip(&plan.targets))
                .filter(|(g, to)| {
                    matches!(self.role_of(g.account, to), Some(Role::Trash | Role::Junk))
                })
                .map(|(g, _)| g.uids.len())
                .sum();
        }
        let (spent, trashed) = self
            .agent_spent
            .iter()
            .fold((0, 0), |(a, b), (_, n, t)| (a + n, b + t));
        if spent + plan.count() > agent::CHANGES_PER_HOUR {
            return Err(format!(
                "Agents have changed {spent} messages in the last hour, and {} is the most: \
                 wait, or ask the user to do it",
                agent::CHANGES_PER_HOUR
            ));
        }
        if trashed + plan.trash > agent::TRASH_PER_HOUR {
            return Err(format!(
                "Agents have moved {trashed} messages to Trash in the last hour, and {} is \
                 the most: wait, or ask the user to do it",
                agent::TRASH_PER_HOUR
            ));
        }
        Ok(plan)
    }

    /// The messages agents' ids name, each still the message it was.
    fn agent_groups(&self, ids: &[String]) -> Result<Vec<Group>, String> {
        if ids.is_empty() {
            return Err("No message ids were given".into());
        }
        if ids.len() > agent::MAX_IDS {
            return Err(format!("At most {} messages a call", agent::MAX_IDS));
        }
        let mut keys: Vec<(usize, String, u32)> = Vec::new();
        for (n, id) in ids.iter().enumerate() {
            let key = self.agent_key(id).ok_or_else(|| {
                format!(
                    "Id {} isn't a message tuimail has now (it moved, or its folder changed): \
                     search again",
                    n + 1
                )
            })?;
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        Ok(Self::grouped(&keys)
            .into_iter()
            .map(|(account, folder, uids)| Group {
                uidvalidity: self.uidvalidity(account, &folder),
                account,
                folder,
                uids,
            })
            .collect())
    }

    /// The message an agent's id names, if it's still that one here.
    fn agent_key(&self, id: &str) -> Option<(usize, String, u32)> {
        let (account, folder, uidvalidity, uid) = agent::unhandle(id)?;
        let i = self.account_index(&account)?;
        self.accounts[i].folders.iter().find(|f| f.name == folder)?;
        if self.uidvalidity(i, &folder) != uidvalidity || self.foreign(i, &folder) {
            return None;
        }
        self.store.message(&account, &folder, uid).ok()??;
        Some((i, folder, uid))
    }

    /// An agent's id for a message.
    fn agent_id(&self, i: usize, folder: &str, uid: u32) -> String {
        agent::handle(
            &self.accounts[i].id,
            folder,
            self.uidvalidity(i, folder),
            uid,
        )
    }

    /// The account an agent names, by its name or address.
    fn agent_account(&self, name: &str) -> Result<usize, String> {
        let name = name.trim();
        self.accounts
            .iter()
            .position(|a| {
                a.id.eq_ignore_ascii_case(name) || a.account.shown_name().eq_ignore_ascii_case(name)
            })
            .ok_or_else(|| format!("There's no account {name:?}: see list_accounts"))
    }

    /// A folder of account `i` an agent names: by its role, server name or
    /// name.
    fn agent_folder(&self, i: usize, name: &str) -> Result<String, String> {
        let account = &self.accounts[i];
        let name = name.trim();
        let role = match name.to_lowercase().as_str() {
            "inbox" => Some(Role::Inbox),
            "archive" => Some(Role::Archive),
            "sent" => Some(Role::Sent),
            "drafts" => Some(Role::Drafts),
            "junk" | "spam" => Some(Role::Junk),
            "trash" => Some(Role::Trash),
            _ => None,
        };
        let by_role = role.and_then(|role| {
            account.folder_for(role).or_else(|| {
                // Gmail archives to All Mail.
                (role == Role::Archive)
                    .then(|| account.folder_for(Role::All))
                    .flatten()
            })
        });
        by_role
            .or_else(|| account.folders.iter().find(|f| f.name == name))
            .or_else(|| (account.folders.iter()).find(|f| f.shown.eq_ignore_ascii_case(name)))
            // The user's own: not another user's, a shared or a public one.
            .filter(|f| f.selectable && !crate::imap::foreign(&f.shown, f.delimiter.as_deref()))
            .map(|f| f.name.clone())
            .ok_or_else(|| {
                format!(
                    "{} has no folder {name:?}: see list_folders",
                    account.account.shown_name()
                )
            })
    }

    /// The messages a change is for, as the question shows them: who
    /// from and the subject, a few.
    fn plan_lines(&self, plan: &Plan) -> Vec<String> {
        let mut keys: Vec<(usize, String, u32)> = plan
            .groups
            .iter()
            .flat_map(|g| g.uids.iter().map(|u| (g.account, g.folder.clone(), *u)))
            .collect();
        // An undo: the messages its change changed, where they are now.
        if let Action::Undo(change) = &plan.action {
            for m in &change.messages {
                let Some(i) = self.account_index(&m.account) else {
                    continue;
                };
                let local = self.accounts[i].is_pop() || self.demo;
                let key = match change.what {
                    What::Move if local => Some((i, m.moved_to.clone(), m.new_uid)),
                    What::Move => (self.store)
                        .message_with_id(&m.account, &m.moved_to, &m.message_id)
                        .ok()
                        .flatten()
                        .map(|x| (i, m.moved_to.clone(), x.uid)),
                    _ => Some((i, m.folder.clone(), m.uid)),
                };
                keys.extend(key);
            }
        }
        let mut lines: Vec<String> = keys
            .iter()
            .take(5)
            .filter_map(|(i, folder, uid)| {
                let m = self
                    .store
                    .message(&self.accounts[*i].id, folder, *uid)
                    .ok()??;
                let subject = if m.summary.subject.is_empty() {
                    "(no subject)".to_string()
                } else {
                    m.summary.subject
                };
                // The address, not a name the sender chose: anyone can
                // call themselves anything.
                Some(format!(
                    "{} · {subject} · {}",
                    m.summary.from.addr,
                    self.folder_path(*i, folder)
                ))
            })
            .collect();
        let count = match &plan.action {
            Action::Undo(change) => change.messages.len(),
            _ => keys.len(),
        };
        if count > 5 {
            lines.push(format!("and {} more", count - 5));
        }
        lines
    }

    /// A folder's whole name, `Other Users/bob/Invoices` and not only
    /// `Invoices`: for questions, where whose folder it is matters.
    fn folder_path(&self, i: usize, folder: &str) -> String {
        let f = self
            .accounts
            .get(i)
            .and_then(|a| a.folders.iter().find(|f| f.name == folder));
        match f {
            Some(f) if f.role != Some(Role::Inbox) => text::one_line(&f.shown, 80),
            _ => self.folder_name(i, folder),
        }
    }

    /// A folder of another user's, a shared or a public namespace
    /// (`imap::foreign`): agents see and change only the user's own.
    fn foreign(&self, i: usize, folder: &str) -> bool {
        (self
            .accounts
            .get(i)
            .and_then(|a| a.folders.iter().find(|f| f.name == folder)))
        .is_some_and(|f| crate::imap::foreign(&f.shown, f.delimiter.as_deref()))
    }

    /// Makes an agent's change, as the user's keys would: kept as a change
    /// that's the agent's, which `z` undoes.
    fn agent_apply(&mut self, who: &str, plan: Plan) -> agent::Answer {
        let _ = self.keep_change();
        let count = plan.count();
        let trash = plan.trash;
        match plan.action {
            Action::Undo(change) => {
                let what = self.describe(&change);
                if let Err(why) = self.undo(*change) {
                    return Err(why.into());
                }
                self.agent_spent.push_back((Instant::now(), count, 0));
                self.info(format!("{who} undid this: {what}"));
                return Ok(serde_json::json!({ "done": format!("undid this: {what}") }));
            }
            Action::Seen(on) | Action::Flagged(on) => {
                let flag = if matches!(plan.action, Action::Seen(_)) {
                    FlagKind::Seen
                } else {
                    FlagKind::Flagged
                };
                let keys: Vec<_> = (plan.groups.iter())
                    .flat_map(|g| g.uids.iter().map(|u| (g.account, g.folder.clone(), *u)))
                    .collect();
                self.change_flag(&keys, flag, on);
            }
            Action::Tag(tag, on) => {
                self.change_tag(plan.groups, &tag, on);
            }
            Action::Move => {
                for (g, to) in plan.groups.iter().zip(&plan.targets) {
                    self.move_messages(g.account, &g.folder, &g.uids, to);
                }
            }
        }
        if let Some(change) = self.change.as_mut() {
            change.who = who.to_string();
        }
        let id = self.keep_change();
        self.agent_spent.push_back((Instant::now(), count, trash));
        let done = id
            .and_then(|id| self.store.change(id).ok().flatten())
            .map(|c| self.describe(&c))
            .unwrap_or(plan.what);
        self.info(format!("{who}: {done} · z undoes"));
        Ok(serde_json::json!({ "done": done, "change": id }))
    }

    fn agent_accounts(&self) -> serde_json::Value {
        let accounts: Vec<serde_json::Value> = (self.accounts.iter())
            .map(|a| {
                serde_json::json!({
                    "account": a.account.shown_name(),
                    "address": a.id,
                    "kind": if a.is_pop() { "pop3" } else { "imap" },
                    "online": a.state == State::Online,
                })
            })
            .collect();
        serde_json::json!(accounts)
    }

    fn agent_folders(&self, account: &str) -> agent::Answer {
        let i = self.agent_account(account)?;
        let folders: Vec<serde_json::Value> = (self.accounts[i].folders.iter())
            .filter(|f| f.selectable && !crate::imap::foreign(&f.shown, f.delimiter.as_deref()))
            .map(|f| {
                serde_json::json!({
                    "folder": f.name,
                    "name": f.shown,
                    "role": f.role.map(Role::as_str),
                    "unread": f.unread,
                    "total": f.total,
                })
            })
            .collect();
        Ok(serde_json::json!(folders))
    }

    fn agent_search(&self, args: &agent::SearchArgs) -> agent::Answer {
        let limit = args.limit.unwrap_or(20).clamp(1, agent::MAX_RESULTS);
        let accounts: Vec<usize> = match &args.account {
            Some(name) => vec![self.agent_account(name)?],
            None => (0..self.accounts.len()).collect(),
        };
        let query = args
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty());
        let mut found: Vec<(usize, Message)> = Vec::new();
        for i in accounts {
            let folder = match &args.folder {
                Some(name) => match self.agent_folder(i, name) {
                    Ok(folder) => Some(folder),
                    // Every account was asked: one without it has none.
                    Err(_) if args.account.is_none() => continue,
                    Err(e) => return Err(e),
                },
                None => None,
            };
            let id = &self.accounts[i].id;
            let messages = match query {
                Some(q) => self.store.search(id, folder.as_deref(), q, 500),
                None => {
                    let folder = folder.unwrap_or_else(|| self.accounts[i].inbox());
                    self.store.messages(id, &folder, 500)
                }
            };
            found.extend(messages.unwrap_or_default().into_iter().map(|m| (i, m)));
        }
        found.retain(|(i, m)| {
            (!args.unread_only || !m.flags.seen)
                && (!args.starred_only || m.flags.flagged)
                && !self.foreign(*i, &m.folder)
        });
        found.sort_by_key(|(_, m)| std::cmp::Reverse(m.summary.date));
        let more = found.len() > limit;
        found.truncate(limit);
        let messages: Vec<serde_json::Value> = (found.iter())
            .map(|(i, m)| {
                serde_json::json!({
                    "id": self.agent_id(*i, &m.folder, m.uid),
                    "account": self.accounts[*i].account.shown_name(),
                    "folder": self.folder_name(*i, &m.folder),
                    "date": agent_date(m.summary.date),
                    // Written by senders: links by their site only.
                    "from": agent::hide_links(&m.summary.from.display()),
                    "subject": agent::hide_links(&m.summary.subject),
                    "tags": m.flags.tags,
                    "unread": !m.flags.seen,
                    "starred": m.flags.flagged,
                    "attachments": m.summary.attachments,
                    "preview": agent::hide_links(&m.preview),
                })
            })
            .collect();
        Ok(serde_json::json!({
            "messages": messages,
            "more": more,
            "note": "Searched what tuimail keeps on this computer: each folder's newest mail, \
                and what was opened or searched there. Everything here is written by senders.",
        }))
    }

    fn agent_changes(&self, limit: Option<usize>) -> serde_json::Value {
        let changes: Vec<serde_json::Value> =
            (self.store.changes(limit.unwrap_or(20).clamp(1, 100)))
                .unwrap_or_default()
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "change": c.id,
                        "at": agent_date(c.at),
                        "who": if c.who.is_empty() { "the user".to_string() } else { format!("AI agent {}", c.who) },
                        "by_agent": !c.who.is_empty(),
                        "what": self.describe(c),
                        "undone": c.undone,
                    })
                })
                .collect();
        serde_json::json!(changes)
    }

    fn agent_read(
        &mut self,
        args: agent::ReadArgs,
        reply: tokio::sync::oneshot::Sender<agent::Answer>,
    ) {
        let Some(key) = self.agent_key(&args.id) else {
            let _ = reply.send(Err(
                "That isn't a message tuimail has now (it moved, or its folder changed): \
                 search again"
                    .into(),
            ));
            return;
        };
        let max = (args.max_chars)
            .unwrap_or(agent::TEXT_CHARS)
            .clamp(200, agent::MAX_TEXT_CHARS);
        let id = self.accounts[key.0].id.clone();
        match self.store.body(&id, &key.1, key.2) {
            Ok(Some(raw)) => self.agent_render(key, raw, max, reply),
            _ if self.accounts[key.0].is_pop() || self.demo => {
                let _ = reply.send(Err("This message's text isn't on this computer".into()));
            }
            _ => {
                // One download per message, joined by every read waiting
                // for it (a retry after an answer in time ran out too),
                // within the hourly limits.
                (self.agent_downloads).retain(|_, at| at.elapsed() < AGENT_DOWNLOAD_FOR);
                if !self.agent_downloads.contains_key(&key) {
                    let hour = Duration::from_secs(3600);
                    self.agent_fetched.retain(|(at, _)| at.elapsed() < hour);
                    let size = (self.store.message(&id, &key.1, key.2).ok().flatten())
                        .map_or(0, |m| u64::from(m.size));
                    let bytes: u64 = self.agent_fetched.iter().map(|(_, b)| b).sum();
                    if self.agent_fetched.len() >= agent::DOWNLOADS_PER_HOUR
                        || bytes + size > agent::DOWNLOAD_BYTES_PER_HOUR
                    {
                        let _ = reply.send(Err(
                            "Agents have downloaded as much mail as they may this hour: \
                             wait, or ask the user to open it"
                                .into(),
                        ));
                        return;
                    }
                    self.agent_fetched.push_back((Instant::now(), size));
                    self.agent_downloads.insert(key.clone(), Instant::now());
                    self.send(
                        key.0,
                        Request::Body {
                            folder: key.1.clone(),
                            uid: key.2,
                            uidvalidity: self.uidvalidity(key.0, &key.1),
                        },
                    );
                }
                self.agent_reads.push(AgentRead {
                    key,
                    max,
                    reply,
                    at: Instant::now(),
                });
            }
        }
    }

    /// The agents' reads waiting for this message, taken.
    fn agent_reads_for(&mut self, key: &(usize, String, u32)) -> Vec<AgentRead> {
        let (waiting, rest) = std::mem::take(&mut self.agent_reads)
            .into_iter()
            .partition(|r| r.key == *key);
        self.agent_reads = rest;
        waiting
    }

    /// Lays a message out for an agent, on a background thread (it may be
    /// HTML: `mail::in_background`), and answers.
    fn agent_render(
        &self,
        key: (usize, String, u32),
        raw: Vec<u8>,
        max: usize,
        reply: tokio::sync::oneshot::Sender<agent::Answer>,
    ) {
        let (i, folder, uid) = key;
        let message = self
            .store
            .message(&self.accounts[i].id, &folder, uid)
            .ok()
            .flatten();
        let header = serde_json::json!({
            "id": self.agent_id(i, &folder, uid),
            "account": self.accounts[i].account.shown_name(),
            "folder": self.folder_name(i, &folder),
            "date": message.as_ref().map(|m| agent_date(m.summary.date)),
            "tags": message.as_ref().map(|m| m.flags.tags.clone()),
            "unread": message.as_ref().map(|m| !m.flags.seen),
            "starred": message.as_ref().map(|m| m.flags.flagged),
        });
        mail::in_background(
            move || agent_text(&raw, max, header),
            move |done| {
                let _ =
                    reply.send(done.unwrap_or_else(|| Err("This message couldn't be read".into())));
            },
        );
    }

    /// Takes messages off the list (moved or deleted), keeping the cursor
    /// in place.
    fn take_off_list(&mut self, keys: &[(usize, String, u32)]) {
        let keys: HashSet<_> = keys.iter().cloned().collect();
        self.items.retain(|it| !keys.contains(&it.key()));
        self.marked.retain(|k| !keys.contains(k));
        if self
            .reader
            .as_ref()
            .is_some_and(|r| keys.contains(&r.key()))
        {
            self.reader = None;
            if self.focus == Focus::Reader {
                self.focus = Focus::List;
            }
        }
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        self.moved_at = Some(Instant::now());
    }

    /// Moves messages to `to` (a folder of the same account).
    fn move_messages(&mut self, account: usize, folder: &str, uids: &[u32], to: &str) {
        let id = self.accounts[account].id.clone();
        let keys: Vec<_> = uids
            .iter()
            .map(|u| (account, folder.to_string(), *u))
            .collect();
        let unread = self.unread_among(account, folder, uids);
        let mut changed: Vec<Changed> = keys.iter().filter_map(|k| self.changed(k)).collect();
        for c in &mut changed {
            c.moved_to = to.to_string();
        }
        if self.accounts[account].is_pop() || self.demo {
            let moved = self
                .store
                .move_local(&id, folder, uids, to)
                .unwrap_or_default();
            for c in &mut changed {
                c.new_uid = moved
                    .iter()
                    .find(|(old, _)| *old == c.uid)
                    .map_or(0, |(_, new)| *new);
            }
            self.refresh_counts(account, to);
        } else {
            let _ = self.store.remove(&id, folder, uids);
            for key in &keys {
                self.local_removed.insert(key.clone(), Instant::now());
            }
            self.send(
                account,
                Request::Move {
                    folder: folder.to_string(),
                    uids: uids.to_vec(),
                    to: to.to_string(),
                    uidvalidity: self.uidvalidity(account, folder),
                },
            );
        }
        self.note_removed(account, folder, uids.len(), unread);
        self.take_off_list(&keys);
        self.note_change(What::Move, changed);
    }

    /// How many of these messages are unread, before they go.
    fn unread_among(&self, account: usize, folder: &str, uids: &[u32]) -> u32 {
        let id = &self.accounts[account].id;
        uids.iter()
            .filter(|uid| {
                self.store
                    .message(id, folder, **uid)
                    .ok()
                    .flatten()
                    .is_some_and(|m| !m.flags.seen)
            })
            .count() as u32
    }

    /// A folder's counts after `n` messages (`unread` of them unread) left.
    fn note_removed(&mut self, account: usize, folder: &str, n: usize, unread: u32) {
        if self.accounts[account].is_pop() || self.demo {
            return self.refresh_counts(account, folder);
        }
        let id = self.accounts[account].id.clone();
        if let Some(f) = self.accounts[account]
            .folders
            .iter_mut()
            .find(|f| f.name == folder)
        {
            f.total = f.total.saturating_sub(n as u32);
            f.unread = f.unread.saturating_sub(unread);
            let _ = self.store.set_counts(&id, folder, f.unread, f.total);
        }
    }

    /// `d`: to Trash, or for good from Trash (asking first). Each account
    /// and folder that needs a question gets its own, one after another.
    fn delete(&mut self) {
        let targets = self.targets();
        self.deletes_waiting.clear();
        for (account, folder, uids) in Self::grouped(&targets) {
            let trash = self.accounts[account]
                .folder_for(Role::Trash)
                .map(|f| f.name.clone());
            let forever = trash.as_deref().is_none_or(|t| t == folder);
            let uidvalidity = self.uidvalidity(account, &folder);
            match trash.filter(|_| !forever) {
                None => self.deletes_waiting.push_back(Confirm::DeleteForever {
                    account,
                    folder,
                    uidvalidity,
                    uids,
                }),
                Some(_) if self.settings.confirm_delete => {
                    self.deletes_waiting.push_back(Confirm::Delete {
                        account,
                        folder,
                        uids,
                        uidvalidity,
                    })
                }
                Some(trash) => {
                    self.move_messages(account, &folder, &uids, &trash);
                    self.info(format!("{} moved to Trash", plural(uids.len(), "message")));
                }
            }
        }
        self.ask_next_delete();
    }

    /// Asks the next of `d`'s questions, if there's one and no other.
    fn ask_next_delete(&mut self) {
        if self.confirm.is_none()
            && let Some(next) = self.deletes_waiting.pop_front()
        {
            self.confirm(next);
        }
    }

    fn delete_forever(&mut self, account: usize, folder: &str, uids: &[u32]) {
        let id = self.accounts[account].id.clone();
        let keys: Vec<_> = uids
            .iter()
            .map(|u| (account, folder.to_string(), *u))
            .collect();
        let unread = self.unread_among(account, folder, uids);
        // Gone for good: the changes kept about them keep nothing of them.
        let _ = self.store.forget_changed(&id, folder, uids);
        if self.accounts[account].is_pop() {
            let _ = self.store.mark_pop_removal(&id, folder, uids);
            let _ = self.store.remove(&id, folder, uids);
            self.refresh_counts(account, folder);
            if !self.demo {
                self.pop_check(account);
            }
        } else {
            let _ = self.store.remove(&id, folder, uids);
            for key in &keys {
                self.local_removed.insert(key.clone(), Instant::now());
            }
            // Kept until the server says it's done, so quitting first
            // doesn't lose it; with any still pending in the folder, which
            // a plain EXPUNGE must count as ours.
            let uidvalidity = self.uidvalidity(account, folder);
            let _ = self
                .store
                .add_pending_deletes(&id, folder, uidvalidity, uids);
            let ours = self
                .store
                .pending_deletes(&id)
                .unwrap_or_default()
                .into_iter()
                .find(|(f, v, _)| f == folder && *v == uidvalidity)
                .map_or_else(|| uids.to_vec(), |(_, _, all)| all);
            self.send(
                account,
                Request::Delete {
                    folder: folder.to_string(),
                    uids: ours,
                    uidvalidity,
                },
            );
            self.note_removed(account, folder, uids.len(), unread);
        }
        self.take_off_list(&keys);
        self.info(format!("{} deleted", plural(uids.len(), "message")));
    }

    /// `e` archives, `!` moves to Junk: to the folder with `role`.
    fn move_to_role(&mut self, role: Role) {
        let targets = self.targets();
        for (account, folder, uids) in Self::grouped(&targets) {
            let target = self.accounts[account]
                .folder_for(role)
                .or_else(|| {
                    // Gmail archives to All Mail.
                    (role == Role::Archive)
                        .then(|| self.accounts[account].folder_for(Role::All))
                        .flatten()
                })
                .map(|f| f.name.clone());
            let Some(target) = target else {
                let what = if role == Role::Junk {
                    "junk"
                } else {
                    "archive"
                };
                self.error(format!(
                    "{} has no {what} folder: set {what}_folder in accounts.toml",
                    self.account_name(account)
                ));
                return;
            };
            if target == folder {
                continue;
            }
            self.move_messages(account, &folder, &uids, &target);
            let done = if role == Role::Junk {
                "marked as junk"
            } else {
                "archived"
            };
            self.info(format!("{} {done}", plural(uids.len(), "message")));
        }
    }

    /// `m`: picks a folder to move to.
    fn pick_folder(&mut self) {
        let targets = self.targets();
        let groups = Self::grouped(&targets);
        let [(account, folder, uids)] = groups.as_slice() else {
            if groups.len() > 1 {
                self.error("Move messages of one account and folder at a time");
            }
            return;
        };
        let folders: Vec<&Folder> = self.accounts[*account]
            .folders
            .iter()
            .filter(|f| f.selectable && &f.name != folder)
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::MoveTo {
                account: *account,
                folder: folder.clone(),
                uids: uids.clone(),
                uidvalidity: self.uidvalidity(*account, folder),
                targets: folders.iter().map(|f| f.name.clone()).collect(),
            },
            title: format!(" Move {} to ", plural(uids.len(), "message")),
            items: folders.iter().map(|f| folder_label(f)).collect(),
            labels: Vec::new(),
            selected: 0,
        });
    }

    // Writing.

    fn compose_account(&self) -> usize {
        if let Some(reader) = &self.reader
            && self.focus == Focus::Reader
        {
            return reader.account;
        }
        match &self.place {
            Place::Folder { account, .. } => *account,
            Place::AllInboxes => self.selected_item().map_or(0, |it| it.account),
        }
    }

    fn compose_new(&mut self) {
        if self.accounts.is_empty() {
            return;
        }
        let account = self.compose_account();
        let draft = Draft {
            body: compose::signature(&self.accounts[account].account),
            ..Draft::default()
        };
        // A reply waiting for its message is given up: it won't open later
        // over this one.
        self.pending = None;
        self.composer = Some(Composer::new(account, Kind::New, draft));
    }

    /// Starts a reply, forward or the editing of a draft from the message
    /// `key`, once its text is here.
    fn start_from(&mut self, key: (usize, String, u32), pending: Pending) {
        let id = self.accounts[key.0].id.clone();
        if let Some(raw) = self
            .reader
            .as_ref()
            .filter(|r| r.key() == key)
            .and_then(|r| r.raw.clone())
            .or_else(|| {
                self.store
                    .body(&id, &key.1, key.2)
                    .ok()
                    .flatten()
                    .map(Arc::new)
            })
        {
            return self.draft_from(key, raw, pending);
        }
        if self.demo || self.accounts[key.0].is_pop() {
            return self.error("That message isn't downloaded");
        }
        self.info("Downloading the message…");
        self.send(
            key.0,
            Request::Body {
                folder: key.1.clone(),
                uid: key.2,
                uidvalidity: self.uidvalidity(key.0, &key.1),
            },
        );
        self.pending = Some((key, pending));
    }

    fn draft_from(&mut self, key: (usize, String, u32), raw: Arc<Vec<u8>>, pending: Pending) {
        let account = self.accounts[key.0].account.clone();
        let kept = self.kept(&key);
        let tx = self.tx.clone();
        let account_id = self.accounts[key.0].id.clone();
        mail::in_background(
            move || {
                let parsed = mail::parse(&raw)?;
                Some(match pending {
                    Pending::Reply { all } => {
                        (Kind::Reply, compose::reply(&parsed, &account, all), None)
                    }
                    Pending::Forward => (
                        Kind::Forward,
                        compose::forward(&parsed, &raw, &account),
                        None,
                    ),
                    Pending::EditDraft => (
                        Kind::Draft,
                        compose::from_saved(&parsed, &raw),
                        Some(kept.clone()),
                    ),
                })
                .map(|(kind, draft, draft_of)| (kind, draft, draft_of, kept))
            },
            move |result| {
                let (kind, draft, draft_of, replying) = match result.flatten() {
                    Some((kind, draft, draft_of, kept)) => {
                        let replying = (kind == Kind::Reply).then_some(kept);
                        (kind, Some(draft), draft_of, replying)
                    }
                    None => (Kind::New, None, None, None),
                };
                let _ = tx.send(AppEvent::Drafted {
                    account: account_id,
                    kind,
                    draft,
                    replying,
                    draft_of,
                });
            },
        );
    }

    fn reply(&mut self, pending: Pending) {
        let targets = self.targets();
        let [key] = targets.as_slice() else {
            return;
        };
        self.start_from(key.clone(), pending);
    }

    fn on_composer_key(&mut self, key: KeyEvent) {
        let Some(composer) = self.composer.as_mut() else {
            return;
        };
        match composer.on_key(key, &self.store) {
            composer::Action::None => {}
            composer::Action::Send => self.send_message(None),
            composer::Action::SaveDraft => self.save_draft(),
            composer::Action::Close => {
                if composer.changed() {
                    self.confirm(Confirm::CloseComposer);
                } else {
                    self.composer = None;
                }
            }
            composer::Action::Attach => {
                self.prompt = Some(Prompt::new(PromptKind::Attach, ""));
            }
            composer::Action::Edit => self.edit_request = true,
            composer::Action::SwitchAccount(by) => {
                let n = self.accounts.len() as isize;
                if n > 1 {
                    composer.account = (composer.account as isize + by).rem_euclid(n) as usize;
                }
            }
        }
    }

    /// Ctrl-e: the text in your editor.
    fn run_editor(&mut self) {
        let Some(composer) = self.composer.as_mut() else {
            return;
        };
        let Some(argv) = tools::editor(&self.settings.editor) else {
            self.error("No editor found: set editor in settings.toml, or $EDITOR");
            return;
        };
        // The data folder's drafts/; without one (the demo), a new folder of
        // the user's own, never a shared one someone could make first.
        let (dir, temporary) = match &self.data_dir {
            Some(data) => (data.join("drafts"), false),
            None => match tools::private_temp_dir() {
                Ok(dir) => (dir, true),
                Err(e) => return self.error(format!("{e:#}")),
            },
        };
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
        ratatui::restore();
        let result = tools::edit(&argv, &dir, &composer.draft().body);
        if temporary {
            let _ = std::fs::remove_dir(&dir);
        }
        match result {
            Ok(text) => {
                let text = text::clean(&text);
                let lines: Vec<String> = text
                    .trim_end_matches('\n')
                    .split('\n')
                    .map(str::to_string)
                    .collect();
                let mut body = TextArea::new(lines);
                body.set_cursor_line_style(ratatui::style::Style::default());
                body.set_wrap_mode(ratatui_textarea::WrapMode::WordOrGlyph);
                composer.body = body;
                composer.field = composer::Field::Body;
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    /// Sends the composer's message; `confirmed` is the digest the user
    /// said yes to.
    fn send_message(&mut self, confirmed: Option<u64>) {
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        let i = composer.account;
        let account = self.accounts[i].account.clone();
        let draft = composer.draft();
        let message_id = match compose::new_message_id(&account.email) {
            Ok(id) => id,
            Err(e) => return self.error(format!("Not sent: {e:#}")),
        };
        let date = now();
        let built = compose::build(&draft, &account, &message_id, date, false).and_then(|sent| {
            Ok((
                sent,
                compose::build(&draft, &account, &message_id, date, true)?,
            ))
        });
        let (sent, copy) = match built {
            Ok(both) => both,
            Err(e) => return self.error(format!("{e:#}")),
        };
        if sent.rcpts.is_empty() {
            return self.error("Who is it to? Write an address in To");
        }
        let print = draft.print() ^ (i as u64).rotate_left(32);
        // What a yes was for may not be what's in the composer now (a
        // paste, or another message in its place): then ask about this one.
        let changed = confirmed.is_some_and(|asked| asked != print);
        let unasked = confirmed != Some(print);
        if unasked && (self.settings.confirm_send || draft.subject.trim().is_empty() || changed) {
            // Sent without a subject only after a yes, always.
            if changed {
                self.info("The message changed: check it again");
            }
            return self.confirm(Confirm::Send {
                print,
                subject: draft.subject.trim().to_string(),
                recipients: sent.rcpts.clone(),
                attachments: draft.attachments.iter().map(|a| a.name()).collect(),
            });
        }
        let replying = composer.replying.clone();
        let draft_of = composer.draft_of.clone();
        let copy = SentCopy {
            raw: copy.raw,
            to: sent.to.clone(),
            replying,
            draft_of,
        };
        let account_id = self.accounts[i].id.clone();
        if let Some(composer) = self.composer.as_mut() {
            composer.sending = true;
        }
        if self.demo {
            let _ = self.tx.send(AppEvent::Sent {
                account: account_id,
                result: Ok(()),
                needs_password: false,
                sent: copy,
            });
            return;
        }
        let Some(worker) = &self.accounts[i].worker else {
            // The composer stays usable: nothing is on its way.
            if let Some(composer) = self.composer.as_mut() {
                composer.sending = false;
            }
            return self.error("This account can't send: see the error at start");
        };
        let creds = worker.creds.clone();
        let endpoint = worker.endpoints.smtp.clone();
        let tx = self.tx.clone();
        self.info("Sending…");
        tokio::spawn(async move {
            let result = worker::send(creds, endpoint, sent.from, sent.rcpts, sent.raw).await;
            let needs_password = result.as_ref().err().is_some_and(worker::needs_password);
            let _ = tx.send(AppEvent::Sent {
                account: account_id,
                result: result.map_err(|e| text::one_line(&format!("{e:#}"), 300)),
                needs_password,
                sent: copy,
            });
        });
    }

    fn on_sent(
        &mut self,
        id: &str,
        result: Result<(), String>,
        needs_password: bool,
        sent: SentCopy,
    ) {
        let Some(i) = self.account_index(id) else {
            return;
        };
        if let Err(e) = result {
            if let Some(composer) = self.composer.as_mut() {
                composer.sending = false;
            }
            self.error(format!("Not sent: {e}"));
            if needs_password
                && self.accounts[i].account.password != PasswordFrom::Command
                && !crate::oauth::built_in(&self.accounts[i].account)
            {
                self.prompt = Some(Prompt::new(PromptKind::Password(i), ""));
            }
            return;
        }
        self.composer = None;
        let n = sent.to.len();
        self.info(format!("Sent to {}", plural(n, "recipient")));
        let _ = self.store.remember(&sent.to, true, now());
        // A copy in Sent.
        let pop = self.accounts[i].is_pop() || self.demo;
        if pop {
            let folder = self.accounts[i]
                .folder_for(Role::Sent)
                .map(|f| f.name.clone())
                .unwrap_or_else(|| "Sent".into());
            let seen = Flags {
                seen: true,
                ..Flags::default()
            };
            let id = self.accounts[i].id.clone();
            if let Ok(uid) = self.store.add_local(&id, &folder, &sent.raw, seen, None) {
                self.make_previews(i, vec![(folder.clone(), uid, Arc::new(sent.raw))]);
            }
            self.refresh_counts(i, &folder);
            if self.shows(i, &folder) {
                self.reload();
            }
        } else if self.accounts[i].account.save_sent() {
            // With no Sent folder, the worker makes one.
            let folder = self.accounts[i]
                .folder_for(Role::Sent)
                .map(|f| f.name.clone())
                .unwrap_or_else(|| "Sent".into());
            let id = self.next_append_id(Append::SentCopy);
            self.send(
                i,
                Request::Append {
                    folder,
                    raw: sent.raw,
                    flags: Flags {
                        seen: true,
                        ..Flags::default()
                    },
                    id,
                    // Made once the connection is back, if it's down.
                    retry: true,
                },
            );
        }
        if let Some(key) = sent.replying.and_then(|k| self.resolve(&k)) {
            self.set_flag(&[key], FlagKind::Answered, true);
        }
        if sent.draft_of.is_some() {
            self.replace_draft(sent.draft_of);
            self.info(format!("Sent to {}", plural(n, "recipient")));
        }
    }

    /// Ctrl-d: keeps what's written in Drafts and closes the composer.
    fn save_draft(&mut self) {
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        let i = composer.account;
        let account = self.accounts[i].account.clone();
        let draft = composer.draft();
        let message_id = match compose::new_message_id(&account.email) {
            Ok(id) => id,
            Err(e) => return self.error(format!("Not saved: {e:#}")),
        };
        let built = match compose::build(&draft, &account, &message_id, now(), true) {
            Ok(built) => built,
            Err(e) => return self.error(format!("Not saved: {e:#}")),
        };
        let flags = Flags {
            seen: true,
            draft: true,
            ..Flags::default()
        };
        let drafts = self.accounts[i]
            .folder_for(Role::Drafts)
            .map(|f| f.name.clone());
        let pop = self.accounts[i].is_pop() || self.demo;
        let draft_of = composer.draft_of.clone();
        if pop {
            let folder = drafts.unwrap_or_else(|| "Drafts".into());
            let id = self.accounts[i].id.clone();
            if let Ok(uid) = self.store.add_local(&id, &folder, &built.raw, flags, None) {
                self.make_previews(i, vec![(folder.clone(), uid, Arc::new(built.raw))]);
            }
            self.refresh_counts(i, &folder);
            if self.shows(i, &folder) {
                self.reload();
            }
            self.composer = None;
            self.replace_draft(draft_of);
            self.info("Saved in Drafts");
            return;
        }
        if self.accounts[i].worker.is_none()
            || !matches!(self.accounts[i].state, State::Online | State::Connecting)
        {
            return self.error(format!(
                "{} isn't connected, so the draft can't be saved there now",
                self.account_name(i)
            ));
        }
        // The composer waits until the server has it (Event::Appended).
        let id = self.next_append_id(Append::Draft { draft_of });
        self.send(
            i,
            Request::Append {
                // With no Drafts folder, the worker makes one.
                folder: drafts.unwrap_or_else(|| "Drafts".into()),
                raw: built.raw,
                flags,
                id,
                retry: false,
            },
        );
        if let Some(composer) = self.composer.as_mut() {
            composer.saving = Some(id);
            composer.sending = true;
        }
        self.info("Saving in Drafts…");
    }

    fn next_append_id(&mut self, what: Append) -> u64 {
        self.next_append += 1;
        self.appends.insert(self.next_append, what);
        self.next_append
    }

    fn attach(&mut self, path: &str) {
        let path = unquote(path);
        if path.is_empty() {
            return;
        }
        let path = PathBuf::from(path);
        if !path.is_absolute() || crate::config::maybe_remote(&path) {
            return self.error("Give the file's full path (or drop it on the window)");
        }
        match compose::Attachment::snapshot(&path) {
            Ok(attachment) => {
                if let Some(composer) = self.composer.as_mut() {
                    composer.attachments.push(attachment);
                }
                // A path through a link says where the file really is.
                if let Ok(real) = std::fs::canonicalize(&path)
                    && real != path
                {
                    self.info(format!(
                        "Attached {}, which is {}",
                        crate::config::shown(&path),
                        crate::config::shown(&real)
                    ));
                }
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    // Reading.

    /// `o`: the message's links.
    fn pick_link(&mut self) {
        let Some(body) = self.reader.as_ref().and_then(|r| r.body.as_ref()) else {
            return;
        };
        if body.links.is_empty() {
            return self.info("No links in this message");
        }
        let items = body.links.iter().map(|l| text::one_line(l, 300)).collect();
        let labels = (body.links.iter().enumerate())
            .map(|(n, l)| (link_host(l), body.link_text(n)))
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Links,
            title: " Open a link ".into(),
            items,
            labels,
            selected: 0,
        });
    }

    /// `U`: the List-Unsubscribe link of a mailing list's message: a web
    /// page opens in the browser once the user has seen where it goes; an
    /// address gets a message to write.
    fn unsubscribe(&mut self) {
        let Some(reader) = self.reader.as_ref() else {
            return;
        };
        let account = reader.account;
        let Some(link) = reader.parsed.as_ref().and_then(|p| p.unsubscribe.clone()) else {
            return self.info("This message doesn't say how to unsubscribe");
        };
        if let Some(mut draft) = mailto_draft(&link) {
            if draft.subject.is_empty() {
                draft.subject = "unsubscribe".into();
            }
            draft.body = "unsubscribe\n".into();
            self.pending = None;
            self.composer = Some(Composer::new(account, Kind::New, draft));
        } else {
            self.confirm(Confirm::OpenLink { url: link });
        }
    }

    /// `A`: the message's attachments.
    fn pick_attachment(&mut self) {
        let Some(parsed) = self.reader.as_ref().and_then(|r| r.parsed.as_ref()) else {
            return;
        };
        if parsed.attachments.is_empty() {
            return self.info("No attachments in this message");
        }
        let items = parsed
            .attachments
            .iter()
            .map(|a| format!("{}  ({}, {})", a.name, a.mime, mail::size(a.size)))
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Attachments,
            title: " Attachments ".into(),
            items,
            labels: Vec::new(),
            selected: 0,
        });
    }

    /// Saves attachment `index` of the reader's message to the downloads
    /// folder; with `open`, into tuimail's own folder and opens it.
    /// Saves or opens attachment `index`, which was listed as `listed`;
    /// `confirmed` when the user said yes to opening it.
    fn extract(&mut self, index: u32, listed: &str, open: bool, confirmed: bool) {
        let Some(raw) = self.reader.as_ref().and_then(|r| r.raw.clone()) else {
            return;
        };
        if self.demo {
            return self.info("The demo's files are made up: nothing is saved");
        }
        let listed = listed.to_string();
        let dir = if open {
            self.data_dir.as_ref().map(|d| d.join("opened"))
        } else {
            self.settings.downloads()
        };
        let Some(dir) = dir else {
            return self.error("No folder to save in");
        };
        let tx = self.tx.clone();
        mail::in_background(
            move || {
                let (name, bytes) =
                    mail::attachment(&raw, index).ok_or("That attachment can't be read")?;
                // The message under the reader may have been fetched again
                // since its files were listed: the file is the one shown, or
                // nothing.
                if name != listed {
                    return Err(
                        "The message changed since its files were listed: open it again"
                            .to_string(),
                    );
                }
                tools::save_new(&dir, &name, &bytes).map_err(|e| format!("{e:#}"))
            },
            move |result| {
                let result = result.unwrap_or_else(|| Err("That attachment can't be read".into()));
                let _ = tx.send(AppEvent::Extracted {
                    open,
                    confirmed,
                    result,
                });
            },
        );
    }

    /// Opens a web link in the browser; a `mailto:` one is written here,
    /// from the reader's account.
    fn open_link(&mut self, url: &str) {
        if !mail::usable_link(url) {
            return self.error("tuimail opens only http, https and mailto links");
        }
        if let Some(draft) = mailto_draft(url) {
            let Some(account) = self.reader.as_ref().map(|r| r.account) else {
                return;
            };
            self.pending = None;
            self.composer = Some(Composer::new(account, Kind::New, draft));
            return;
        }
        if self.demo {
            return self.info(format!("Would open {}", text::one_line(url, 80)));
        }
        match tools::open_externally(url) {
            Ok(()) => self.info(format!("Opened {}", link_host(url))),
            Err(e) => self.error(format!("Couldn't open it: {e}")),
        }
    }

    // Keys.

    /// Terminal events that were waiting together. A paste is meant to come
    /// as one `Event::Paste` (bracketed paste), but on Windows, and from a
    /// terminal that doesn't bracket pastes, it comes as keystrokes, all at
    /// once: a run of different keys read together is pasted text, never
    /// commands. Where something is being written it's added as text;
    /// elsewhere it's ignored. A key held down acts as usual.
    fn on_terminal_events(&mut self, events: Vec<Event>) {
        let presses: Vec<KeyEvent> = events
            .iter()
            .filter_map(|event| match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => Some(*key),
                _ => None,
            })
            .collect();
        let repeated = presses.windows(2).all(|w| w[0].code == w[1].code);
        if presses.len() < 2 || repeated {
            return events.into_iter().for_each(|event| self.on_terminal(event));
        }
        for event in events.into_iter().filter(|e| !matches!(e, Event::Key(_))) {
            self.on_terminal(event);
        }
        // Under a question, keys answer it: nothing is written.
        let writing = self.confirm.is_none()
            && (self.prompt.is_some() || self.composer.is_some() || self.setup.is_some());
        if !writing {
            if presses.len() >= PASTED_KEYS {
                return self.info("Pasted text goes where you write: press c, / or Tab first");
            }
            return presses.into_iter().for_each(|key| self.on_key(key));
        }
        let typed = |key: &KeyEvent| {
            matches!(key.code, KeyCode::Char(_))
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        };
        let in_body = self
            .composer
            .as_ref()
            .is_some_and(|c| c.field == composer::Field::Body)
            && self.prompt.is_none();
        let (last, before) = presses.split_last().expect("two or more");
        let submit = last.code == KeyCode::Enter && before.iter().all(typed) && !in_body;
        let text: String = presses
            .iter()
            .filter_map(|key| match key.code {
                KeyCode::Char(c) if typed(key) => Some(c),
                KeyCode::Enter if in_body => Some('\n'),
                KeyCode::Enter | KeyCode::Tab => Some(' '),
                _ => None,
            })
            .collect();
        self.on_terminal(Event::Paste(text));
        if submit {
            self.on_key(*last);
        }
    }

    fn on_terminal(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.on_key(key),
            // A paste never answers a question, nor changes what it's about.
            Event::Paste(_) if self.confirm.is_some() => {
                self.info("Answer the question first: y, or any other key")
            }
            Event::Paste(pasted) => {
                let pasted = text::first_chars(&pasted, 4 << 20).to_string();
                if let Some(prompt) = self.prompt.as_mut() {
                    let room = MAX_PROMPT.saturating_sub(prompt.text().chars().count());
                    let line = if matches!(prompt.kind, PromptKind::Password(_)) {
                        pasted
                            .trim_matches(['\r', '\n'])
                            .chars()
                            .take(room)
                            .collect()
                    } else {
                        text::one_line(&pasted, room)
                    };
                    prompt.input.insert_str(line);
                } else if let Some(setup) = self.setup.as_mut() {
                    setup.paste(&pasted);
                } else if let Some(composer) = self.composer.as_mut() {
                    // A file dropped on the window attaches it.
                    let path = unquote(&pasted);
                    let file = !path.contains('\n')
                        && PathBuf::from(&path).is_absolute()
                        && !crate::config::maybe_remote(std::path::Path::new(&path))
                        && std::fs::metadata(&path).is_ok_and(|m| m.is_file());
                    if file && composer.field != composer::Field::Body {
                        self.attach(&path);
                    } else {
                        composer.paste(&pasted);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.handle_key(key);
        // What the key changed is one change, to undo at once.
        let _ = self.keep_change();
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl
            && key.code == KeyCode::Char('c')
            && self.composer.is_none()
            && self.setup.is_none()
            && self.confirm.is_none()
        {
            return self.ask_quit();
        }
        if self.confirm.is_some() {
            return self.on_confirm_key(key);
        }
        if self.prompt.is_some() {
            return self.on_prompt_key(key);
        }
        if self.picker.is_some() {
            return self.on_picker_key(key);
        }
        if self.help.is_some() {
            return self.on_help_key(key, ctrl);
        }
        if self.setup.is_some() {
            return self.on_setup_key(key);
        }
        if self.composer.is_some() {
            if ctrl && key.code == KeyCode::Char('c') {
                return self.on_composer_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
            return self.on_composer_key(key);
        }
        if std::mem::take(&mut self.pending_g) && key.code == KeyCode::Char('g') {
            return self.go_to(0);
        }
        if !ctrl {
            match key.code {
                KeyCode::Char('q') if self.focus != Focus::Reader || self.wide.get() => {
                    return self.ask_quit();
                }
                KeyCode::Char('?') => return self.help = Some(HelpMenu::new(HelpTab::Keys)),
                KeyCode::Char('O') => return self.help = Some(HelpMenu::new(HelpTab::Settings)),
                KeyCode::Char('c') => return self.compose_new(),
                KeyCode::Char('/') => {
                    let text = self.search.clone().unwrap_or_default();
                    self.prompt = Some(Prompt::new(PromptKind::Search, &text));
                    return;
                }
                KeyCode::Char('R') => {
                    for i in 0..self.accounts.len() {
                        if let Some(worker) = &self.accounts[i].worker {
                            worker.creds.retry();
                        }
                        self.check(i, true);
                    }
                    if !self.demo {
                        self.info("Checking for mail…");
                    }
                    return;
                }
                KeyCode::Char('+') => {
                    self.setup = Some(Setup::default());
                    return;
                }
                KeyCode::Char('z') => return self.undo_last(),
                KeyCode::Char('Z') => return self.pick_change(),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::List,
                    Focus::List if self.reader.is_some() => Focus::Reader,
                    Focus::List | Focus::Reader => Focus::Sidebar,
                };
                return;
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Sidebar if self.reader.is_some() => Focus::Reader,
                    Focus::Sidebar | Focus::Reader => Focus::List,
                    Focus::List => Focus::Sidebar,
                };
                return;
            }
            _ => {}
        }
        match self.focus {
            Focus::Sidebar => self.on_sidebar_key(key),
            Focus::List => self.on_list_key(key),
            Focus::Reader => self.on_reader_key(key),
        }
    }

    /// Keys acting on messages, in the list and the reader alike.
    fn on_message_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match key.code {
            KeyCode::Char('r') => self.reply(Pending::Reply { all: false }),
            KeyCode::Char('a') => self.reply(Pending::Reply { all: true }),
            KeyCode::Char('f') => self.reply(Pending::Forward),
            KeyCode::Char('d') | KeyCode::Delete => self.delete(),
            KeyCode::Char('e') => self.move_to_role(Role::Archive),
            KeyCode::Char('!') => self.move_to_role(Role::Junk),
            KeyCode::Char('m') => self.pick_folder(),
            KeyCode::Char('t') => self.pick_tag(),
            KeyCode::Char('s') => self.toggle_flag(FlagKind::Flagged),
            KeyCode::Char('u') => self.toggle_flag(FlagKind::Seen),
            _ => return false,
        }
        true
    }

    fn on_list_key(&mut self, key: KeyEvent) {
        if self.on_message_key(key) {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = usize::from(self.list_rows.get().max(2));
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.select(self.selected + 1),
            KeyCode::Char('k') | KeyCode::Up => self.select(self.selected.saturating_sub(1)),
            KeyCode::Char('d') if ctrl => self.select(self.selected + page / 2),
            KeyCode::Char('u') if ctrl => self.select(self.selected.saturating_sub(page / 2)),
            KeyCode::PageDown => self.select(self.selected + page),
            KeyCode::PageUp => self.select(self.selected.saturating_sub(page)),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Home => self.go_to(0),
            KeyCode::Char('G') | KeyCode::End => self.go_to(usize::MAX),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_selected(true),
            KeyCode::Char(' ') => {
                if let Some(key) = self.selected_item().map(Item::key)
                    && !self.marked.remove(&key)
                {
                    self.marked.insert(key);
                }
                self.select(self.selected + 1);
            }
            KeyCode::Char('*') => {
                if self.marked.len() == self.items.len() {
                    self.marked.clear();
                } else {
                    self.marked = self.items.iter().map(Item::key).collect();
                }
            }
            KeyCode::Esc if !self.marked.is_empty() => self.marked.clear(),
            KeyCode::Esc if self.search.is_some() => {
                self.search = None;
                self.reload();
            }
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                self.focus = Focus::Sidebar;
            }
            _ => {}
        }
    }

    fn go_to(&mut self, i: usize) {
        match self.focus {
            Focus::Sidebar => {
                self.sidebar_selected = i.min(self.sidebar_rows().len().saturating_sub(1))
            }
            Focus::List => self.select(i),
            Focus::Reader => {
                if let Some(reader) = self.reader.as_mut() {
                    reader.scroll = if i == 0 { 0 } else { usize::MAX / 2 };
                }
            }
        }
    }

    fn on_reader_key(&mut self, key: KeyEvent) {
        if self.on_message_key(key) {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = usize::from(self.reader_rows.get().max(2));
        let Some(reader) = self.reader.as_mut() else {
            self.focus = Focus::List;
            return;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => reader.scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => reader.scroll = reader.scroll.saturating_sub(1),
            KeyCode::Char('d') if ctrl => reader.scroll += page / 2,
            KeyCode::Char('u') if ctrl => reader.scroll = reader.scroll.saturating_sub(page / 2),
            KeyCode::Char(' ') | KeyCode::PageDown => reader.scroll += page.saturating_sub(2),
            KeyCode::Char('b') | KeyCode::PageUp => {
                reader.scroll = reader.scroll.saturating_sub(page.saturating_sub(2))
            }
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Home => reader.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => reader.scroll = usize::MAX / 2,
            KeyCode::Char('J') | KeyCode::Char('n') => self.step_message(1),
            KeyCode::Char('K') | KeyCode::Char('p') => self.step_message(-1),
            KeyCode::Char('o') => self.pick_link(),
            KeyCode::Char('U') => self.unsubscribe(),
            KeyCode::Char('A') | KeyCode::Char('w') => self.pick_attachment(),
            KeyCode::Char('H') => {
                let now_plain = reader.plain.unwrap_or(self.settings.prefer_plain);
                reader.plain = Some(!now_plain);
                reader.scroll = 0;
                self.render();
            }
            KeyCode::Esc
            | KeyCode::Char('h')
            | KeyCode::Left
            | KeyCode::Backspace
            | KeyCode::Char('q') => {
                self.focus = Focus::List;
                if !self.preview_shown() {
                    self.reader = None;
                }
            }
            _ => {}
        }
    }

    /// `J` / `K` in the reader: the next or previous message in the list.
    fn step_message(&mut self, by: isize) {
        let next = self.selected as isize + by;
        if next < 0 || next as usize >= self.items.len() {
            return;
        }
        self.select(next as usize);
        self.moved_at = None;
        self.open_selected(true);
    }

    fn on_sidebar_key(&mut self, key: KeyEvent) {
        let rows = self.sidebar_rows();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.sidebar_selected = (self.sidebar_selected + 1).min(last);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.sidebar_selected = self.sidebar_selected.saturating_sub(1);
            }
            KeyCode::Char('d') if ctrl => {
                self.sidebar_selected = (self.sidebar_selected + 10).min(last)
            }
            KeyCode::Char('u') if ctrl => {
                self.sidebar_selected = self.sidebar_selected.saturating_sub(10)
            }
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Home => self.sidebar_selected = 0,
            KeyCode::Char('G') | KeyCode::End => self.sidebar_selected = last,
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right | KeyCode::Char(' ') => {
                match rows.get(self.sidebar_selected) {
                    Some(Row::AllInboxes) => {
                        self.open_place(Place::AllInboxes);
                        self.focus = Focus::List;
                    }
                    Some(Row::Account(i)) => {
                        let i = *i;
                        if matches!(self.accounts[i].state, State::NeedPassword { .. })
                            && key.code == KeyCode::Enter
                        {
                            // One that signs in here takes no password: the
                            // browser's sign-in, as P.
                            if crate::oauth::built_in(&self.accounts[i].account) {
                                self.sign_in(i);
                            } else {
                                self.prompt = Some(Prompt::new(PromptKind::Password(i), ""));
                            }
                        } else {
                            self.accounts[i].expanded = !self.accounts[i].expanded;
                        }
                    }
                    Some(Row::Folder(i, f, _)) => {
                        let folder = &self.accounts[*i].folders[*f];
                        if folder.selectable {
                            let place = Place::Folder {
                                account: *i,
                                folder: folder.name.clone(),
                            };
                            self.reader = None;
                            self.open_place(place);
                            self.focus = Focus::List;
                        }
                    }
                    Some(Row::Add) => self.setup = Some(Setup::default()),
                    None => {}
                }
            }
            KeyCode::Char('x') => {
                if let Some(Row::Account(i)) = rows.get(self.sidebar_selected) {
                    self.confirm(Confirm::RemoveAccount(*i));
                }
            }
            KeyCode::Char('P') => {
                if let Some(Row::Account(i) | Row::Folder(i, _, _)) =
                    rows.get(self.sidebar_selected)
                {
                    if crate::oauth::built_in(&self.accounts[*i].account) {
                        self.sign_in(*i);
                    } else {
                        self.prompt = Some(Prompt::new(PromptKind::Password(*i), ""));
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => {
                if let Some(Row::Folder(i, _, _)) = rows.get(self.sidebar_selected) {
                    let i = *i;
                    self.accounts[i].expanded = false;
                    self.sidebar_selected = self
                        .sidebar_rows()
                        .iter()
                        .position(|r| *r == Row::Account(i))
                        .unwrap_or(0);
                }
            }
            _ => {}
        }
    }

    /// The sidebar's rows: every inbox together (with two accounts or
    /// more), then each account and its folders, then "Add an account".
    pub fn sidebar_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if self.accounts.len() > 1 {
            rows.push(Row::AllInboxes);
        }
        for (i, account) in self.accounts.iter().enumerate() {
            rows.push(Row::Account(i));
            if account.expanded {
                for (f, depth) in folder_rows(&account.folders) {
                    rows.push(Row::Folder(i, f, depth));
                }
            }
        }
        rows.push(Row::Add);
        rows
    }

    /// Puts the sidebar's cursor on the place shown.
    fn sync_sidebar(&mut self) {
        let rows = self.sidebar_rows();
        let at = rows.iter().position(|row| match (row, &self.place) {
            (Row::AllInboxes, Place::AllInboxes) => true,
            (Row::Folder(i, f, _), Place::Folder { account, folder }) => {
                i == account && self.accounts[*i].folders[*f].name == *folder
            }
            _ => false,
        });
        if let Some(at) = at {
            self.sidebar_selected = at;
        }
    }

    fn on_prompt_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                if let PromptKind::Password(i) = prompt.kind
                    && let Some(account) = self.accounts.get_mut(i)
                {
                    account.password_put_off = true;
                }
                self.prompt = None;
            }
            KeyCode::Enter => {
                let Some(prompt) = self.prompt.take() else {
                    return;
                };
                let text = prompt.text();
                match prompt.kind {
                    PromptKind::Search => self.run_search(text.trim()),
                    PromptKind::Password(i) => {
                        self.use_password(i, text);
                        // The next account waiting for its password asks.
                        let next = (0..self.accounts.len()).find(|&j| {
                            j != i
                                && matches!(self.accounts[j].state, State::NeedPassword { .. })
                                && !self.accounts[j].password_put_off
                        });
                        if let Some(j) = next {
                            self.prompt = Some(Prompt::new(PromptKind::Password(j), ""));
                        }
                    }
                    PromptKind::Attach => self.attach(&text),
                    PromptKind::Tag => {
                        let groups = std::mem::take(&mut self.tagging);
                        match tags::normalize(&text) {
                            Some(tag) => self.tag(groups, &tag, true),
                            None if text.trim().is_empty() => {}
                            None => self.error(format!(
                                "A tag is a word of letters, digits, - _ or . (at most {} characters)",
                                tags::MAX_TAG
                            )),
                        }
                    }
                }
            }
            KeyCode::Char('u') if ctrl => {
                prompt.input = Prompt::new(prompt.kind, "").input;
            }
            KeyCode::Char(c)
                if !ctrl && (text::is_hidden(c) || prompt.text().chars().count() >= MAX_PROMPT) => {
            }
            KeyCode::Char('m' | 'j') if ctrl => {}
            _ => {
                prompt.input.input(key);
            }
        }
    }

    fn use_password(&mut self, i: usize, password: String) {
        if password.is_empty() || i >= self.accounts.len() {
            return;
        }
        let Some(worker) = &self.accounts[i].worker else {
            return;
        };
        let creds = worker.creds.clone();
        futures::executor::block_on(creds.set(Secret::new(password)));
        self.accounts[i].password_put_off = false;
        self.accounts[i].state = State::Connecting;
        self.check(i, true);
    }

    fn run_search(&mut self, query: &str) {
        if query.is_empty() {
            self.search = None;
            self.reload();
            return;
        }
        self.search = Some(query.to_string());
        self.selected = 0;
        self.list_scroll.set(0);
        self.reload();
        self.focus = Focus::List;
        if let Place::Folder { account, folder } = self.place.clone()
            && !self.accounts[account].is_pop()
            && !self.demo
        {
            self.send(
                account,
                Request::Search {
                    folder,
                    query: query.to_string(),
                },
            );
        }
    }

    fn on_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let last = picker.items.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.picker = None,
            KeyCode::Char('j') | KeyCode::Down => picker.selected = (picker.selected + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => picker.selected = 0,
            KeyCode::Char('G') | KeyCode::End => picker.selected = last,
            KeyCode::Enter | KeyCode::Char('s') | KeyCode::Char('y') => {
                let Some(picker) = self.picker.take() else {
                    return;
                };
                let n = picker.selected;
                match (picker.kind, key.code) {
                    (
                        PickerKind::MoveTo {
                            account,
                            folder,
                            uids,
                            uidvalidity,
                            targets,
                        },
                        KeyCode::Enter,
                    ) => {
                        if !self.still_means(account, &folder, uidvalidity) {
                            return;
                        }
                        if let Some(to) = targets.get(n) {
                            self.move_messages(account, &folder, &uids, to);
                            let name = self.folder_name(account, to);
                            self.info(format!("{} moved to {name}", plural(uids.len(), "message")));
                        }
                    }
                    (PickerKind::Tag { groups, tags }, KeyCode::Enter) => match tags.get(n) {
                        Some(tag) => {
                            let keys: Vec<_> = groups
                                .iter()
                                .flat_map(|g| {
                                    g.uids.iter().map(|u| (g.account, g.folder.clone(), *u))
                                })
                                .collect();
                            let all = self
                                .tags_of(&keys)
                                .iter()
                                .any(|(t, n)| t == tag && *n == keys.len());
                            self.tag(groups, tag, !all);
                        }
                        None => {
                            self.tagging = groups;
                            self.prompt = Some(Prompt::new(PromptKind::Tag, ""));
                        }
                    },
                    (PickerKind::Changes(ids), KeyCode::Enter) => {
                        match ids
                            .get(n)
                            .and_then(|id| self.store.change(*id).ok().flatten())
                        {
                            Some(change) => {
                                let _ = self.undo(change);
                            }
                            None => self.error("That change is no longer kept"),
                        }
                    }
                    (PickerKind::Links, code) => {
                        let link = self
                            .reader
                            .as_ref()
                            .and_then(|r| r.body.as_ref())
                            .and_then(|b| b.links.get(n).cloned());
                        if let Some(link) = link {
                            if code == KeyCode::Char('y') {
                                self.copy(&link);
                            } else {
                                self.open_link(&link);
                            }
                        }
                    }
                    (PickerKind::Attachments, code) => {
                        let attachment = self
                            .reader
                            .as_ref()
                            .and_then(|r| r.parsed.as_ref())
                            .and_then(|p| p.attachments.get(n).cloned());
                        if let Some(a) = attachment {
                            if code == KeyCode::Char('s') {
                                self.extract(a.index, &a.name, false, false);
                            } else if code == KeyCode::Enter {
                                if tools::safe_to_open(&a.name) {
                                    self.extract(a.index, &a.name, true, false);
                                } else {
                                    self.confirm(Confirm::OpenAttachment {
                                        index: a.index,
                                        name: a.name,
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Copies `text` to the clipboard, or through the terminal (OSC 52)
    /// where there's no clipboard to reach (over SSH).
    fn copy(&mut self, text: &str) {
        if self.copy_quietly(text) {
            self.info("Copied");
        } else {
            self.error("Couldn't copy it");
        }
    }

    /// Copies `text`; whether it was.
    fn copy_quietly(&mut self, text: &str) -> bool {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let copied = self
            .clipboard
            .as_mut()
            .is_some_and(|c| c.set_text(text.to_string()).is_ok());
        copied || copy_through_terminal(text).is_ok()
    }

    fn confirm(&mut self, confirm: Confirm) {
        self.confirm = Some(confirm);
        self.confirm_at = Instant::now();
    }

    /// `y` answers yes; for an unsaved message, `n` discards it; any other
    /// key cancels.
    fn on_confirm_key(&mut self, key: KeyEvent) {
        let grace = match self.confirm {
            Some(Confirm::Agent { .. }) => AGENT_GRACE,
            _ => CONFIRM_GRACE,
        };
        let early = self.confirm_at.elapsed() < grace;
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        let yes = key.code == KeyCode::Char('y') && !early;
        let no = key.code == KeyCode::Char('n') && !early;
        let deleting = matches!(
            confirm,
            Confirm::Delete { .. } | Confirm::DeleteForever { .. }
        );
        match confirm {
            // Any key but `y` says no, once there's been time to read it:
            // it comes up on its own, maybe while keys are being pressed.
            Confirm::Agent { id } if !early => self.agent_answer(id, yes),
            Confirm::DeleteForever {
                account,
                folder,
                uids,
                uidvalidity,
            } if yes => {
                if self.still_means(account, &folder, uidvalidity) {
                    self.delete_forever(account, &folder, &uids)
                }
            }
            Confirm::Delete {
                account,
                ref folder,
                uidvalidity,
                ..
            } if yes && !self.still_means(account, folder, uidvalidity) => {}
            Confirm::Delete {
                account,
                folder,
                uids,
                ..
            } if yes => {
                if let Some(trash) = self.accounts[account]
                    .folder_for(Role::Trash)
                    .map(|f| f.name.clone())
                {
                    self.move_messages(account, &folder, &uids, &trash);
                }
            }
            Confirm::Send { print, .. } if yes => self.send_message(Some(print)),
            Confirm::CloseComposer if yes => self.save_draft(),
            Confirm::CloseComposer if no => {
                self.composer = None;
                self.info("Discarded");
            }
            Confirm::OpenAttachment { index, name } if yes => {
                self.extract(index, &name, true, true)
            }
            Confirm::OpenLink { url } if yes => self.open_link(&url),
            Confirm::RemoveAccount(i) if yes => self.remove_account(i),
            Confirm::Quit { .. } if yes => self.quit = true,
            confirm if early => self.confirm = Some(confirm),
            _ => {}
        }
        // Answered: `d`'s question about another folder's messages.
        if deleting {
            self.ask_next_delete();
        }
    }

    fn remove_account(&mut self, i: usize) {
        if i >= self.accounts.len() {
            return;
        }
        let state = self.accounts.remove(i);
        let id = state.id.clone();
        drop(state);
        let _ = self.store.forget_account(&id);
        if let Some(path) = &self.accounts_path
            && let Err(e) = account::remove(path, &id)
        {
            self.error(format!("{e:#}"));
        }
        if !self.demo {
            let id = id.clone();
            tokio::spawn(async move {
                let _ = crate::secrets::keychain_delete(&id).await;
                let _ = crate::secrets::keychain_delete(&crate::secrets::token_key(&id)).await;
            });
        }
        // Indexes past `i` moved down by one: nothing may keep one.
        self.reader = None;
        for read in std::mem::take(&mut self.agent_reads) {
            let _ = read.reply.send(Err("The account was removed".into()));
        }
        self.agent_downloads.clear();
        self.marked.clear();
        self.pending = None;
        self.local_flags.clear();
        self.local_removed.clear();
        self.picker = None;
        self.prompt = None;
        match self.composer.as_mut() {
            Some(c) if c.account == i => self.composer = None,
            Some(c) if c.account > i => c.account -= 1,
            _ => {}
        }
        self.place = match self.accounts.first() {
            Some(first) => Place::Folder {
                account: 0,
                folder: first.inbox(),
            },
            None => Place::AllInboxes,
        };
        self.sidebar_selected = 0;
        self.reload();
        self.info(format!("Removed {id}"));
        if self.accounts.is_empty() {
            self.setup = Some(Setup::default());
        }
    }

    fn on_help_key(&mut self, key: KeyEvent, ctrl: bool) {
        let Some(menu) = self.help.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('h' | 'l')
            | KeyCode::Left
            | KeyCode::Right => {
                menu.tab = match menu.tab {
                    HelpTab::Keys => HelpTab::Settings,
                    HelpTab::Settings => HelpTab::Keys,
                };
                return;
            }
            KeyCode::Enter | KeyCode::Char(' ') if menu.tab == HelpTab::Settings => {
                let row = menu.selected;
                return self.change_setting(row);
            }
            KeyCode::Esc | KeyCode::Char('q' | '?' | 'O') => {
                self.help = None;
                return;
            }
            _ => {}
        }
        let delta: isize = match key.code {
            KeyCode::Char('j') | KeyCode::Down => 1,
            KeyCode::Char('k') | KeyCode::Up => -1,
            KeyCode::Char('d') if ctrl => HELP_PAGE,
            KeyCode::Char('u') if ctrl => -HELP_PAGE,
            KeyCode::PageDown => 2 * HELP_PAGE,
            KeyCode::PageUp => -2 * HELP_PAGE,
            KeyCode::Char('g') | KeyCode::Home => isize::MIN,
            KeyCode::Char('G') | KeyCode::End => isize::MAX,
            _ => return,
        };
        match menu.tab {
            HelpTab::Keys => menu.scroll = menu.scroll.saturating_add_signed(delta),
            HelpTab::Settings => {
                menu.selected = menu
                    .selected
                    .saturating_add_signed(delta)
                    .min(HelpMenu::LAST);
            }
        }
    }

    /// Enter on the settings tab: turns the setting on `row` on or off, or
    /// picks the theme on it. Saved at once.
    fn change_setting(&mut self, row: usize) {
        let flip = |b: bool| toml::Value::Boolean(!b);
        match row {
            HelpMenu::PREVIEW => self.save_setting("preview", flip(self.settings.preview)),
            HelpMenu::PREFER_PLAIN => {
                self.save_setting("prefer_plain", flip(self.settings.prefer_plain));
                if let Some(reader) = self.reader.as_mut() {
                    reader.plain = None;
                }
                self.render();
            }
            HelpMenu::MARK_READ => self.save_setting("mark_read", flip(self.settings.mark_read)),
            HelpMenu::THREADS => {
                self.save_setting("threads", flip(self.settings.threads));
                self.reload();
            }
            HelpMenu::CONFIRM_DELETE => {
                self.save_setting("confirm_delete", flip(self.settings.confirm_delete))
            }
            HelpMenu::CONFIRM_SEND => {
                self.save_setting("confirm_send", flip(self.settings.confirm_send))
            }
            HelpMenu::AGENTS => {
                self.save_setting("agents", flip(self.settings.agents));
                self.listen_for_agents();
            }
            HelpMenu::CONFIRM_AGENTS => {
                self.save_setting("confirm_agents", flip(self.settings.confirm_agents))
            }
            HelpMenu::NOTIFICATIONS => {
                let on = self.settings.notifications != Notifications::Off;
                let value = if on { "off" } else { "auto" };
                self.save_setting("notifications", toml::Value::String(value.into()));
                self.notifications = self
                    .settings
                    .notifications
                    .resolve(|name| std::env::var(name).ok());
            }
            row => {
                if let Some((id, _)) = theme::names().get(row - HelpMenu::THEMES) {
                    self.save_setting("theme", toml::Value::String(id.to_string()));
                    self.colors = Colors::named(id);
                }
            }
        }
    }

    fn save_setting(&mut self, key: &str, value: toml::Value) {
        let Some(path) = self.settings_path.clone() else {
            // The demo: changed for this run only.
            let mut table = toml::Table::try_from(&self.settings).unwrap_or_default();
            table.insert(key.to_string(), value);
            if let Ok(settings) = table.try_into() {
                self.settings = settings;
            }
            return;
        };
        match Settings::save_one(&path, key, value) {
            Ok(settings) => {
                self.settings = settings;
                // The file may say agents are off now (another tuimail
                // turned them off): then they're off here too, not only
                // on the settings tab.
                if !self.settings.agents {
                    self.listen_for_agents();
                }
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    // Adding an account.

    fn on_setup_key(&mut self, key: KeyEvent) {
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        match setup.on_key(key) {
            setup::Action::None => {}
            setup::Action::Cancel => {
                self.setup = None;
                if self.accounts.is_empty() && !self.demo {
                    self.info("No account yet: press + to add one, q to quit");
                }
            }
            setup::Action::Check => self.check_setup(false),
            setup::Action::SaveAnyway => self.check_setup(true),
        }
    }

    fn check_setup(&mut self, skip: bool) {
        let keychain = self.keychain;
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        let account = match setup.account(keychain) {
            Ok(account) => account,
            Err(e) => {
                setup.error = Some(e);
                return;
            }
        };
        if self.accounts.iter().any(|a| a.id == account.id()) {
            setup.error = Some("That account is already here".into());
            return;
        }
        if skip || self.demo {
            let _ = self.tx.send(AppEvent::Checked { result: Ok(None) });
            setup.checking = true;
            return;
        }
        setup.checking = true;
        setup.error = None;
        let password = setup.password();
        let tx = self.tx.clone();
        // Signing in: the browser first, then the login with its token.
        let client = crate::oauth::built_in(&account)
            .then(|| crate::oauth::client_for(&account))
            .flatten();
        tokio::spawn(async move {
            let result = async {
                let Some(client) = client else {
                    check_login(&account, &password).await?;
                    return Ok(None);
                };
                let tokens = sign_in(client, &account.email, &tx).await?;
                check_login(&account, tokens.access.expose()).await?;
                Ok(Some(tokens))
            }
            .await;
            let _ = tx.send(AppEvent::Checked {
                result: result.map_err(|e: anyhow::Error| text::one_line(&format!("{e:#}"), 300)),
            });
        });
    }

    fn on_checked(&mut self, result: Result<Option<crate::oauth::Tokens>, String>) {
        let keychain = self.keychain;
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        setup.checking = false;
        self.signing_in = false;
        let tokens = match result {
            Ok(tokens) => tokens,
            Err(e) => {
                setup.error = Some(format!("{e} · Ctrl-f saves it anyway"));
                return;
            }
        };
        let Ok(account) = setup.account(keychain) else {
            return;
        };
        let password = setup.password();
        if let Some(path) = &self.accounts_path
            && let Err(e) = account::save(path, &account)
        {
            setup.error = Some(format!("{e:#}"));
            return;
        }
        // The login worked: what it took is kept (the refresh token, when
        // signing in, in its own entry). Nothing for an account that signs
        // in here and hasn't (saved without checking): no password is its.
        let kept = match &tokens {
            Some(tokens) => {
                (tokens.refresh.clone()).map(|t| (crate::secrets::token_key(&account.id()), t))
            }
            None if crate::oauth::built_in(&account) || password.is_empty() => None,
            None => Some((account.id(), Secret::new(password.clone()))),
        };
        if account.password == PasswordFrom::Keychain
            && !self.demo
            && let Some((key, secret)) = kept
        {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::secrets::keychain_set(&key, &secret).await {
                    // The form is closed by now: said on the status line.
                    let _ = tx.send(AppEvent::Failed {
                        error: text::one_line(&format!("{e:#}"), 300),
                    });
                }
            });
        }
        self.setup = None;
        let name = account.shown_name();
        let password = tokens.is_none().then(|| Secret::new(password));
        let i = self.add_account(account, password);
        if let (Some(tokens), Some(worker)) = (tokens, &self.accounts[i].worker) {
            worker.creds.set_tokens(tokens);
        }
        let inbox = self.accounts[i].inbox();
        self.open_place(Place::Folder {
            account: i,
            folder: inbox,
        });
        self.check(i, true);
        self.sync_sidebar();
        self.focus = Focus::List;
        self.info(format!("Added {name}"));
    }
}

/// Signs in with the browser: the link goes to the app to open, and the
/// tokens come back once the browser does.
async fn sign_in(
    client: crate::oauth::Client,
    email: &str,
    tx: &UnboundedSender<AppEvent>,
) -> Result<crate::oauth::Tokens> {
    let sign_in = crate::oauth::SignIn::start(client, email).await?;
    let _ = tx.send(AppEvent::SignInOpened {
        url: sign_in.url.clone(),
    });
    sign_in.finish().await
}

/// Logs in to an account's servers, receiving and sending, to check them.
async fn check_login(account: &Account, password: &str) -> Result<()> {
    let tls = net::tls_config(account.certificate.as_deref())?;
    let endpoints = Endpoints::of(account, tls);
    match account.protocol {
        Protocol::Imap => {
            let (session, _) = crate::imap::connect(
                &endpoints.incoming,
                account.username(),
                password,
                account.auth,
            )
            .await?;
            crate::imap::logout(session).await;
        }
        Protocol::Pop3 => {
            let pop = crate::pop3::connect(
                &endpoints.incoming,
                account.username(),
                password,
                account.auth,
            )
            .await?;
            pop.quit().await?;
        }
    }
    crate::smtp::check(
        &endpoints.smtp,
        account.smtp_username(),
        password,
        account.auth,
    )
    .await
}

/// The folders a POP3 account has here: the downloaded inbox, and the
/// folders tuimail keeps for it.
pub fn local_folders() -> Vec<Folder> {
    [
        ("INBOX", "Inbox", Role::Inbox),
        ("Drafts", "Drafts", Role::Drafts),
        ("Sent", "Sent", Role::Sent),
        ("Archive", "Archive", Role::Archive),
        ("Trash", "Trash", Role::Trash),
    ]
    .into_iter()
    .map(|(name, shown, role)| Folder {
        name: name.into(),
        shown: shown.into(),
        delimiter: None,
        role: Some(role),
        selectable: true,
        unread: 0,
        total: 0,
    })
    .collect()
}

/// A folder's name in the sidebar: Inbox for the inbox, else the last
/// part of its name.
pub fn folder_label(folder: &Folder) -> String {
    if folder.role == Some(Role::Inbox) {
        return "Inbox".into();
    }
    match &folder.delimiter {
        Some(d) if !d.is_empty() => folder
            .shown
            .rsplit(d.as_str())
            .next()
            .unwrap_or(&folder.shown)
            .to_string(),
        _ => folder.shown.clone(),
    }
}

/// The sidebar's folders: those with a role first (flat), then the rest by
/// name, each with how deep it's nested. A folder that only holds folders
/// with roles (Gmail's "[Gmail]") is left out.
pub fn folder_rows(folders: &[Folder]) -> Vec<(usize, usize)> {
    let mut rows: Vec<(usize, usize)> = folders
        .iter()
        .enumerate()
        .filter(|(_, f)| f.role.is_some())
        .map(|(i, _)| (i, 0))
        .collect();
    let mut rest: Vec<(usize, &Folder)> = folders
        .iter()
        .enumerate()
        .filter(|(_, f)| f.role.is_none())
        .collect();
    rest.sort_by_key(|(_, f)| f.shown.to_lowercase());
    for (i, folder) in rest {
        let delimiter = folder.delimiter.as_deref().filter(|d| !d.is_empty());
        let children: Vec<&Folder> = match delimiter {
            Some(d) => folders
                .iter()
                .filter(|f| f.name.starts_with(&format!("{}{d}", folder.name)))
                .collect(),
            None => Vec::new(),
        };
        if !folder.selectable && children.iter().all(|c| c.role.is_some()) {
            continue;
        }
        let depth = delimiter
            .map_or(0, |d| folder.name.matches(d).count())
            .min(6);
        rows.push((i, depth));
    }
    rows
}

pub fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// A link's host, for saying where it goes: where a browser takes it.
/// A `mailto:` link as a message to write: its addresses and subject.
/// Its body and any other header are left out: the sender wrote them.
pub fn mailto_draft(link: &str) -> Option<Draft> {
    let rest = link
        .get(..7)
        .filter(|s| s.eq_ignore_ascii_case("mailto:"))
        .map(|_| &link[7..])?;
    let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
    let subject = query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case("subject"))
        .map(|(_, v)| text::one_line(&percent_decoded(v), mail::MAX_SUBJECT))
        .unwrap_or_default();
    Some(Draft {
        to: text::one_line(&percent_decoded(to), 1000),
        subject,
        ..Draft::default()
    })
}

/// `%XX` escapes decoded (RFC 3986); bytes that aren't UTF-8 become �.
fn percent_decoded(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn link_host(url: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    // As browsers read it (the URL Standard): tabs and line breaks dropped,
    // any `/` and `\` after the scheme skipped, the host ended at `\` as
    // at `/`.
    let url: String = url
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let rest = url.split_once(':').map_or(url.as_str(), |(_, r)| r);
    let rest = rest.trim_start_matches(['/', '\\']);
    let host = rest.split(['/', '\\', '?', '#']).next().unwrap_or(rest);
    // Then `%XX` decoded and letters mapped as IDNA maps them (NFKC, lower
    // case): `%2E`, `․` and `﹒` are all a dot to the browser, so here.
    let host = percent_decoded(host);
    let host = host.rsplit('@').next().unwrap_or(&host);
    let host = text::one_line(host, host.len());
    let host = host.nfkc().collect::<String>().to_lowercase();
    // Letters from other alphabets are shown as the browser sends them
    // (punycode, `xn--`): `аpple.com` with a Cyrillic а isn't apple.com.
    let host = if host.is_ascii() {
        host
    } else {
        host.split(['.', '。', '．', '｡'])
            .map(ascii_label)
            .collect::<Vec<_>>()
            .join(".")
    };
    // A long host keeps its end: the site it belongs to is there.
    let count = host.chars().count();
    if count <= 80 {
        host
    } else {
        format!("…{}", host.chars().skip(count - 79).collect::<String>())
    }
}

/// A host's label as DNS has it: punycode for one with letters outside
/// ASCII (RFC 3492). Longer than DNS allows, its other letters are `?`.
fn ascii_label(label: &str) -> String {
    const BASE: u32 = 36;
    const T_MIN: u32 = 1;
    const T_MAX: u32 = 26;
    fn digit(d: u32) -> char {
        char::from(if d < 26 {
            b'a' + d as u8
        } else {
            b'0' + (d - 26) as u8
        })
    }
    fn adapt(delta: u32, points: u32, first: bool) -> u32 {
        let mut delta = delta / if first { 700 } else { 2 };
        delta += delta / points;
        let mut k = 0;
        while delta > ((BASE - T_MIN) * T_MAX) / 2 {
            delta /= BASE - T_MIN;
            k += BASE;
        }
        k + (BASE - T_MIN + 1) * delta / (delta + 38)
    }
    if label.is_ascii() {
        return label.to_string();
    }
    let points: Vec<u32> = label.chars().map(u32::from).collect();
    let unreadable = || {
        label
            .chars()
            .map(|c| if c.is_ascii() { c } else { '?' })
            .collect()
    };
    if points.len() > 63 {
        return unreadable();
    }
    let mut out: String = label.chars().filter(char::is_ascii).collect();
    let basic = out.len() as u32;
    if basic > 0 {
        out.push('-');
    }
    let (mut n, mut delta, mut bias, mut done) = (128u32, 0u32, 72u32, basic);
    while (done as usize) < points.len() {
        let Some(m) = points.iter().copied().filter(|&c| c >= n).min() else {
            return unreadable();
        };
        delta += (m - n) * (done + 1);
        n = m;
        for &c in &points {
            if c < n {
                delta += 1;
            } else if c == n {
                let (mut q, mut k) = (delta, BASE);
                loop {
                    let t = k.saturating_sub(bias).clamp(T_MIN, T_MAX);
                    if q < t {
                        break;
                    }
                    out.push(digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                out.push(digit(q));
                bias = adapt(delta, done + 1, done == basic);
                delta = 0;
                done += 1;
            }
        }
        delta += 1;
        n += 1;
    }
    format!("xn--{out}")
}

/// Asks the terminal to put `text` on the clipboard (OSC 52).
fn copy_through_terminal(text: &str) -> std::io::Result<()> {
    use base64::Engine;
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()
}

/// A path as typed or dropped on the terminal: quotes around it, `\ ` for
/// spaces, `~` for home.
pub fn unquote(path: &str) -> String {
    let path = path.trim();
    let path = path
        .strip_prefix('\'')
        .and_then(|p| p.strip_suffix('\''))
        .or_else(|| path.strip_prefix('"').and_then(|p| p.strip_suffix('"')))
        .map_or_else(|| path.replace("\\ ", " "), str::to_string);
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|home| home.join(rest).to_string_lossy().into_owned())
            .unwrap_or(path),
        None => path,
    }
}

/// Turns SIGHUP (the terminal window closed) and SIGTERM into a normal quit.
#[cfg(unix)]
fn quit_on_hangup(tx: UnboundedSender<AppEvent>) -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut hangup = signal(SignalKind::hangup())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::spawn(async move {
        tokio::select! {
            _ = hangup.recv() => {}
            _ = terminate.recv() => {}
        }
        let _ = tx.send(AppEvent::Quit);
    });
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The demo's two accounts and mail, in memory, with no workers.
    pub fn app() -> App {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let accounts = crate::demo::accounts();
        let mut app = App::new(
            Store::in_memory(),
            Settings::default(),
            accounts[..1].to_vec(),
            false,
            None,
            tx,
            true,
        );
        app.icons = crate::icons::PLAIN;
        app.status = None;
        app.test_rx = Some(rx);
        app
    }

    impl App {
        /// Handles the events the app sent itself (layouts, drafts, sends).
        pub fn drain_for_tests(&mut self) {
            let mut events = Vec::new();
            if let Some(rx) = self.test_rx.as_mut() {
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
            }
            for event in events {
                self.on_event(event);
            }
        }

        /// Waits until `done` holds, handling the app's events meanwhile.
        pub async fn wait_until(&mut self, done: impl Fn(&App) -> bool) {
            for _ in 0..500 {
                self.drain_for_tests();
                if done(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("it didn't happen");
        }
    }

    /// Fills `app` with the demo's mail.
    #[tokio::test]
    async fn the_link_list_says_what_each_link_says_beside_its_real_site() {
        let mut app = app();
        with_mail(&mut app);
        let links = [
            "https://r.example.com/a?id=1",
            "https://r.example.com/b?id=2",
            "https://evil.example/x",
        ];
        app.picker = Some(Picker {
            kind: PickerKind::Links,
            title: " Open a link ".into(),
            items: links.iter().map(|l| l.to_string()).collect(),
            labels: [
                ("r.example.com", "Aerial view"),
                ("r.example.com", "Try it"),
                ("evil.example", ""),
            ]
            .iter()
            .map(|(h, s)| (h.to_string(), s.to_string()))
            .collect(),
            selected: 0,
        });
        let shown = crate::ui::tests::screen(&mut app, 160, 30);
        let row = |said: &str| {
            shown
                .lines()
                .find(|l| l.contains(said))
                .unwrap_or_default()
                .to_string()
        };
        let aerial = row("Aerial view");
        assert!(
            aerial.contains("r.example.com") && aerial.contains("id=1"),
            "{shown}"
        );
        assert!(row("Try it").contains("id=2"), "{shown}");
        assert!(row("evil.example/x").contains("evil.example  "), "{shown}");
    }

    #[test]
    fn a_link_site_comes_from_the_whole_url() {
        // What's before the @ is a user name, however long.
        let url = format!("https://bank.example{}@evil.example/x", "-".repeat(400));
        assert_eq!(link_host(&url), "evil.example");
        // Where browsers end it: a `\` before the @.
        assert_eq!(
            link_host("https://evil.example\\@bank.example/x"),
            "evil.example"
        );
        assert_eq!(
            link_host("https://\\evil.example\\@bank.example/x"),
            "evil.example"
        );
        assert_eq!(
            link_host("mailto:someone@example.org?subject=x"),
            "example.org"
        );
        // A long host keeps the site it belongs to.
        let long = format!("https://bank.example.{}.evil.example/x", "a".repeat(120));
        let host = link_host(&long);
        assert!(
            host.ends_with(".evil.example") && host.starts_with('…'),
            "{host}"
        );
        assert_eq!(host.chars().count(), 80);
    }

    #[test]
    fn a_link_site_is_the_one_the_browser_opens() {
        // Decoded, as the browser decodes it.
        assert_eq!(
            link_host("https://paypal.com%2Eevil.com/login"),
            "paypal.com.evil.com"
        );
        // Characters IDNA maps to a dot are one.
        for dot in ['\u{2024}', '\u{fe52}', '\u{ff0e}'] {
            let url = format!("https://paypal.com{dot}evil.com/login");
            assert_eq!(link_host(&url), "paypal.com.evil.com", "{dot:?}");
        }
        // Fullwidth letters are their ASCII ones; case doesn't matter.
        assert_eq!(link_host("https://\u{ff30}ay\u{ff30}al.COM/"), "paypal.com");
        // Line breaks inside are dropped, not shown as spaces.
        assert_eq!(link_host("https://pay\npal.com/"), "paypal.com");
    }

    #[test]
    fn a_site_with_letters_from_other_alphabets_shows_as_its_browser_sends_it() {
        // A Cyrillic а in place of a.
        assert_eq!(
            link_host("https://\u{430}pple.com/login"),
            "xn--pple-43d.com"
        );
        assert_eq!(link_host("https://M\u{fc}nchen.DE/"), "xn--mnchen-3ya.de");
        assert_eq!(
            link_host("https://\u{3b1}\u{3b2}\u{3b3}.gr"),
            "xn--mxacd.gr"
        );
        assert_eq!(link_host("https://\u{fc}.example"), "xn--tda.example");
        // Dots of other alphabets end a label as `.` does.
        assert_eq!(
            link_host("https://b\u{fc}cher\u{3002}de"),
            "xn--bcher-kva.de"
        );
        let long = format!("https://{}\u{430}.com", "a".repeat(80));
        let host = link_host(&long);
        assert!(host.ends_with("a?.com"), "{host}");
        assert!(host.trim_start_matches('…').is_ascii(), "{host}");
    }

    #[test]
    fn a_mailto_link_gives_only_its_addresses_and_subject() {
        let draft = mailto_draft(
            "MAILTO:list%40example.org?Subject=Hi%20there%0D%0ABcc:%20x@evil.example\
             &body=Send%20your%20password&bcc=spy@evil.example&cc=more@evil.example",
        )
        .unwrap();
        assert_eq!(draft.to, "list@example.org");
        assert_eq!(draft.subject, "Hi there Bcc: x@evil.example");
        assert_eq!(draft.body, "");
        assert_eq!(draft.cc, "");
        assert_eq!(draft.bcc, "");
        assert!(mailto_draft("https://example.org").is_none());
        // A stray `%` or one cut short stays as it is.
        assert_eq!(mailto_draft("mailto:a%b@c%4").unwrap().to, "a%b@c%4");
    }

    #[tokio::test]
    async fn unsubscribing_on_a_web_page_shows_where_it_goes_first() {
        let mut app = app();
        with_mail(&mut app);
        open_first(&mut app).await;
        let reader = app.reader.as_mut().unwrap();
        let mut parsed = (**reader.parsed.as_ref().unwrap()).clone();
        parsed.unsubscribe = Some("https://lists.example.org/u?id=42".into());
        reader.parsed = Some(Arc::new(parsed));
        app.on_key(key(KeyCode::Char('U')));
        assert!(matches!(app.confirm, Some(Confirm::OpenLink { .. })));
        let screen = crate::ui::tests::screen(&mut app, 120, 40);
        assert!(screen.contains("lists.example.org/u?id=42"), "{screen}");
        // Anything but `y` opens nothing.
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('n')));
        assert!(app.confirm.is_none());
        assert!(app.status.is_none());
    }

    #[tokio::test]
    async fn a_mailto_link_is_written_in_tuimail() {
        let mut app = app();
        with_mail(&mut app);
        open_first(&mut app).await;
        let account = app.reader.as_ref().unwrap().account;
        app.open_link("mailto:help@example.org?subject=Question&body=x");
        let composer = app.composer.as_ref().expect("a message to write");
        assert_eq!(composer.account, account);
        let draft = composer.draft();
        assert_eq!(draft.to, "help@example.org");
        assert_eq!(draft.subject, "Question");
        assert_eq!(draft.body, "");
    }

    #[tokio::test]
    async fn accounts_at_one_provider_are_told_apart() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut accounts = crate::demo::accounts();
        // As the setup form used to name them: both after the domain.
        for (a, email) in accounts
            .iter_mut()
            .zip(["sam@gmail.com", "sam.work@gmail.com"])
        {
            a.name = "gmail.com".into();
            a.email = email.into();
        }
        let mut app = App::new(
            Store::in_memory(),
            Settings::default(),
            accounts,
            false,
            None,
            tx,
            true,
        );
        app.icons = crate::icons::PLAIN;
        with_mail(&mut app);
        app.open_place(Place::AllInboxes);
        assert_eq!(app.account_name(0), "sam@gmail.com");
        assert_eq!(app.account_name(1), "sam.work@gmail.com");
        let shown = crate::ui::tests::screen(&mut app, 160, 30);
        assert!(shown.contains("sam@gmail.com"), "{shown}");
        assert!(shown.contains("sam.work@gmail.com"), "{shown}");
        assert!(!shown.contains("[gmail.com]"), "{shown}");

        // Unnamed, as the setup form makes them now: the address in the
        // sidebar, what's before the @ in All inboxes' tags.
        for a in &mut app.accounts {
            Arc::make_mut(&mut a.account).name.clear();
        }
        assert_eq!(app.account_name(1), "sam.work@gmail.com");
        assert_eq!(app.account_tag(0), "sam");
        assert_eq!(app.account_tag(1), "sam.work");
        let shown = crate::ui::tests::screen(&mut app, 160, 30);
        assert!(shown.contains("[sam]"), "{shown}");
        assert!(shown.contains("[sam.work]"), "{shown}");
    }

    pub fn with_mail(app: &mut App) {
        let accounts: Vec<Account> = app.accounts.iter().map(|a| (*a.account).clone()).collect();
        crate::demo::fill(&mut app.store, &accounts);
        for a in &mut app.accounts {
            a.folders = app.store.folders(&a.id).unwrap_or_default();
        }
        app.reload();
    }

    /// Opens the first message and waits for it to be laid out.
    pub async fn open_first(app: &mut App) {
        app.focus = Focus::List;
        app.selected = 0;
        app.open_selected(true);
        wait_for_layout(app).await;
    }

    pub async fn wait_for_layout(app: &mut App) {
        app.wait_until(|app| {
            app.reader
                .as_ref()
                .is_some_and(|r| r.pending.is_none() && (r.body.is_some() || r.error.is_some()))
        })
        .await;
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn subjects(app: &App) -> Vec<String> {
        app.items
            .iter()
            .map(|i| i.message.summary.subject.clone())
            .collect()
    }

    #[tokio::test]
    async fn the_inbox_opens_first_with_its_mail() {
        let mut app = app();
        with_mail(&mut app);
        assert_eq!(
            app.place,
            Place::Folder {
                account: 0,
                folder: "INBOX".into()
            }
        );
        assert_eq!(subjects(&app)[0], "Lunch tomorrow?");
        assert_eq!(app.items.len(), 20);
    }

    #[tokio::test]
    async fn reading_marks_read_and_keys_change_flags() {
        let mut app = app();
        with_mail(&mut app);
        let unread = |app: &App| app.accounts[0].folder_for(Role::Inbox).unwrap().unread;
        assert_eq!(unread(&app), 2);
        open_first(&mut app).await;
        assert!(app.items[0].message.flags.seen, "Enter marks it read");
        assert_eq!(unread(&app), 1);
        app.on_key(key(KeyCode::Char('u')));
        assert!(!app.items[0].message.flags.seen, "u: unread again");
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.items[0].message.flags.flagged);
        let stored = app
            .store
            .message(&app.accounts[0].id, "INBOX", app.items[0].message.uid)
            .unwrap()
            .unwrap();
        assert!(stored.flags.flagged && !stored.flags.seen, "kept");
    }

    #[tokio::test]
    async fn delete_moves_to_trash_and_from_trash_asks_first() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        let before = app.items.len();
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.items.len(), before - 1);
        assert!(!subjects(&app).contains(&"Lunch tomorrow?".to_string()));
        let trash = Place::Folder {
            account: 0,
            folder: "Trash".into(),
        };
        app.open_place(trash);
        assert_eq!(subjects(&app), ["Lunch tomorrow?"]);
        app.on_key(key(KeyCode::Char('d')));
        assert!(matches!(app.confirm, Some(Confirm::DeleteForever { .. })));
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.items.is_empty(), "gone for good");
    }

    #[tokio::test]
    async fn deleting_from_several_folders_asks_about_each_that_needs_it() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        let (a, b) = (app.items[0].clone(), app.items[1].clone());
        app.move_messages(0, "INBOX", &[a.message.uid], "Trash");
        app.move_messages(0, "INBOX", &[b.message.uid], "Website");
        let in_folder = |app: &mut App, folder: &str| {
            app.open_place(Place::Folder {
                account: 0,
                folder: folder.into(),
            });
            app.items.iter().map(Item::key).collect::<Vec<_>>()
        };
        let mut marked = in_folder(&mut app, "Website");
        marked.extend(in_folder(&mut app, "Trash"));
        assert_eq!(marked.len(), 2);
        app.marked = marked.into_iter().collect();
        app.focus = Focus::List;
        app.on_key(key(KeyCode::Char('d')));
        // Trash's asks; Website's, which comes after it, isn't left behind.
        assert!(matches!(
            &app.confirm,
            Some(Confirm::DeleteForever { folder, .. }) if folder == "Trash"
        ));
        assert!(in_folder(&mut app, "Website").is_empty());
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('y')));
        app.open_place(Place::Folder {
            account: 0,
            folder: "Trash".into(),
        });
        assert_eq!(
            subjects(&app),
            std::slice::from_ref(&b.message.summary.subject)
        );
    }

    #[tokio::test]
    async fn marking_read_counts_only_those_that_were_unread() {
        let mut app = app();
        with_mail(&mut app);
        let unread = |app: &App| app.accounts[0].folder_for(Role::Inbox).unwrap().unread;
        assert_eq!(unread(&app), 2);
        let one_unread = app
            .items
            .iter()
            .find(|it| !it.message.flags.seen)
            .unwrap()
            .key();
        let one_read = app
            .items
            .iter()
            .find(|it| it.message.flags.seen)
            .unwrap()
            .key();
        app.set_flag(
            &[one_unread.clone(), one_read.clone()],
            FlagKind::Seen,
            true,
        );
        assert_eq!(unread(&app), 1);
        app.set_flag(&[one_unread, one_read], FlagKind::Seen, false);
        assert_eq!(unread(&app), 3);
    }

    #[tokio::test]
    async fn a_download_that_failed_drops_the_reply_waiting_for_it() {
        let mut app = app();
        with_mail(&mut app);
        let key = app.items[0].key();
        app.pending = Some((key.clone(), Pending::Reply { all: false }));
        app.on_worker(
            0,
            worker::Event::Body {
                folder: key.1,
                uid: key.2,
                result: Err("Not downloaded: the account is offline".into()),
            },
        );
        assert!(app.pending.is_none(), "it won't open much later");
        let status = app.status.as_ref().unwrap();
        assert!(
            status.error && status.text.contains("offline"),
            "{}",
            status.text
        );
    }

    #[tokio::test]
    async fn a_password_prompt_put_off_isnt_opened_again_on_its_own() {
        let mut app = app();
        with_mail(&mut app);
        let need = || worker::Event::State(State::NeedPassword { refused: true });
        app.on_worker(0, need());
        assert!(matches!(
            app.prompt.as_ref().map(|p| p.kind),
            Some(PromptKind::Password(0))
        ));
        app.on_key(key(KeyCode::Esc));
        assert!(app.prompt.is_none());
        app.on_worker(0, need());
        assert!(app.prompt.is_none(), "put off: keys stay the list's");
    }

    #[tokio::test]
    async fn marked_messages_move_together() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        let before = app.items.len();
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(app.marked.len(), 2);
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(app.items.len(), before - 2);
        assert!(app.marked.is_empty());
        app.open_place(Place::Folder {
            account: 0,
            folder: "Archive".into(),
        });
        assert_eq!(app.items.len(), 2);
        app.on_key(key(KeyCode::Char('m')));
        let picker = app.picker.as_ref().expect("a folder picker");
        assert!(
            picker.items.contains(&"Website".to_string()),
            "{:?}",
            picker.items
        );
        assert!(
            !picker.items.contains(&"Archive".to_string()),
            "not where it is"
        );
    }

    #[tokio::test]
    async fn search_finds_by_sender_and_esc_ends_it() {
        let mut app = app();
        with_mail(&mut app);
        let before = app.items.len();
        app.on_key(key(KeyCode::Char('/')));
        for c in "bob q3".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.search.as_deref(), Some("bob q3"));
        assert_eq!(subjects(&app), ["Quarterly numbers"]);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.search, None);
        assert_eq!(app.items.len(), before);
    }

    #[tokio::test]
    async fn a_reply_is_written_and_sent_in_the_demo() {
        let mut app = app();
        with_mail(&mut app);
        open_first(&mut app).await;
        app.on_key(key(KeyCode::Char('r')));
        app.wait_until(|app| app.composer.is_some()).await;
        let composer = app.composer.as_ref().expect("the composer");
        assert_eq!(composer.kind, Kind::Reply);
        let draft = composer.draft();
        assert_eq!(draft.to, "Alice Rivera <alice@example.com>");
        assert_eq!(draft.subject, "Re: Lunch tomorrow?");
        assert!(
            draft.body.contains("> Are we still on for noon?"),
            "{}",
            draft.body
        );
        app.settings.confirm_send = false;
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        app.wait_until(|app| app.composer.is_none()).await;
        let sent = app.store.messages(&app.accounts[0].id, "Sent", 10).unwrap();
        assert_eq!(sent[0].summary.subject, "Re: Lunch tomorrow?");
        let original = app
            .store
            .message(&app.accounts[0].id, "INBOX", app.items[0].message.uid)
            .unwrap()
            .unwrap();
        assert!(original.flags.answered, "marked answered");
    }

    /// Opens a new message, writes `to` and `subject`, and presses Ctrl-s.
    fn write_and_send(app: &mut App, to: &str, subject: &str) {
        app.on_key(key(KeyCode::Char('c')));
        for c in to.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        for _ in 0..3 {
            app.on_key(key(KeyCode::Tab));
        }
        for c in subject.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    }

    async fn settle(app: &mut App) {
        for _ in 0..50 {
            app.drain_for_tests();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn sent_subjects(app: &App) -> Vec<String> {
        let sent = app.store.messages(&app.accounts[0].id, "Sent", 20).unwrap();
        sent.iter().map(|m| m.summary.subject.clone()).collect()
    }

    #[tokio::test]
    async fn a_reply_made_ready_late_does_not_take_over_a_send_question() {
        let mut app = app();
        with_mail(&mut app);
        open_first(&mut app).await;
        // The reply is laid out in the background; another message is
        // written and sent meanwhile.
        app.on_key(key(KeyCode::Char('r')));
        write_and_send(&mut app, "x@y.z", "Mine");
        assert!(matches!(app.confirm, Some(Confirm::Send { .. })));
        settle(&mut app).await;
        let composer = app.composer.as_ref().unwrap();
        assert_eq!(composer.kind, Kind::New);
        assert_eq!(composer.draft().subject, "Mine");
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('y')));
        app.wait_until(|app| app.composer.is_none()).await;
        let sent = sent_subjects(&app);
        assert!(sent.contains(&"Mine".to_string()), "{sent:?}");
        assert!(!sent.iter().any(|s| s.starts_with("Re:")), "{sent:?}");
    }

    #[tokio::test]
    async fn a_yes_sends_only_the_message_it_was_asked_about() {
        let mut app = app();
        with_mail(&mut app);
        let before = sent_subjects(&app);
        write_and_send(&mut app, "x@y.z", "Mine");
        let Some(Confirm::Send { recipients, .. }) = app.confirm.clone() else {
            panic!("asked");
        };
        assert_eq!(recipients, ["x@y.z"]);
        // A paste under the question changes nothing.
        app.on_terminal(Event::Paste(", other@example.net".into()));
        assert_eq!(app.composer.as_ref().unwrap().draft().to, "x@y.z");
        // Another message in its place: the yes doesn't send it, the
        // question comes back about it.
        let other = Draft {
            to: "z@y.x".into(),
            subject: "Other".into(),
            ..Draft::default()
        };
        app.composer = Some(Composer::new(0, Kind::New, other));
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('y')));
        settle(&mut app).await;
        assert_eq!(sent_subjects(&app), before);
        let Some(Confirm::Send {
            recipients,
            subject,
            ..
        }) = app.confirm.clone()
        else {
            panic!("asked again");
        };
        assert_eq!(
            (recipients, subject),
            (vec!["z@y.x".to_string()], "Other".to_string())
        );
    }

    /// An app with both demo accounts: work (IMAP) and home (POP3).
    fn two_accounts() -> App {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            Store::in_memory(),
            Settings::default(),
            crate::demo::accounts(),
            false,
            None,
            tx,
            true,
        );
        app.icons = crate::icons::PLAIN;
        app.test_rx = Some(rx);
        with_mail(&mut app);
        app
    }

    /// Opens work's saved draft in the composer.
    async fn edit_works_draft(app: &mut App) {
        app.open_place(Place::Folder {
            account: 0,
            folder: "Drafts".into(),
        });
        app.focus = Focus::List;
        app.selected = 0;
        app.open_selected(true);
        app.wait_until(|a| a.composer.is_some()).await;
    }

    fn drafts(app: &App, account: usize) -> Vec<String> {
        let id = &app.accounts[account].id;
        let list = app.store.messages(id, "Drafts", 20).unwrap();
        list.iter().map(|m| m.summary.subject.clone()).collect()
    }

    #[tokio::test]
    async fn a_draft_sent_from_another_account_leaves_that_accounts_mail_alone() {
        let mut app = two_accounts();
        let home = app.accounts[1].id.clone();
        let raw =
            b"From: Sam <sam@home.example>\r\nTo: x@y.z\r\nSubject: Home draft\r\n\r\nkeep\r\n";
        let flags = Flags {
            seen: true,
            draft: true,
            ..Flags::default()
        };
        assert_eq!(
            app.store
                .add_local(&home, "Drafts", raw, flags, None)
                .unwrap(),
            1
        );
        edit_works_draft(&mut app).await;
        let work_draft = drafts(&app, 0);
        // From switched to home, then sent.
        app.composer.as_mut().unwrap().field = composer::Field::From;
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.composer.as_ref().unwrap().account, 1);
        app.settings.confirm_send = false;
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        app.wait_until(|a| a.composer.is_none()).await;
        assert_eq!(drafts(&app, 1), ["Home draft"], "home's draft stays");
        assert!(
            drafts(&app, 0).len() < work_draft.len(),
            "work's draft goes"
        );
    }

    #[tokio::test]
    async fn a_folder_made_again_on_the_server_keeps_its_new_messages_safe() {
        let mut app = two_accounts();
        let work = app.accounts[0].id.clone();
        let known = |uidvalidity| crate::store::Known {
            uidvalidity,
            low: 1,
            high: 10,
        };
        app.store.set_known(&work, "Drafts", known(1)).unwrap();
        edit_works_draft(&mut app).await;
        let before = drafts(&app, 0);
        // The server makes Drafts again: UID 1 is another message now.
        app.store.set_known(&work, "Drafts", known(999)).unwrap();
        app.settings.confirm_send = false;
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        app.wait_until(|a| a.composer.is_none()).await;
        assert_eq!(
            drafts(&app, 0),
            before,
            "nothing deleted under a new UIDVALIDITY"
        );

        // A question asked before the change does nothing after it.
        app.store.set_known(&work, "Drafts", known(1)).unwrap();
        let uids = vec![app.store.messages(&work, "Drafts", 1).unwrap()[0].uid];
        app.confirm(Confirm::DeleteForever {
            account: 0,
            folder: "Drafts".into(),
            uids,
            uidvalidity: 1,
        });
        app.store.set_known(&work, "Drafts", known(999)).unwrap();
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(drafts(&app, 0), before);
    }

    #[tokio::test]
    async fn a_file_is_opened_only_as_the_one_it_was_listed_as() {
        let mut app = app();
        with_mail(&mut app);
        open_first(&mut app).await;
        let dir = std::env::temp_dir().join(format!("tuimail-open-{}", std::process::id()));
        app.data_dir = Some(dir.clone());
        app.demo = false;
        // The server sends the message again with other files in it.
        let other = b"From: a@b.c\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nx\r\n\
--b\r\nContent-Type: application/octet-stream; name=\"run.desktop\"\r\n\r\nx\r\n--b--\r\n";
        app.reader.as_mut().unwrap().raw = Some(Arc::new(other.to_vec()));
        app.extract(0, "photo.jpg", true, false);
        app.wait_until(|a| a.status.as_ref().is_some_and(|s| s.error))
            .await;
        let status = app.status.as_ref().unwrap().text.clone();
        assert!(status.contains("changed"), "{status}");
        assert!(!dir.join("opened").join("run.desktop").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quitting_with_changes_on_their_way_asks_first() {
        let mut app = app();
        with_mail(&mut app);
        app.accounts[0].waiting.set(2);
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.quit);
        assert_eq!(app.confirm, Some(Confirm::Quit { waiting: 2 }));
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('n')));
        assert!(!app.quit, "stays");
        app.accounts[0].waiting.set(0);
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit, "nothing waiting: quits at once");
    }

    #[tokio::test]
    async fn a_delete_for_good_is_kept_until_the_server_has_made_it() {
        let mut app = app();
        with_mail(&mut app);
        let id = app.accounts[0].id.clone();
        let uid = app.store.messages(&id, "INBOX", 1).unwrap()[0].uid;
        app.delete_forever(0, "INBOX", &[uid]);
        assert_eq!(
            app.store.pending_deletes(&id).unwrap(),
            [("INBOX".to_string(), 0, vec![uid])]
        );
        app.on_worker(
            0,
            worker::Event::Deleted {
                folder: "INBOX".into(),
                uids: vec![uid],
                all: true,
            },
        );
        assert!(app.store.pending_deletes(&id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn closing_a_written_message_asks_to_keep_it() {
        let mut app = app();
        with_mail(&mut app);
        app.on_key(key(KeyCode::Char('c')));
        for c in "x@y.z".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.confirm, Some(Confirm::CloseComposer));
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.composer.is_none());
        let drafts = app
            .store
            .messages(&app.accounts[0].id, "Drafts", 10)
            .unwrap();
        assert_eq!(drafts.len(), 2, "kept in Drafts");
    }

    #[tokio::test]
    async fn pasted_keys_are_not_run_as_commands() {
        let mut app = app();
        with_mail(&mut app);
        let before = app.items.len();
        let keys: Vec<Event> = "dddd"
            .chars()
            .chain("xyz".chars())
            .map(|c| Event::Key(key(KeyCode::Char(c))))
            .collect();
        app.on_terminal_events(keys);
        assert_eq!(app.items.len(), before, "nothing deleted");
    }

    #[tokio::test]
    async fn an_account_is_added_from_the_form() {
        let mut app = app();
        with_mail(&mut app);
        app.on_key(key(KeyCode::Char('+')));
        assert!(app.setup.is_some());
        for c in "me@fastmail.com".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Tab));
        for c in "app-password".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        app.wait_until(|app| app.setup.is_none()).await;
        assert_eq!(app.accounts.len(), 2);
        assert_eq!(app.accounts[1].account.host, "imap.fastmail.com");
        assert_eq!(
            app.place,
            Place::Folder {
                account: 1,
                folder: "INBOX".into()
            }
        );
        assert!(
            app.sidebar_rows().contains(&Row::AllInboxes),
            "two accounts: every inbox together"
        );
        // Removing it asks, then forgets it.
        app.focus = Focus::Sidebar;
        app.sidebar_selected = app
            .sidebar_rows()
            .iter()
            .position(|r| *r == Row::Account(1))
            .unwrap();
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.confirm, Some(Confirm::RemoveAccount(1)));
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.accounts.len(), 1);
    }

    #[tokio::test]
    async fn pop3_mail_downloaded_twice_is_kept_once() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            Store::in_memory(),
            Settings::default(),
            crate::demo::accounts(),
            false,
            None,
            tx,
            true,
        );
        let raw = b"From: Mom <mom@family.example>\r\nSubject: Call me\r\n\r\nhi\r\n".to_vec();
        let popped = || worker::Popped {
            new: vec![("uidl-1".to_string(), raw.clone())],
            on_server: HashSet::from(["uidl-1".to_string()]),
            ..worker::Popped::default()
        };
        app.on_popped(1, popped());
        // A second check that started before the first was stored.
        app.on_popped(1, popped());
        let inbox = app
            .store
            .messages(&app.accounts[1].id, "INBOX", 10)
            .unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(app.accounts[1].folder_for(Role::Inbox).unwrap().unread, 1);
    }

    #[test]
    fn folders_nest_under_their_parents_and_gmail_parents_hide() {
        let f = |name: &str, role: Option<Role>, selectable: bool| Folder {
            name: name.into(),
            shown: name.into(),
            delimiter: Some("/".into()),
            role,
            selectable,
            unread: 0,
            total: 0,
        };
        let folders = vec![
            f("INBOX", Some(Role::Inbox), true),
            f("[Gmail]", None, false),
            f("[Gmail]/Sent Mail", Some(Role::Sent), true),
            f("Work", None, true),
            f("Work/2026", None, true),
        ];
        let rows = folder_rows(&folders);
        assert_eq!(rows, [(0, 0), (2, 0), (3, 0), (4, 1)]);
        assert_eq!(folder_label(&folders[2]), "Sent Mail");
        assert_eq!(folder_label(&folders[0]), "Inbox");
        assert_eq!(
            link_host("https://user@shop.example:8080/x?y"),
            "shop.example:8080"
        );
        assert_eq!(unquote("'/tmp/a b.pdf'"), "/tmp/a b.pdf");
        assert_eq!(unquote("/tmp/a\\ b.pdf"), "/tmp/a b.pdf");
    }
    /// The selected message as the cache has it.
    fn stored_selected(app: &App) -> Message {
        let item = &app.items[app.selected];
        app.store
            .message(
                &app.accounts[item.account].id,
                &item.message.folder,
                item.message.uid,
            )
            .unwrap()
            .unwrap()
    }

    /// Opens the tag picker and picks `tag` (`None`: a new one).
    fn pick_tag(app: &mut App, tag: Option<&str>) {
        app.on_key(key(KeyCode::Char('t')));
        let Some(Picker {
            kind: PickerKind::Tag { tags, .. },
            items,
            ..
        }) = app.picker.as_ref()
        else {
            panic!("no tag picker");
        };
        assert_eq!(items.len(), tags.len() + 1);
        let n = match tag {
            Some(tag) => tags.iter().position(|t| t == tag).expect(tag),
            None => tags.len(),
        };
        app.picker.as_mut().unwrap().selected = n;
        app.on_key(key(KeyCode::Enter));
    }

    fn last_sent(app: &App) -> Option<(usize, Request)> {
        app.test_sent.borrow().last().cloned()
    }

    #[tokio::test]
    async fn t_puts_a_tag_on_and_takes_it_off_here_and_on_the_server() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        app.selected = 0;
        let uid = app.items[0].message.uid;
        assert!(app.items[0].message.flags.tags.is_empty());
        pick_tag(&mut app, Some("work"));
        assert_eq!(app.items[0].message.flags.tags, ["work"]);
        assert_eq!(stored_selected(&app).flags.tags, ["work"], "kept");
        assert!(
            matches!(
                last_sent(&app),
                Some((0, Request::Tag { ref folder, ref uids, ref tag, on: true, .. }))
                    if folder == "INBOX" && uids == &[uid] && tag == "work"
            ),
            "{:?}",
            last_sent(&app)
        );
        // On every one picked: it comes off.
        let picker_mark = |app: &mut App| {
            app.on_key(key(KeyCode::Char('t')));
            let picker = app.picker.take().unwrap();
            let PickerKind::Tag { tags, .. } = &picker.kind else {
                panic!("no tag picker");
            };
            let n = tags.iter().position(|t| t == "work").unwrap();
            picker.items[n].clone()
        };
        assert_eq!(picker_mark(&mut app), "✓ work");
        pick_tag(&mut app, Some("work"));
        assert!(app.items[0].message.flags.tags.is_empty());
        assert!(stored_selected(&app).flags.tags.is_empty());
        assert!(matches!(
            last_sent(&app),
            Some((0, Request::Tag { on: false, .. }))
        ));
        assert_eq!(picker_mark(&mut app), "  work");
    }

    #[tokio::test]
    async fn a_new_tag_is_typed_and_one_that_cant_be_is_refused() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        app.selected = 0;
        pick_tag(&mut app, None);
        assert!(matches!(
            app.prompt.as_ref().map(|p| p.kind),
            Some(PromptKind::Tag)
        ));
        for c in "To Read".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(stored_selected(&app).flags.tags, ["to-read"]);
        assert_eq!(
            app.status.as_ref().unwrap().text,
            "1 message tagged to-read"
        );
        pick_tag(&mut app, None);
        for c in "a(b".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        let status = app.status.as_ref().unwrap();
        assert!(
            status.error && status.text.starts_with("A tag is"),
            "{}",
            status.text
        );
        assert_eq!(stored_selected(&app).flags.tags, ["to-read"], "unchanged");
    }

    #[tokio::test]
    async fn marked_messages_are_tagged_together_and_pop3_tags_stay_here() {
        let mut app = two_accounts();
        app.open_place(Place::AllInboxes);
        app.focus = Focus::List;
        let work = app.items.iter().filter(|it| it.account == 0).take(2);
        let home = app.items.iter().find(|it| it.account == 1);
        app.marked = work.chain(home).map(Item::key).collect();
        let marked: Vec<_> = app.marked.iter().cloned().collect();
        assert!(marked.iter().any(|k| k.0 == 1), "a POP3 message among them");
        app.test_sent.borrow_mut().clear();
        pick_tag(&mut app, Some("work"));
        for (account, folder, uid) in &marked {
            let m = app
                .store
                .message(&app.accounts[*account].id, folder, *uid)
                .unwrap()
                .unwrap();
            assert!(
                m.flags.tags.contains(&"work".to_string()),
                "{}",
                m.summary.subject
            );
        }
        assert!(app.marked.is_empty());
        let sent = app.test_sent.borrow().clone();
        assert!(
            sent.iter()
                .all(|(account, r)| *account == 0 && matches!(r, Request::Tag { .. })),
            "only the IMAP account's server is told: {sent:?}"
        );
        assert!(!sent.is_empty());
    }

    #[tokio::test]
    async fn a_tag_word_finds_tagged_mail_in_the_cache() {
        let mut app = app();
        with_mail(&mut app);
        app.run_search("tag:work");
        assert_eq!(subjects(&app), ["Quarterly numbers"]);
        app.run_search("tag:work numbers");
        assert_eq!(subjects(&app), ["Quarterly numbers"]);
        app.run_search("tag:work lunch");
        assert!(app.items.is_empty());
        app.run_search("tag:nothing");
        assert!(app.items.is_empty());
    }
    #[tokio::test]
    async fn z_undoes_the_last_change_and_only_what_it_changed() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        let unread = |app: &App| app.accounts[0].folder_for(Role::Inbox).unwrap().unread;
        let seen = |app: &App| -> Vec<bool> {
            app.items
                .iter()
                .take(3)
                .map(|i| i.message.flags.seen)
                .collect()
        };
        let before = seen(&app);
        assert_eq!(before, [false, false, true]);
        assert_eq!(unread(&app), 2);
        app.marked = app.items.iter().take(3).map(Item::key).collect();
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(seen(&app), [true, true, true]);
        app.on_key(key(KeyCode::Char('z')));
        assert_eq!(seen(&app), before, "the one read before stays read");
        assert_eq!(unread(&app), 2);
        assert_eq!(
            app.status.as_ref().unwrap().text,
            "Undone: 3 messages marked read"
        );
        app.on_key(key(KeyCode::Char('z')));
        assert_eq!(app.status.as_ref().unwrap().text, "Nothing to undo");
    }

    #[tokio::test]
    async fn changes_are_undone_newest_first() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        app.selected = 0;
        pick_tag(&mut app, Some("work"));
        app.on_key(key(KeyCode::Char('s')));
        let message = |app: &App| app.items[0].message.flags.clone();
        assert!(message(&app).flagged && message(&app).tags == ["work"]);
        app.on_key(key(KeyCode::Char('z')));
        assert!(!message(&app).flagged && message(&app).tags == ["work"]);
        app.on_key(key(KeyCode::Char('z')));
        assert!(message(&app).tags.is_empty());
        assert!(stored_selected(&app).flags.tags.is_empty());
        assert!(matches!(
            last_sent(&app),
            Some((0, Request::Tag { on: false, .. }))
        ));
    }

    #[tokio::test]
    async fn undoing_a_move_brings_back_only_what_it_moved() {
        // Two messages with one Message-ID (the sender writes it): the
        // first put in Trash, then the second; undoing the second's move
        // leaves the first there.
        let mut app = app();
        with_mail(&mut app);
        let id = app.accounts[0].id.clone();
        for subject in ["P: sign in again", "B: weekly newsletter"] {
            let raw = format!(
                "From: Mallory <m@evil.example>\r\nSubject: {subject}\r\n\
                 Date: Mon, 1 Jan 2035 00:00:00 +0000\r\nMessage-ID: <dup@evil.example>\r\n\r\nhi\r\n"
            );
            (app.store)
                .add_local(&id, "INBOX", raw.as_bytes(), Flags::default(), None)
                .unwrap();
        }
        app.reload();
        app.focus = Focus::List;
        for subject in ["P: sign in again", "B: weekly newsletter"] {
            app.selected = subjects(&app).iter().position(|s| s == subject).unwrap();
            app.on_key(key(KeyCode::Char('d')));
        }
        app.on_key(key(KeyCode::Char('z')));
        let shown = subjects(&app);
        assert!(
            shown.contains(&"B: weekly newsletter".to_string()),
            "{shown:?}"
        );
        assert!(
            !shown.contains(&"P: sign in again".to_string()),
            "{shown:?}"
        );
        let trash = app.accounts[0]
            .folder_for(Role::Trash)
            .unwrap()
            .name
            .clone();
        let kept: Vec<String> = (app.store.messages(&id, &trash, 100).unwrap().into_iter())
            .map(|m| m.summary.subject)
            .collect();
        assert_eq!(kept, ["P: sign in again"]);
    }

    #[tokio::test]
    async fn a_message_moved_to_trash_comes_back_with_z() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        app.selected = 0;
        let total = subjects(&app).len();
        app.on_key(key(KeyCode::Char('d')));
        assert!(!subjects(&app).contains(&"Lunch tomorrow?".to_string()));
        app.on_key(key(KeyCode::Char('z')));
        assert_eq!(
            app.status.as_ref().unwrap().text,
            "Undone: 1 message moved to Trash"
        );
        assert_eq!(subjects(&app).len(), total);
        assert_eq!(subjects(&app)[0], "Lunch tomorrow?");
        let trash = app.accounts[0]
            .folder_for(Role::Trash)
            .unwrap()
            .name
            .clone();
        assert!(
            app.store
                .messages(&app.accounts[0].id, &trash, 100)
                .unwrap()
                .iter()
                .all(|m| m.summary.subject != "Lunch tomorrow?")
        );
    }

    #[tokio::test]
    async fn on_the_server_a_move_is_undone_by_where_it_put_the_mail() {
        let mut app = app();
        with_mail(&mut app);
        app.demo = false;
        app.focus = Focus::List;
        app.selected = 0;
        let message = app.items[0].message.clone();
        assert!(!message.summary.message_id.is_empty());
        app.on_key(key(KeyCode::Char('e')));
        let Some((
            0,
            Request::Move {
                folder,
                uids,
                to,
                uidvalidity,
            },
        )) = last_sent(&app)
        else {
            panic!("{:?}", last_sent(&app));
        };
        assert_eq!(to, "Archive");
        // Before the server has made it, where it went isn't known.
        app.on_key(key(KeyCode::Char('z')));
        assert!(app.status.as_ref().unwrap().error);
        assert!(matches!(last_sent(&app), Some((0, Request::Move { .. }))));
        app.on_worker(
            0,
            worker::Event::Moved {
                folder,
                uidvalidity,
                uids,
                to,
                to_uidvalidity: 77,
                floor: 501,
            },
        );
        app.on_key(key(KeyCode::Char('z')));
        assert!(
            matches!(
                last_sent(&app),
                Some((0, Request::MoveBack { ref folder, uidvalidity: 77, floor: 501, ref moved, ref to }))
                    if folder == "Archive" && to == "INBOX"
                        && moved == &[(message.summary.message_id.clone(), message.size)]
            ),
            "{:?}",
            last_sent(&app)
        );
        // The server didn't find it there any more.
        app.on_worker(
            0,
            worker::Event::MovedBack {
                folder: "Archive".into(),
                to: "INBOX".into(),
                moved: Vec::new(),
                asked: 1,
            },
        );
        let status = app.status.as_ref().unwrap();
        assert!(
            status.error && status.text.starts_with("1 of them"),
            "{}",
            status.text
        );
    }

    #[tokio::test]
    async fn the_changes_list_undoes_the_one_picked() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        app.selected = 0;
        app.on_key(key(KeyCode::Char('s')));
        app.selected = 1;
        app.on_key(key(KeyCode::Char('s')));
        app.on_key(key(KeyCode::Char('Z')));
        let picker = app.picker.as_ref().expect("the changes");
        assert_eq!(picker.items.len(), 2);
        assert!(
            picker.items[0].ends_with("you: 1 message starred"),
            "{:?}",
            picker.items
        );
        // The older one: the first message's star.
        app.picker.as_mut().unwrap().selected = 1;
        app.on_key(key(KeyCode::Enter));
        assert!(!app.items[0].message.flags.flagged);
        assert!(app.items[1].message.flags.flagged, "the newer one stays");
        app.on_key(key(KeyCode::Char('Z')));
        let items = &app.picker.as_ref().unwrap().items;
        assert!(items[1].ends_with("(undone)"), "{items:?}");
    }
    /// Asks the app as an agent would.
    fn agent_call(app: &mut App, call: Call) -> tokio::sync::oneshot::Receiver<agent::Answer> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let request = agent::Request {
            client: "Claude Code".into(),
            call,
        };
        app.on_agent(request, reply);
        answer
    }

    /// What the app answered at once.
    fn agent_now(app: &mut App, call: Call) -> agent::Answer {
        agent_call(app, call).try_recv().expect("answered at once")
    }

    /// The ids of what an agent's search finds.
    fn agent_ids(app: &mut App, args: agent::SearchArgs) -> Vec<String> {
        let found = agent_now(app, Call::SearchMessages(args)).unwrap();
        (found["messages"].as_array().unwrap().iter())
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn first_id(app: &mut App) -> String {
        agent_ids(app, agent::SearchArgs::default()).remove(0)
    }

    #[tokio::test]
    async fn agents_read_the_cache_and_a_message_with_links_shown_by_site() {
        let mut app = app();
        with_mail(&mut app);
        let accounts = agent_now(&mut app, Call::ListAccounts(agent::NoArgs {})).unwrap();
        assert_eq!(accounts[0]["address"], app.accounts[0].id.as_str());
        let name = accounts[0]["account"].as_str().unwrap().to_string();
        let folders = agent_now(
            &mut app,
            Call::ListFolders(agent::FolderArgs { account: name }),
        )
        .unwrap();
        assert!(
            folders
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["role"] == "trash")
        );
        let found = agent_now(
            &mut app,
            Call::SearchMessages(agent::SearchArgs {
                unread_only: true,
                ..Default::default()
            }),
        )
        .unwrap();
        let messages = found["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["subject"], "Lunch tomorrow?");
        // Previews, subjects and names too: the demo's lunch mail has a link
        // in its first lines.
        assert!(
            messages[0]["preview"]
                .as_str()
                .unwrap()
                .contains("[link: ramen.example.com]"),
            "{found}"
        );
        assert!(!found.to_string().contains("https://"), "{found}");
        let tagged = agent_ids(
            &mut app,
            agent::SearchArgs {
                query: Some("tag:work".into()),
                ..Default::default()
            },
        );
        assert_eq!(tagged.len(), 1);
        let id = messages[0]["id"].as_str().unwrap().to_string();
        let answer = agent_call(
            &mut app,
            Call::ReadMessage(agent::ReadArgs {
                id,
                max_chars: None,
            }),
        );
        let read = tokio::time::timeout(Duration::from_secs(5), answer)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = read["text"].as_str().unwrap();
        assert!(text.contains("[link: ramen.example.com]"), "{text}");
        assert!(!text.contains("https://"), "{text}");
        assert_eq!(read["attachments"][0]["name"], "menu.pdf");
        assert!(!app.items[0].message.flags.seen, "not marked read");
    }

    #[tokio::test]
    async fn agents_reads_download_a_message_once_and_within_limits() {
        let mut app = app();
        with_mail(&mut app);
        app.demo = false;
        let id = app.accounts[0].id.clone();
        let raw = b"From: a@b.c\r\nSubject: Not here yet\r\nMessage-ID: <n@b.c>\r\n\r\nhi\r\n";
        let header = crate::store::Header {
            uid: 77001,
            summary: mail::summarize(raw),
            flags: Flags::default(),
            size: 4000,
        };
        app.store.add_headers(&id, "INBOX", &[header]).unwrap();
        let handle = app.agent_id(0, "INBOX", 77001);
        let read = |app: &mut App| {
            agent_call(
                app,
                Call::ReadMessage(agent::ReadArgs {
                    id: handle.clone(),
                    max_chars: None,
                }),
            )
        };
        let downloads = |app: &App| {
            (app.test_sent.borrow().iter())
                .filter(|(_, r)| matches!(r, Request::Body { uid: 77001, .. }))
                .count()
        };
        let waiting: Vec<_> = (0..8).map(|_| read(&mut app)).collect();
        assert_eq!(downloads(&app), 1, "one download for every read");
        // Over the hour's bytes: refused, not downloaded.
        app.agent_fetched
            .push_back((Instant::now(), agent::DOWNLOAD_BYTES_PER_HOUR));
        app.agent_downloads.clear();
        let mut refused = read(&mut app);
        assert!(refused.try_recv().unwrap().is_err());
        assert_eq!(downloads(&app), 1);
        drop(waiting);
    }

    #[tokio::test]
    async fn agents_keep_to_the_users_own_folders() {
        let mut app = app();
        with_mail(&mut app);
        // Another user's folder the server lists, with a name like one of
        // the user's own.
        app.accounts[0].folders.push(Folder {
            name: "shared/bob/Invoices".into(),
            shown: "shared/bob/Invoices".into(),
            delimiter: Some("/".into()),
            role: None,
            selectable: true,
            unread: 0,
            total: 0,
        });
        let name = app.accounts[0].account.shown_name();
        let folders = agent_now(
            &mut app,
            Call::ListFolders(agent::FolderArgs { account: name }),
        )
        .unwrap()
        .to_string();
        assert!(!folders.contains("bob"), "{folders}");
        for to in ["shared/bob/Invoices", "drafts", "sent"] {
            let id = first_id(&mut app);
            let refused = agent_now(
                &mut app,
                Call::MoveMessages(agent::MoveArgs {
                    ids: vec![id],
                    folder: to.into(),
                }),
            );
            assert!(refused.is_err(), "{to}: {refused:?}");
        }
    }

    #[tokio::test]
    async fn an_agents_name_never_hides_its_question() {
        let mut app = app();
        with_mail(&mut app);
        let id = first_id(&mut app);
        let (reply, _answer) = tokio::sync::oneshot::channel();
        // Halfwidth sound marks: no columns to unicode-width, one each as
        // drawn.
        app.on_agent(
            agent::Request {
                client: "\u{FF9E}".repeat(40),
                call: Call::MoveToTrash(agent::IdsArgs { ids: vec![id] }),
            },
            reply,
        );
        let shown = crate::ui::tests::screen(&mut app, 160, 30);
        for said in [
            "(an AI agent, as it calls",
            "itself) wants to move 1 message to Trash:",
        ] {
            assert!(shown.contains(said), "{said}: {shown}");
        }
    }

    #[tokio::test]
    async fn an_agents_change_waits_for_y_and_z_undoes_it() {
        let mut app = app();
        with_mail(&mut app);
        let id = first_id(&mut app);
        let mut answer = agent_call(
            &mut app,
            Call::MoveToTrash(agent::IdsArgs { ids: vec![id] }),
        );
        assert!(answer.try_recv().is_err(), "it waits");
        assert!(matches!(app.confirm, Some(Confirm::Agent { .. })));
        let shown = crate::ui::tests::screen(&mut app, 160, 30);
        for said in [
            "An AI agent asks",
            "Claude Code",
            "wants to move 1",
            "Lunch tomorrow? · Inbox",
        ] {
            assert!(shown.contains(said), "{said}: {shown}");
        }
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('y')));
        let done = answer.try_recv().unwrap().unwrap();
        assert_eq!(done["done"], "1 message moved to Trash");
        assert!(!subjects(&app).contains(&"Lunch tomorrow?".to_string()));
        let change = app.store.changes(1).unwrap().remove(0);
        assert_eq!(change.who, "Claude Code");
        assert_eq!(done["change"], change.id);
        let status = app.status.as_ref().unwrap();
        assert_eq!(
            status.text,
            "Claude Code: 1 message moved to Trash · z undoes"
        );
        app.on_key(key(KeyCode::Char('z')));
        assert!(subjects(&app).contains(&"Lunch tomorrow?".to_string()));
    }

    #[tokio::test]
    async fn no_means_no() {
        let mut app = app();
        with_mail(&mut app);
        let id = first_id(&mut app);
        let mut answer = agent_call(
            &mut app,
            Call::Star(agent::StarArgs {
                ids: vec![id],
                starred: true,
            }),
        );
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('n')));
        let refused = answer.try_recv().unwrap().unwrap_err();
        assert!(refused.contains("said no"), "{refused}");
        assert!(!app.items[0].message.flags.flagged);
        assert!(app.confirm.is_none());
        // A key pressed as the question comes up answers nothing: it
        // stays, for a key pressed once it can be read.
        let id = first_id(&mut app);
        let mut answer = agent_call(
            &mut app,
            Call::Star(agent::StarArgs {
                ids: vec![id],
                starred: true,
            }),
        );
        app.on_key(key(KeyCode::Char('y')));
        assert!(answer.try_recv().is_err(), "too soon to be an answer");
        assert!(matches!(app.confirm, Some(Confirm::Agent { .. })));
        assert!(!app.items[0].message.flags.flagged);
        app.confirm_at = Instant::now() - AGENT_GRACE;
        app.on_key(key(KeyCode::Char('j')));
        assert!(answer.try_recv().unwrap().is_err(), "any other key is a no");
        assert!(!app.items[0].message.flags.flagged);
    }

    #[tokio::test]
    async fn an_agent_is_told_when_its_undo_isnt_made() {
        let mut app = app();
        with_mail(&mut app);
        app.settings.confirm_agents = false;
        // On a server: a move is undone once the server says where it went.
        app.demo = false;
        let id = first_id(&mut app);
        let moved = agent_now(
            &mut app,
            Call::MoveToTrash(agent::IdsArgs { ids: vec![id] }),
        );
        let change = moved.unwrap()["change"].as_i64().unwrap();
        let undo = agent_now(&mut app, Call::UndoChange(agent::UndoArgs { change }));
        let refused = undo.unwrap_err();
        assert!(refused.contains("hasn't reached the server"), "{refused}");
        assert!(!app.store.change(change).unwrap().unwrap().undone);
    }

    #[tokio::test]
    async fn without_asking_a_change_is_made_at_once_and_said() {
        let mut app = app();
        with_mail(&mut app);
        app.settings.confirm_agents = false;
        let id = first_id(&mut app);
        let done = agent_now(
            &mut app,
            Call::SetTag(agent::TagArgs {
                ids: vec![id],
                tag: "Receipts".into(),
                on: true,
            }),
        )
        .unwrap();
        assert_eq!(done["done"], "1 message tagged receipts");
        assert_eq!(app.items[0].message.flags.tags, ["receipts"]);
        assert_eq!(
            app.status.as_ref().unwrap().text,
            "Claude Code: 1 message tagged receipts · z undoes"
        );
        assert!(app.confirm.is_none());
    }

    #[tokio::test]
    async fn agents_never_delete_and_keep_to_the_rules() {
        let mut app = app();
        with_mail(&mut app);
        app.settings.confirm_agents = false;
        let refused = |answer: agent::Answer, says: &str| {
            let error = answer.expect_err(says);
            assert!(error.contains(says), "{says}: {error}");
        };
        let id = first_id(&mut app);
        agent_now(
            &mut app,
            Call::MoveToTrash(agent::IdsArgs { ids: vec![id] }),
        )
        .unwrap();
        // From Trash, a move to Trash is a delete for good: never.
        let in_trash = agent_ids(
            &mut app,
            agent::SearchArgs {
                folder: Some("trash".into()),
                ..Default::default()
            },
        );
        assert!(!in_trash.is_empty());
        refused(
            agent_now(
                &mut app,
                Call::MoveToTrash(agent::IdsArgs {
                    ids: in_trash.clone(),
                }),
            ),
            "only the user deletes mail for good",
        );
        let id = first_id(&mut app);
        refused(
            agent_now(
                &mut app,
                Call::MoveMessages(agent::MoveArgs {
                    ids: vec![id.clone()],
                    folder: "Trash".into(),
                }),
            ),
            "Use move_to_trash",
        );
        refused(
            agent_now(
                &mut app,
                Call::MoveToTrash(agent::IdsArgs {
                    ids: vec![id.clone(); 51],
                }),
            ),
            "At most 50",
        );
        refused(
            agent_now(
                &mut app,
                Call::Star(agent::StarArgs {
                    ids: vec!["bm9wZQ".into()],
                    starred: true,
                }),
            ),
            "isn't a message tuimail has",
        );
        // The user's own changes are theirs to undo.
        app.focus = Focus::List;
        app.on_key(key(KeyCode::Char('s')));
        let mine = app.store.changes(1).unwrap().remove(0).id;
        refused(
            agent_now(&mut app, Call::UndoChange(agent::UndoArgs { change: mine })),
            "the user's",
        );
        // An hour's worth of changes is the most.
        app.agent_spent
            .push_back((Instant::now(), agent::CHANGES_PER_HOUR, 0));
        refused(
            agent_now(
                &mut app,
                Call::Star(agent::StarArgs {
                    ids: vec![id],
                    starred: true,
                }),
            ),
            "in the last hour",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_agent_reaches_tuimail_through_its_socket_while_the_setting_is_on() {
        let mut app = app();
        with_mail(&mut app);
        let dir = crate::tools::private_temp_dir().unwrap();
        let socket = agent::socket(&dir, false);
        app.agent_socket = Some(socket.clone());
        app.settings.agents = true;
        app.listen_for_agents();
        assert!(app.agents.is_some());
        let request = agent::Request {
            client: "Claude Code".into(),
            call: Call::ListAccounts(agent::NoArgs {}),
        };
        let asked = {
            let (socket, request) = (socket.clone(), request.clone());
            tokio::spawn(async move { agent::ask(&socket, &request).await })
        };
        app.wait_until(|_| asked.is_finished()).await;
        let accounts = asked.await.unwrap().unwrap();
        assert_eq!(accounts[0]["address"], app.accounts[0].id.as_str());
        app.settings.agents = false;
        app.listen_for_agents();
        assert!(!socket.exists(), "the socket goes");
        assert_eq!(
            agent::ask(&socket, &request).await.unwrap_err(),
            agent::NOT_LISTENING
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agents_turned_off_elsewhere_are_off_here() {
        let mut app = app();
        with_mail(&mut app);
        let dir = crate::tools::private_temp_dir().unwrap();
        let socket = agent::socket(&dir, false);
        let settings = crate::settings::path(&dir);
        std::fs::write(&settings, "agents = true\n").unwrap();
        app.settings_path = Some(settings.clone());
        app.agent_socket = Some(socket.clone());
        app.settings.agents = true;
        app.listen_for_agents();
        let request = agent::Request {
            client: "Claude Code".into(),
            call: Call::ListAccounts(agent::NoArgs {}),
        };
        async fn ask(
            app: &mut App,
            socket: &std::path::Path,
            request: &agent::Request,
        ) -> agent::Answer {
            let (socket, request) = (socket.to_path_buf(), request.clone());
            let asked = tokio::spawn(async move { agent::ask(&socket, &request).await });
            app.wait_until(|_| asked.is_finished()).await;
            asked.await.unwrap()
        }
        assert!(ask(&mut app, &socket, &request).await.is_ok());
        // Another tuimail (or a hand edit) turns them off.
        std::fs::write(&settings, "agents = false\n").unwrap();
        assert_eq!(
            ask(&mut app, &socket, &request).await.unwrap_err(),
            "The user turned AI agents off"
        );
        app.on_tick();
        assert!(
            app.agents.is_none() && !app.settings.agents,
            "and stops listening"
        );
        assert!(!socket.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn p_signs_an_account_in_and_a_finished_sign_in_logs_in() {
        let mut app = app();
        with_mail(&mut app);
        let mut account = (*app.accounts[0].account).clone();
        account.host = "outlook.office365.com".into();
        account.auth = account::Auth::Oauth2;
        account.oauth_client_id = String::new();
        app.accounts[0].account = Arc::new(account.clone());
        app.focus = Focus::Sidebar;
        app.sidebar_selected = 1;
        app.on_key(key(KeyCode::Char('P')));
        if crate::oauth::MICROSOFT_CLIENT_ID.is_empty() {
            let status = app.status.as_ref().unwrap();
            assert!(
                status.error && status.text.contains("can't sign in"),
                "{}",
                status.text
            );
        }
        assert!(app.prompt.is_none(), "no password to type");
        // With an app to sign in as, the browser's sign-in starts; not in
        // the demo, whose accounts are made up.
        account.oauth_client_id = "my-app".into();
        app.accounts[0].account = Arc::new(account);
        app.on_key(key(KeyCode::Char('P')));
        assert_eq!(
            app.status.as_ref().unwrap().text,
            "The demo doesn't sign in"
        );
        assert!(!app.signing_in);
        let id = app.accounts[0].id.clone();
        app.on_signed_in(
            &id,
            Ok(crate::oauth::Tokens {
                access: Secret::new("at"),
                refresh: Some(Secret::new("rt")),
                renew_at: Instant::now() + Duration::from_secs(600),
            }),
        );
        assert!(!app.signing_in);
        assert_eq!(app.status.as_ref().unwrap().text, "work: signed in");
        app.on_signed_in(&id, Err("the sign-in was refused: no".into()));
        assert!(app.status.as_ref().unwrap().error);
    }
}
