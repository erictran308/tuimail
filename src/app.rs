//! The app's state and everything that changes it: keys, and what the
//! accounts' workers report. All state lives in [`App`]; nothing else
//! mutates it. The cache (`Store`) is written only from here.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
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
use crate::compose::{self, Draft};
use crate::composer::{self, Composer, Kind};
use crate::icons::Icons;
use crate::imap::FlagKind;
use crate::mail::{self, Addr, Body, Parsed};
use crate::notify::{self, Notifications, Notifier};
use crate::secrets::{Credentials, Secret};
use crate::settings::Settings;
use crate::setup::{self, Setup};
use crate::store::{self, Flags, Folder, Known, Message, Role, Store};
use crate::theme::{self, Colors};
use crate::worker::{self, Endpoints, Request, State, Worker};
use crate::{net, text, tools};

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
const HELP_PAGE: isize = 5;
/// Every this many checks, each folder's counts are asked for too.
const FOLDERS_EVERY: u32 = 4;

pub enum AppEvent {
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
    /// A reply, forward or saved draft made ready for the composer.
    Drafted {
        account: usize,
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
    Checked { result: Result<(), String> },
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
    /// Changes (moves, flags, deletes) not yet made on the server.
    waiting: usize,
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
    pub const CONFIRM_DELETE: usize = 3;
    pub const CONFIRM_SEND: usize = 4;
    pub const NOTIFICATIONS: usize = 5;
    pub const THEMES: usize = 6;
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
    pub picker: Option<Picker>,
    pub confirm: Option<Confirm>,
    confirm_at: Instant,
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
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
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
            picker: None,
            confirm: None,
            confirm_at: Instant::now(),
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
        self.accounts.push(AccountState {
            account,
            id,
            worker,
            state: State::Connecting,
            folders,
            expanded: true,
            syncing: HashSet::new(),
            older_asked: HashSet::new(),
            last_check: None,
            checks: 0,
            popped_once: false,
            pop_again: false,
            waiting: 0,
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
    fn after_draw(&mut self) {
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
        self.accounts[i].syncing.insert("INBOX".into());
        self.send(
            i,
            Request::Pop {
                seen,
                remove,
                count,
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
        if let Some(worker) = &self.accounts[i].worker {
            let _ = worker.tx.send(request);
        }
    }

    fn account_index(&self, id: &str) -> Option<usize> {
        self.accounts.iter().position(|a| a.id == id)
    }

    // Events.

    fn on_event(&mut self, event: AppEvent) {
        match event {
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
            } => match draft {
                // A reply made ready late never takes the place of a
                // message being written, or of the one a question is about.
                Some(_) if self.composer.is_some() || self.confirm.is_some() => {
                    self.info("A message is open: finish it, then start the reply again")
                }
                Some(draft) if account < self.accounts.len() => {
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
                    let from_command = self.accounts[i].account.password == PasswordFrom::Command
                        || self.accounts[i].account.auth == account::Auth::Oauth2;
                    if from_command {
                        self.error(format!(
                            "{name}: the server refused password_command's password; R tries again"
                        ));
                    } else if self.prompt.is_none()
                        && self.composer.is_none()
                        && self.setup.is_none()
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
                match result {
                    Ok(Some(raw)) => {
                        let _ = self.store.save_body(&id, &folder, uid, &raw);
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
                        if let Some(reader) = self.reader.as_mut().filter(|r| r.key() == key) {
                            reader.error = Some("This message is gone from the server".into());
                        }
                    }
                    Err(e) => {
                        if let Some(reader) = self.reader.as_mut().filter(|r| r.key() == key) {
                            reader.error = Some(e);
                        }
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
            E::Waiting(n) => self.accounts[i].waiting = n,
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
                .copied()
                .filter(|(uid, _)| !self.local_flags.contains_key(&(i, s.folder.clone(), *uid)))
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
        let first = !self.accounts[i].popped_once
            && self.store.pop_seen(&id).unwrap_or_default().is_empty();
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
            if now != 0 && uidvalidity != 0 && now != uidvalidity {
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
            self.accounts.iter().map(|a| a.waiting).sum::<usize>() + self.appends.len();
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
            }));
        }
        if matches!(self.place, Place::AllInboxes) {
            items.sort_by_key(|it| std::cmp::Reverse(it.message.summary.date));
        }
        self.items = items;
        self.selected = current
            .and_then(|key| self.items.iter().position(|it| it.key() == key))
            .unwrap_or(self.selected)
            .min(self.items.len().saturating_sub(1));
        self.marked
            .retain(|key| self.items.iter().any(|it| &it.key() == key));
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
        if let Some(reader) = &mut self.reader
            && reader.key() == key
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

    /// Sets a flag here (cache and list) and on the server.
    fn set_flag(&mut self, keys: &[(usize, String, u32)], flag: FlagKind, on: bool) {
        for (account, folder, uids) in Self::grouped(keys) {
            let id = self.accounts[account].id.clone();
            let mut changed = Vec::new();
            for uid in &uids {
                let Ok(Some(message)) = self.store.message(&id, &folder, *uid) else {
                    continue;
                };
                let mut flags = message.flags;
                match flag {
                    FlagKind::Seen => flags.seen = on,
                    FlagKind::Flagged => flags.flagged = on,
                    FlagKind::Answered => flags.answered = on,
                }
                changed.push((*uid, flags));
                self.local_flags
                    .insert((account, folder.clone(), *uid), Instant::now());
                if let Some(item) = self
                    .items
                    .iter_mut()
                    .find(|it| it.key() == (account, folder.clone(), *uid))
                {
                    item.message.flags = flags;
                }
                if let Some(reader) = self
                    .reader
                    .as_mut()
                    .filter(|r| r.key() == (account, folder.clone(), *uid))
                {
                    reader.message.flags = flags;
                }
            }
            let _ = self.store.set_flags(&id, &folder, &changed);
            if flag == FlagKind::Seen {
                let delta = changed.len() as i64;
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
        self.set_flag(&targets, flag, on);
        if flag == FlagKind::Seen
            && let Some(reader) = self.reader.as_mut()
        {
            // Marked unread by hand: the reader doesn't mark it read again.
            reader.marked_read = true;
        }
        self.marked.clear();
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
        if self.accounts[account].is_pop() || self.demo {
            let _ = self.store.move_local(&id, folder, uids, to);
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

    /// `d`: to Trash, or for good from Trash (asking first).
    fn delete(&mut self, confirmed: bool) {
        let targets = self.targets();
        for (account, folder, uids) in Self::grouped(&targets) {
            let trash = self.accounts[account]
                .folder_for(Role::Trash)
                .map(|f| f.name.clone());
            let forever = trash.as_deref().is_none_or(|t| t == folder);
            if forever && !confirmed {
                self.confirm(Confirm::DeleteForever {
                    uidvalidity: self.uidvalidity(account, &folder),
                    account,
                    folder,
                    uids,
                });
                return;
            }
            if !forever && !confirmed && self.settings.confirm_delete {
                self.confirm(Confirm::Delete {
                    uidvalidity: self.uidvalidity(account, &folder),
                    account,
                    folder,
                    uids,
                });
                return;
            }
            match trash.filter(|_| !forever) {
                Some(trash) => {
                    self.move_messages(account, &folder, &uids, &trash);
                    self.info(format!("{} moved to Trash", plural(uids.len(), "message")));
                }
                None => self.delete_forever(account, &folder, &uids),
            }
        }
    }

    fn delete_forever(&mut self, account: usize, folder: &str, uids: &[u32]) {
        let id = self.accounts[account].id.clone();
        let keys: Vec<_> = uids
            .iter()
            .map(|u| (account, folder.to_string(), *u))
            .collect();
        let unread = self.unread_among(account, folder, uids);
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
        let index = key.0;
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
                    account: index,
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
            if needs_password && self.accounts[i].account.password != PasswordFrom::Command {
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
        if !matches!(self.accounts[i].state, State::Online | State::Connecting) {
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
            KeyCode::Char('d') | KeyCode::Delete => self.delete(false),
            KeyCode::Char('e') => self.move_to_role(Role::Archive),
            KeyCode::Char('!') => self.move_to_role(Role::Junk),
            KeyCode::Char('m') => self.pick_folder(),
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
                            self.prompt = Some(Prompt::new(PromptKind::Password(i), ""));
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
                    self.prompt = Some(Prompt::new(PromptKind::Password(*i), ""));
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
            KeyCode::Esc => self.prompt = None,
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
                            j != i && matches!(self.accounts[j].state, State::NeedPassword { .. })
                        });
                        if let Some(j) = next {
                            self.prompt = Some(Prompt::new(PromptKind::Password(j), ""));
                        }
                    }
                    PromptKind::Attach => self.attach(&text),
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
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let copied = self
            .clipboard
            .as_mut()
            .is_some_and(|c| c.set_text(text.to_string()).is_ok());
        if copied || copy_through_terminal(text).is_ok() {
            self.info("Copied");
        } else {
            self.error("Couldn't copy it");
        }
    }

    fn confirm(&mut self, confirm: Confirm) {
        self.confirm = Some(confirm);
        self.confirm_at = Instant::now();
    }

    /// `y` answers yes; for an unsaved message, `n` discards it; any other
    /// key cancels.
    fn on_confirm_key(&mut self, key: KeyEvent) {
        let early = self.confirm_at.elapsed() < CONFIRM_GRACE;
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        let yes = key.code == KeyCode::Char('y') && !early;
        let no = key.code == KeyCode::Char('n') && !early;
        match confirm {
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
            });
        }
        // Indexes past `i` moved down by one: nothing may keep one.
        self.reader = None;
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
            HelpMenu::CONFIRM_DELETE => {
                self.save_setting("confirm_delete", flip(self.settings.confirm_delete))
            }
            HelpMenu::CONFIRM_SEND => {
                self.save_setting("confirm_send", flip(self.settings.confirm_send))
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
            Ok(settings) => self.settings = settings,
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
            let _ = self.tx.send(AppEvent::Checked { result: Ok(()) });
            setup.checking = true;
            return;
        }
        setup.checking = true;
        setup.error = None;
        let password = setup.password();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = check_login(&account, &password).await;
            let _ = tx.send(AppEvent::Checked {
                result: result.map_err(|e| text::one_line(&format!("{e:#}"), 300)),
            });
        });
    }

    fn on_checked(&mut self, result: Result<(), String>) {
        let keychain = self.keychain;
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        setup.checking = false;
        if let Err(e) = result {
            setup.error = Some(format!("{e} · Ctrl-f saves it anyway"));
            return;
        }
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
        if account.password == PasswordFrom::Keychain && !self.demo {
            let id = account.id();
            let secret = Secret::new(password.clone());
            let tx = self.tx.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::secrets::keychain_set(&id, &secret).await {
                    let _ = tx.send(AppEvent::Checked {
                        result: Err(format!("{e:#}")),
                    });
                }
            });
        }
        self.setup = None;
        let name = account.shown_name();
        let i = self.add_account(account, Some(Secret::new(password)));
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
    // Browsers skip any `/` and `\` after the scheme, and end the host at
    // `\` as at `/`, in http and https.
    let rest = url.split_once(':').map_or(url, |(_, r)| r);
    let rest = rest.trim_start_matches(['/', '\\']);
    let host = rest.split(['/', '\\', '?', '#']).next().unwrap_or(rest);
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = text::one_line(host, host.len());
    // Letters from other alphabets are shown as the browser sends them
    // (punycode, `xn--`): `аpple.com` with a Cyrillic а isn't apple.com.
    let host = if host.is_ascii() {
        host
    } else {
        host.to_lowercase()
            .split(['.', '。', '．', '｡'])
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
        app.confirm_at = Instant::now() - CONFIRM_GRACE;
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
        assert_eq!(app.items.len(), 7);
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
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.items.len(), 6);
        assert!(!subjects(&app).contains(&"Lunch tomorrow?".to_string()));
        let trash = Place::Folder {
            account: 0,
            folder: "Trash".into(),
        };
        app.open_place(trash);
        assert_eq!(subjects(&app), ["Lunch tomorrow?"]);
        app.on_key(key(KeyCode::Char('d')));
        assert!(matches!(app.confirm, Some(Confirm::DeleteForever { .. })));
        app.confirm_at = Instant::now() - CONFIRM_GRACE;
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.items.is_empty(), "gone for good");
    }

    #[tokio::test]
    async fn marked_messages_move_together() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(app.marked.len(), 2);
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(app.items.len(), 5);
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
        app.on_key(key(KeyCode::Char('/')));
        for c in "bob q3".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.search.as_deref(), Some("bob q3"));
        assert_eq!(subjects(&app), ["Quarterly numbers"]);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.search, None);
        assert_eq!(app.items.len(), 7);
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
        app.confirm_at = Instant::now() - CONFIRM_GRACE;
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
        app.confirm_at = Instant::now() - CONFIRM_GRACE;
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
        app.confirm_at = Instant::now() - CONFIRM_GRACE;
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
        app.accounts[0].waiting = 2;
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.quit);
        assert_eq!(app.confirm, Some(Confirm::Quit { waiting: 2 }));
        app.confirm_at = Instant::now() - CONFIRM_GRACE;
        app.on_key(key(KeyCode::Char('n')));
        assert!(!app.quit, "stays");
        app.accounts[0].waiting = 0;
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
        app.confirm_at = Instant::now() - CONFIRM_GRACE;
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
        let keys: Vec<Event> = "dddd"
            .chars()
            .chain("xyz".chars())
            .map(|c| Event::Key(key(KeyCode::Char(c))))
            .collect();
        app.on_terminal_events(keys);
        assert_eq!(app.items.len(), 7, "nothing deleted");
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
        app.confirm_at = Instant::now() - CONFIRM_GRACE;
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
}
