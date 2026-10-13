//! What tuimail keeps between runs, in `tuimail.db` (SQLite) in the data
//! folder: each account's folders, the headers of their messages, the
//! messages opened (so they read offline), and for POP3 accounts the mail
//! itself. It never leaves this computer.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::config;
use crate::mail::{self, Addr, Summary};
use crate::{tags, text};

pub struct Store {
    db: Connection,
}

/// What a folder is for, from the server's SPECIAL-USE attributes or its
/// name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    Inbox,
    Drafts,
    Sent,
    Archive,
    All,
    Flagged,
    Junk,
    Trash,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Inbox => "inbox",
            Role::Drafts => "drafts",
            Role::Sent => "sent",
            Role::Archive => "archive",
            Role::All => "all",
            Role::Flagged => "flagged",
            Role::Junk => "junk",
            Role::Trash => "trash",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "inbox" => Role::Inbox,
            "drafts" => Role::Drafts,
            "sent" => Role::Sent,
            "archive" => Role::Archive,
            "all" => Role::All,
            "flagged" => Role::Flagged,
            "junk" => Role::Junk,
            "trash" => Role::Trash,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Folder {
    /// The name on the server (modified UTF-7 for IMAP), used in commands.
    pub name: String,
    /// The name as shown.
    pub shown: String,
    pub delimiter: Option<String>,
    pub role: Option<Role>,
    pub selectable: bool,
    pub unread: u32,
    pub total: u32,
}

/// What's kept of a folder's state on the server, for the next sync.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Known {
    pub uidvalidity: u32,
    /// Every message with a UID from `low` to `high` is in the cache.
    pub low: u32,
    pub high: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    pub draft: bool,
    /// Its tags ([`tags`]), sorted.
    pub tags: Vec<String>,
}

/// A message in the list.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub folder: String,
    pub uid: u32,
    pub summary: Summary,
    pub flags: Flags,
    pub size: u32,
    /// The start of its text, once it's been downloaded.
    pub preview: String,
}

/// A message's headers as fetched, to store.
#[derive(Clone, Debug, PartialEq)]
pub struct Header {
    pub uid: u32,
    pub summary: Summary,
    pub flags: Flags,
    pub size: u32,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS folders (
    account TEXT NOT NULL,
    name TEXT NOT NULL,
    shown TEXT NOT NULL,
    delimiter TEXT,
    role TEXT,
    selectable INTEGER NOT NULL DEFAULT 1,
    uidvalidity INTEGER NOT NULL DEFAULT 0,
    low INTEGER NOT NULL DEFAULT 0,
    high INTEGER NOT NULL DEFAULT 0,
    unread INTEGER NOT NULL DEFAULT 0,
    total INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account, name)
);
CREATE TABLE IF NOT EXISTS messages (
    account TEXT NOT NULL,
    folder TEXT NOT NULL,
    uid INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    in_reply_to TEXT NOT NULL,
    refs TEXT NOT NULL,
    date INTEGER NOT NULL,
    from_name TEXT NOT NULL,
    from_addr TEXT NOT NULL,
    to_list TEXT NOT NULL,
    cc_list TEXT NOT NULL,
    subject TEXT NOT NULL,
    attachments INTEGER NOT NULL DEFAULT 0,
    seen INTEGER NOT NULL DEFAULT 0,
    flagged INTEGER NOT NULL DEFAULT 0,
    answered INTEGER NOT NULL DEFAULT 0,
    draft INTEGER NOT NULL DEFAULT 0,
    size INTEGER NOT NULL DEFAULT 0,
    preview TEXT NOT NULL DEFAULT '',
    pop_id TEXT,
    tags TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (account, folder, uid)
);
CREATE INDEX IF NOT EXISTS messages_by_date ON messages(account, folder, date DESC);
CREATE TABLE IF NOT EXISTS bodies (
    account TEXT NOT NULL,
    folder TEXT NOT NULL,
    uid INTEGER NOT NULL,
    raw BLOB NOT NULL,
    PRIMARY KEY (account, folder, uid)
);
-- Deletes for good asked for and not yet confirmed by the server: made
-- again at the next start, so quitting (or a lost connection) never leaves
-- a message the user deleted on the server, or hidden there but kept.
CREATE TABLE IF NOT EXISTS pending_deletes (
    account TEXT NOT NULL,
    folder TEXT NOT NULL,
    uidvalidity INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    PRIMARY KEY (account, folder, uid)
);
CREATE TABLE IF NOT EXISTS pop_seen (
    account TEXT NOT NULL,
    pop_id TEXT NOT NULL,
    remove INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account, pop_id)
);
CREATE TABLE IF NOT EXISTS addresses (
    addr TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    used INTEGER NOT NULL DEFAULT 0,
    last INTEGER NOT NULL DEFAULT 0
);
";

const MESSAGE_COLUMNS: &str = "folder, uid, message_id, in_reply_to, refs, date, from_name, \
     from_addr, to_list, cc_list, subject, attachments, seen, flagged, answered, draft, size, \
     preview, tags";

/// Messages bigger than this aren't kept: they're fetched again to read.
pub const MAX_KEPT_BODY: usize = 25 << 20;

fn message_row(row: &rusqlite::Row) -> rusqlite::Result<Message> {
    // Cleaned again on the way out: the file could have been changed.
    let one = |i: usize, max: usize| -> rusqlite::Result<String> {
        Ok(text::one_line(&row.get::<_, String>(i)?, max))
    };
    Ok(Message {
        folder: row.get(0)?,
        uid: row.get(1)?,
        summary: Summary {
            message_id: one(2, 300)?,
            in_reply_to: one(3, 300)?,
            references: one(4, 20_000)?,
            date: row.get(5)?,
            from: Addr {
                name: one(6, mail::MAX_NAME)?,
                addr: one(7, mail::MAX_NAME)?,
            },
            to: one(8, mail::MAX_ADDRESSES)?,
            cc: one(9, mail::MAX_ADDRESSES)?,
            subject: one(10, mail::MAX_SUBJECT)?,
            attachments: row.get(11)?,
        },
        flags: Flags {
            seen: row.get(12)?,
            flagged: row.get(13)?,
            answered: row.get(14)?,
            draft: row.get(15)?,
            tags: tags::from_stored(&row.get::<_, String>(18)?),
        },
        size: row.get(16)?,
        preview: one(17, 300)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let shown = config::shown(path);
        // Not through a link: the cache is the file in the data folder.
        let flags = rusqlite::OpenFlags::default() | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let db = Connection::open_with_flags(path, flags)
            .with_context(|| format!("cannot open {shown}"))?;
        // Deleted rows (a message deleted) are overwritten, not left
        // readable in the file's free pages; under WAL once checkpointed,
        // which removals do.
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA secure_delete = ON;")?;
        db.execute_batch(SCHEMA)
            .and_then(|()| Self::migrate(&db))
            .with_context(|| format!("cannot set up {shown}"))?;
        Self::protect(path).with_context(|| format!("cannot protect {shown}"))?;
        Ok(Self { db })
    }

    /// A store that lives only in memory: the demo's, and tests'.
    pub fn in_memory() -> Self {
        let db = Connection::open_in_memory().expect("SQLite opens in memory");
        db.execute_batch(SCHEMA).expect("the schema is valid");
        Self { db }
    }

    /// Adds what a cache made by an older tuimail lacks.
    fn migrate(db: &Connection) -> rusqlite::Result<()> {
        let tagged: bool = db.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'tags'",
            [],
            |r| r.get(0),
        )?;
        if !tagged {
            db.execute_batch("ALTER TABLE messages ADD COLUMN tags TEXT NOT NULL DEFAULT '';")?;
        }
        Ok(())
    }

    /// The database and its journal files readable only by you, whatever
    /// the umask.
    fn protect(path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["", "-wal", "-shm"] {
                let mut file = path.as_os_str().to_owned();
                file.push(suffix);
                match std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)) {
                    // The journal files come and go with the connection.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound && !suffix.is_empty() => {}
                    result => result?,
                }
            }
        }
        #[cfg(not(unix))]
        let _ = path;
        Ok(())
    }

    /// Writes deleted pages out of the journal, so removed mail is gone
    /// from the file.
    fn scrub(&self) {
        let _ = self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }

    // Folders.

    pub fn folders(&self, account: &str) -> Result<Vec<Folder>> {
        let mut stmt = self.db.prepare(
            "SELECT name, shown, delimiter, role, selectable, unread, total FROM folders
             WHERE account = ?1",
        )?;
        let rows = stmt.query_map(params![account], |row| {
            Ok(Folder {
                name: row.get(0)?,
                shown: text::one_line(&row.get::<_, String>(1)?, 200),
                delimiter: row.get(2)?,
                role: row
                    .get::<_, Option<String>>(3)?
                    .and_then(|r| Role::parse(&r)),
                selectable: row.get(4)?,
                unread: row.get(5)?,
                total: row.get(6)?,
            })
        })?;
        let mut folders = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        sort_folders(&mut folders);
        Ok(folders)
    }

    /// Replaces the account's folder list with `folders`; folders gone from
    /// the server go, with their messages.
    pub fn set_folders(&mut self, account: &str, folders: &[Folder]) -> Result<()> {
        let tx = self.db.transaction()?;
        let names: HashSet<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        let old: Vec<String> = {
            let mut stmt = tx.prepare("SELECT name FROM folders WHERE account = ?1")?;
            let rows = stmt.query_map(params![account], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut removed = false;
        for name in old.iter().filter(|n| !names.contains(n.as_str())) {
            for table in ["folders", "messages", "bodies"] {
                let column = if table == "folders" { "name" } else { "folder" };
                tx.execute(
                    &format!("DELETE FROM {table} WHERE account = ?1 AND {column} = ?2"),
                    params![account, name],
                )?;
            }
            removed = true;
        }
        for f in folders {
            tx.execute(
                "INSERT INTO folders (account, name, shown, delimiter, role, selectable)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(account, name) DO UPDATE SET shown = excluded.shown,
                     delimiter = excluded.delimiter, role = excluded.role,
                     selectable = excluded.selectable",
                params![
                    account,
                    f.name,
                    f.shown,
                    f.delimiter,
                    f.role.map(Role::as_str),
                    f.selectable
                ],
            )?;
        }
        tx.commit()?;
        if removed {
            self.scrub();
        }
        Ok(())
    }

    pub fn set_counts(&self, account: &str, folder: &str, unread: u32, total: u32) -> Result<()> {
        self.db.execute(
            "UPDATE folders SET unread = ?3, total = ?4 WHERE account = ?1 AND name = ?2",
            params![account, folder, unread, total],
        )?;
        Ok(())
    }

    /// Counts a folder's unread and total messages from what's cached, for
    /// folders kept here (POP3) and after a change made here.
    pub fn count(&self, account: &str, folder: &str) -> Result<(u32, u32)> {
        Ok(self.db.query_row(
            "SELECT COALESCE(SUM(seen = 0), 0), COUNT(*) FROM messages
             WHERE account = ?1 AND folder = ?2",
            params![account, folder],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    pub fn known(&self, account: &str, folder: &str) -> Result<Known> {
        Ok(self
            .db
            .query_row(
                "SELECT uidvalidity, low, high FROM folders WHERE account = ?1 AND name = ?2",
                params![account, folder],
                |r| {
                    Ok(Known {
                        uidvalidity: r.get(0)?,
                        low: r.get(1)?,
                        high: r.get(2)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn set_known(&self, account: &str, folder: &str, known: Known) -> Result<()> {
        self.db.execute(
            "UPDATE folders SET uidvalidity = ?3, low = ?4, high = ?5
             WHERE account = ?1 AND name = ?2",
            params![account, folder, known.uidvalidity, known.low, known.high],
        )?;
        Ok(())
    }

    /// Forgets a folder's messages: its UIDs mean other messages now.
    pub fn reset_folder(&self, account: &str, folder: &str) -> Result<()> {
        for table in ["messages", "bodies"] {
            self.db.execute(
                &format!("DELETE FROM {table} WHERE account = ?1 AND folder = ?2"),
                params![account, folder],
            )?;
        }
        self.db.execute(
            "UPDATE folders SET uidvalidity = 0, low = 0, high = 0
             WHERE account = ?1 AND name = ?2",
            params![account, folder],
        )?;
        self.scrub();
        Ok(())
    }

    // Messages.

    /// A folder's messages, newest first.
    pub fn messages(&self, account: &str, folder: &str, limit: usize) -> Result<Vec<Message>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE account = ?1 AND folder = ?2
             ORDER BY date DESC, uid DESC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![account, folder, limit as i64], message_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn message(&self, account: &str, folder: &str, uid: u32) -> Result<Option<Message>> {
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages
                     WHERE account = ?1 AND folder = ?2 AND uid = ?3"
                ),
                params![account, folder, uid],
                message_row,
            )
            .optional()?)
    }

    /// The account's messages whose sender, recipients, subject or text
    /// contain every word of `query` (and that have the tag of every
    /// `tag:` word), newest first; in `folder` only if given.
    pub fn search(
        &self,
        account: &str,
        folder: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let mut tagged = Vec::new();
        let mut words = Vec::new();
        for w in query.split_whitespace().take(8) {
            if let Some(tag) = tags::searched(w) {
                tagged.push(tag);
                continue;
            }
            let escaped = w
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            words.push(format!("%{escaped}%"));
        }
        if words.is_empty() && tagged.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE account = ?1");
        let mut values: Vec<rusqlite::types::Value> = vec![account.to_string().into()];
        if let Some(folder) = folder {
            values.push(folder.to_string().into());
            sql.push_str(&format!(" AND folder = ?{}", values.len()));
        }
        for tag in tagged {
            values.push(tag.into());
            let n = values.len();
            sql.push_str(&format!(
                " AND instr(' ' || tags || ' ', ' ' || ?{n} || ' ') > 0"
            ));
        }
        for word in words {
            values.push(word.into());
            let n = values.len();
            sql.push_str(&format!(
                " AND (subject LIKE ?{n} ESCAPE '\\' OR from_name LIKE ?{n} ESCAPE '\\'
                   OR from_addr LIKE ?{n} ESCAPE '\\' OR to_list LIKE ?{n} ESCAPE '\\'
                   OR cc_list LIKE ?{n} ESCAPE '\\' OR preview LIKE ?{n} ESCAPE '\\')"
            ));
        }
        sql.push_str(&format!(" ORDER BY date DESC, uid DESC LIMIT {limit}"));
        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values), message_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds fetched headers, or updates the ones already there (keeping
    /// what was learned from the message's text).
    pub fn add_headers(&mut self, account: &str, folder: &str, headers: &[Header]) -> Result<()> {
        let tx = self.db.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO messages (account, folder, uid, message_id, in_reply_to, refs, date,
                     from_name, from_addr, to_list, cc_list, subject, attachments, seen, flagged,
                     answered, draft, size, tags)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19)
                 ON CONFLICT(account, folder, uid) DO UPDATE SET
                     seen = excluded.seen, flagged = excluded.flagged,
                     answered = excluded.answered, draft = excluded.draft,
                     tags = excluded.tags",
            )?;
            for h in headers {
                let s = &h.summary;
                stmt.execute(params![
                    account,
                    folder,
                    h.uid,
                    s.message_id,
                    s.in_reply_to,
                    s.references,
                    s.date,
                    s.from.name,
                    s.from.addr,
                    s.to,
                    s.cc,
                    s.subject,
                    s.attachments,
                    h.flags.seen,
                    h.flags.flagged,
                    h.flags.answered,
                    h.flags.draft,
                    h.size,
                    tags::to_stored(&h.flags.tags)
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Sets the flags of messages already cached.
    pub fn set_flags(&mut self, account: &str, folder: &str, flags: &[(u32, Flags)]) -> Result<()> {
        let tx = self.db.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE messages SET seen = ?4, flagged = ?5, answered = ?6, draft = ?7,
                     tags = ?8
                 WHERE account = ?1 AND folder = ?2 AND uid = ?3",
            )?;
            for (uid, f) in flags {
                stmt.execute(params![
                    account,
                    folder,
                    uid,
                    f.seen,
                    f.flagged,
                    f.answered,
                    f.draft,
                    tags::to_stored(&f.tags)
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every tag on a cached message, of any account, sorted: the ones to
    /// offer when tagging.
    pub fn all_tags(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .db
            .prepare("SELECT DISTINCT tags FROM messages WHERE tags != '' LIMIT 5000")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut all = std::collections::BTreeSet::new();
        for row in rows {
            all.extend(tags::from_stored(&row?));
        }
        Ok(all.into_iter().take(500).collect())
    }

    /// The UIDs cached in `folder` from `low` to `high`.
    pub fn uids(&self, account: &str, folder: &str, low: u32, high: u32) -> Result<Vec<u32>> {
        let mut stmt = self.db.prepare(
            "SELECT uid FROM messages WHERE account = ?1 AND folder = ?2 AND uid BETWEEN ?3 AND ?4",
        )?;
        let rows = stmt.query_map(params![account, folder, low, high], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove(&mut self, account: &str, folder: &str, uids: &[u32]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let tx = self.db.transaction()?;
        for table in ["messages", "bodies"] {
            let mut stmt = tx.prepare(&format!(
                "DELETE FROM {table} WHERE account = ?1 AND folder = ?2 AND uid = ?3"
            ))?;
            for uid in uids {
                stmt.execute(params![account, folder, uid])?;
            }
        }
        tx.commit()?;
        self.scrub();
        Ok(())
    }

    pub fn body(&self, account: &str, folder: &str, uid: u32) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row(
                "SELECT raw FROM bodies WHERE account = ?1 AND folder = ?2 AND uid = ?3",
                params![account, folder, uid],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Keeps a downloaded message, and the start of its text for the list.
    /// Keeps a downloaded message, unless it's bigger than [`MAX_KEPT_BODY`]
    /// (then it's downloaded again to read). Its preview for the list comes
    /// later ([`Store::set_preview`]): making one may lay out HTML, which
    /// needs the big stack of a background thread.
    pub fn save_body(&self, account: &str, folder: &str, uid: u32, raw: &[u8]) -> Result<()> {
        if raw.len() <= MAX_KEPT_BODY {
            self.keep_body(account, folder, uid, raw)?;
        }
        Ok(())
    }

    fn keep_body(&self, account: &str, folder: &str, uid: u32, raw: &[u8]) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO bodies (account, folder, uid, raw) VALUES (?1, ?2, ?3, ?4)",
            params![account, folder, uid, raw],
        )?;
        Ok(())
    }

    /// The start of a message's text, for the list.
    pub fn set_preview(&self, account: &str, folder: &str, uid: u32, preview: &str) -> Result<()> {
        self.db.execute(
            "UPDATE messages SET preview = ?4 WHERE account = ?1 AND folder = ?2 AND uid = ?3",
            params![account, folder, uid, text::one_line(preview, 200)],
        )?;
        Ok(())
    }

    // Folders kept here: POP3 mail, and its Sent, Drafts and Trash.

    /// Adds a whole message to a folder kept here, under the next UID;
    /// returns that UID.
    pub fn add_local(
        &mut self,
        account: &str,
        folder: &str,
        raw: &[u8],
        flags: Flags,
        pop_id: Option<&str>,
    ) -> Result<u32> {
        // Seen first: a message that can't be kept is never downloaded
        // again and again.
        if let Some(id) = pop_id {
            self.pop_skip(account, id)?;
        }
        let uid: u32 = self.db.query_row(
            "SELECT COALESCE(MAX(uid), 0) + 1 FROM messages WHERE account = ?1 AND folder = ?2",
            params![account, folder],
            |r| r.get(0),
        )?;
        let header = Header {
            uid,
            summary: mail::summarize(raw),
            flags,
            size: raw.len() as u32,
        };
        self.add_headers(account, folder, &[header])?;
        self.db.execute(
            "UPDATE messages SET pop_id = ?4 WHERE account = ?1 AND folder = ?2 AND uid = ?3",
            params![account, folder, uid, pop_id],
        )?;
        // The only copy: kept whatever its size.
        self.keep_body(account, folder, uid, raw)?;
        Ok(uid)
    }

    /// Moves messages between folders kept here; their new UIDs.
    pub fn move_local(
        &mut self,
        account: &str,
        from: &str,
        uids: &[u32],
        to: &str,
    ) -> Result<Vec<u32>> {
        let mut moved = Vec::new();
        for &uid in uids {
            let Some(raw) = self.body(account, from, uid)? else {
                continue;
            };
            let Some(message) = self.message(account, from, uid)? else {
                continue;
            };
            let pop_id: Option<String> = self.db.query_row(
                "SELECT pop_id FROM messages WHERE account = ?1 AND folder = ?2 AND uid = ?3",
                params![account, from, uid],
                |r| r.get(0),
            )?;
            let new = self.add_local(account, to, &raw, message.flags, None)?;
            self.db.execute(
                "UPDATE messages SET pop_id = ?4 WHERE account = ?1 AND folder = ?2 AND uid = ?3",
                params![account, to, new, pop_id],
            )?;
            moved.push((uid, new));
        }
        // Only what was copied goes.
        let gone: Vec<u32> = moved.iter().map(|(old, _)| *old).collect();
        self.remove(account, from, &gone)?;
        Ok(moved.into_iter().map(|(_, new)| new).collect())
    }

    /// The POP3 ids of every message ever downloaded for the account, so
    /// none is downloaded twice.
    pub fn pop_seen(&self, account: &str) -> Result<HashSet<String>> {
        let mut stmt = self
            .db
            .prepare("SELECT pop_id FROM pop_seen WHERE account = ?1")?;
        let rows = stmt.query_map(params![account], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Notes deletes for good on their way to the server.
    pub fn add_pending_deletes(
        &mut self,
        account: &str,
        folder: &str,
        uidvalidity: u32,
        uids: &[u32],
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO pending_deletes (account, folder, uidvalidity, uid)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for uid in uids {
                stmt.execute(params![account, folder, uidvalidity, uid])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes the server has made.
    pub fn done_deletes(&mut self, account: &str, folder: &str, uids: &[u32]) -> Result<()> {
        let tx = self.db.transaction()?;
        {
            let mut stmt = tx.prepare(
                "DELETE FROM pending_deletes WHERE account = ?1 AND folder = ?2 AND uid = ?3",
            )?;
            for uid in uids {
                stmt.execute(params![account, folder, uid])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The deletes still to make, by folder: (folder, UIDVALIDITY, UIDs).
    pub fn pending_deletes(&self, account: &str) -> Result<Vec<(String, u32, Vec<u32>)>> {
        let mut stmt = self.db.prepare(
            "SELECT folder, uidvalidity, uid FROM pending_deletes WHERE account = ?1
             ORDER BY folder, uidvalidity, uid",
        )?;
        let rows = stmt.query_map(params![account], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, u32>(2)?,
            ))
        })?;
        let mut out: Vec<(String, u32, Vec<u32>)> = Vec::new();
        for row in rows {
            let (folder, uidvalidity, uid) = row?;
            match out.last_mut() {
                Some(last) if last.0 == folder && last.1 == uidvalidity => last.2.push(uid),
                _ => out.push((folder, uidvalidity, vec![uid])),
            }
        }
        Ok(out)
    }

    /// Remembers a POP3 id that wasn't downloaded, so it isn't asked about
    /// again.
    pub fn pop_skip(&self, account: &str, pop_id: &str) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO pop_seen (account, pop_id) VALUES (?1, ?2)",
            params![account, pop_id],
        )?;
        Ok(())
    }

    /// [`Store::pop_skip`] for many ids, in one write: a server listing
    /// thousands costs one commit, not one each.
    pub fn pop_skip_all(&mut self, account: &str, pop_ids: &[String]) -> Result<()> {
        let tx = self.db.transaction()?;
        {
            let mut stmt =
                tx.prepare("INSERT OR IGNORE INTO pop_seen (account, pop_id) VALUES (?1, ?2)")?;
            for id in pop_ids {
                stmt.execute(params![account, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The POP3 ids of these messages, marked to be deleted from the server
    /// at the next check.
    pub fn mark_pop_removal(&self, account: &str, folder: &str, uids: &[u32]) -> Result<()> {
        for uid in uids {
            self.db.execute(
                "UPDATE pop_seen SET remove = 1 WHERE account = ?1 AND pop_id =
                     (SELECT pop_id FROM messages WHERE account = ?1 AND folder = ?2 AND uid = ?3)",
                params![account, folder, uid],
            )?;
        }
        Ok(())
    }

    /// The POP3 ids waiting to be deleted from the server.
    pub fn pop_removals(&self, account: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .db
            .prepare("SELECT pop_id FROM pop_seen WHERE account = ?1 AND remove = 1")?;
        let rows = stmt.query_map(params![account], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forgets POP3 ids the server no longer has: they can't come back.
    pub fn pop_forget(&mut self, account: &str, keep: &HashSet<String>) -> Result<()> {
        let gone: Vec<String> = self
            .pop_seen(account)?
            .into_iter()
            .filter(|id| !keep.contains(id))
            .collect();
        let tx = self.db.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM pop_seen WHERE account = ?1 AND pop_id = ?2")?;
            for id in &gone {
                stmt.execute(params![account, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // Addresses, for completing To, Cc and Bcc.

    /// Remembers addresses written to (`used`) or seen. A name seen in mail
    /// never replaces one already known: anyone can put any name on any
    /// address. Nor does a name sent to an address already written to: a
    /// reply-all carries the names its sender chose for everyone on it.
    pub fn remember(&self, addrs: &[Addr], used: bool, now: i64) -> Result<()> {
        for a in addrs.iter().take(100) {
            if !crate::account::valid_address(&a.addr) {
                continue;
            }
            self.db.execute(
                "INSERT INTO addresses (addr, name, used, last) VALUES (lower(?1), ?2, ?3, ?4)
                 ON CONFLICT(addr) DO UPDATE SET
                     name = CASE
                         WHEN excluded.name = '' THEN addresses.name
                         WHEN addresses.name = '' THEN excluded.name
                         WHEN excluded.used = 1 AND addresses.used = 0 THEN excluded.name
                         ELSE addresses.name END,
                     used = addresses.used + excluded.used, last = MAX(addresses.last, excluded.last)",
                params![a.addr, a.name, i64::from(used), now],
            )?;
        }
        Ok(())
    }

    /// Known addresses starting with `prefix` (or whose name has a word
    /// starting with it), the most written to first.
    pub fn complete(&self, prefix: &str, limit: usize) -> Result<Vec<Addr>> {
        let prefix = prefix.trim().to_lowercase();
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let mut stmt = self.db.prepare(
            "SELECT name, addr FROM addresses
             WHERE addr LIKE ?1 ESCAPE '\\' OR lower(name) LIKE ?1 ESCAPE '\\'
                OR lower(name) LIKE ?2 ESCAPE '\\'
             ORDER BY used DESC, last DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![format!("{escaped}%"), format!("% {escaped}%"), limit as i64],
            |r| {
                Ok(Addr {
                    name: text::one_line(&r.get::<_, String>(0)?, mail::MAX_NAME),
                    addr: text::one_line(&r.get::<_, String>(1)?, mail::MAX_NAME),
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forgets everything about an account: it was removed.
    pub fn forget_account(&self, account: &str) -> Result<()> {
        for table in [
            "folders",
            "messages",
            "bodies",
            "pop_seen",
            "pending_deletes",
        ] {
            self.db.execute(
                &format!("DELETE FROM {table} WHERE account = ?1"),
                params![account],
            )?;
        }
        self.scrub();
        Ok(())
    }
}

/// The start of a message's text for the list: one line, signature and
/// quotes left out.
pub fn preview(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line == "--" || line.starts_with("-- ") {
            break;
        }
        if line.is_empty() || line.starts_with('>') {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
        if out.chars().count() > 200 {
            break;
        }
    }
    text::one_line(&out, 200)
}

/// Folders in the order the sidebar shows them: the inbox, then folders
/// with a role in [`Role`]'s order, then the rest by name.
pub fn sort_folders(folders: &mut [Folder]) {
    folders.sort_by(|a, b| {
        let key = |f: &Folder| (f.role.map_or(u8::MAX, |r| r as u8), f.shown.to_lowercase());
        key(a).cmp(&key(b))
    });
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn header(uid: u32, subject: &str, date: i64) -> Header {
        Header {
            uid,
            summary: Summary {
                subject: subject.into(),
                date,
                from: Addr {
                    name: "Alice".into(),
                    addr: "alice@example.com".into(),
                },
                to: "me@example.org".into(),
                ..Summary::default()
            },
            flags: Flags::default(),
            size: 100,
        }
    }

    fn folder(name: &str, role: Option<Role>) -> Folder {
        Folder {
            name: name.into(),
            shown: name.into(),
            delimiter: Some("/".into()),
            role,
            selectable: true,
            unread: 0,
            total: 0,
        }
    }

    fn tagged(uid: u32, subject: &str, tags: &[&str]) -> Header {
        let mut h = header(uid, subject, i64::from(uid));
        h.flags.tags = tags.iter().map(|t| t.to_string()).collect();
        h
    }

    #[test]
    fn tags_are_kept_with_the_flags_and_found_by_tag_words() {
        let mut store = Store::in_memory();
        store
            .add_headers(
                "a",
                "INBOX",
                &[
                    tagged(1, "Invoice March", &["receipts", "work"]),
                    tagged(2, "Invoice April", &["receipts"]),
                    tagged(3, "Lunch", &[]),
                ],
            )
            .unwrap();
        let tags = |store: &Store, uid| {
            store
                .message("a", "INBOX", uid)
                .unwrap()
                .unwrap()
                .flags
                .tags
        };
        assert_eq!(tags(&store, 1), ["receipts", "work"]);
        let found = |store: &Store, q: &str| -> Vec<u32> {
            let found = store.search("a", Some("INBOX"), q, 50).unwrap();
            found.iter().map(|m| m.uid).collect()
        };
        assert_eq!(found(&store, "tag:receipts"), [2, 1]);
        assert_eq!(found(&store, "tag:receipts march"), [1]);
        assert_eq!(found(&store, "tag:work tag:receipts"), [1]);
        assert!(found(&store, "tag:rec").is_empty(), "whole tags only");
        // The next check's flags replace them.
        let flags = Flags {
            tags: vec!["later".into()],
            ..Flags::default()
        };
        store.set_flags("a", "INBOX", &[(1, flags)]).unwrap();
        assert_eq!(tags(&store, 1), ["later"]);
        store
            .add_headers("a", "INBOX", &[tagged(1, "Invoice March", &[])])
            .unwrap();
        assert!(tags(&store, 1).is_empty());
        assert_eq!(store.all_tags().unwrap(), ["receipts"]);
        // Read back checked: a cache changed by hand shows no bad tag.
        store
            .db
            .execute(
                "UPDATE messages SET tags = 'ok Bad \u{1b}x' WHERE uid = 3",
                [],
            )
            .unwrap();
        assert_eq!(tags(&store, 3), ["ok"]);
    }

    #[test]
    fn a_cache_made_before_tags_gets_them() {
        let db = Connection::open_in_memory().unwrap();
        let old = SCHEMA.replace("    tags TEXT NOT NULL DEFAULT '',\n", "");
        assert_ne!(old, SCHEMA);
        db.execute_batch(&old).unwrap();
        Store::migrate(&db).unwrap();
        Store::migrate(&db).unwrap();
        let mut store = Store { db };
        store
            .add_headers("a", "INBOX", &[tagged(1, "Hi", &["work"])])
            .unwrap();
        let m = store.message("a", "INBOX", 1).unwrap().unwrap();
        assert_eq!(m.flags.tags, ["work"]);
    }

    #[test]
    fn folders_are_kept_in_the_sidebars_order_and_gone_ones_removed() {
        let mut store = Store::in_memory();
        store
            .set_folders(
                "a",
                &[
                    folder("Work", None),
                    folder("Trash", Some(Role::Trash)),
                    folder("INBOX", Some(Role::Inbox)),
                    folder("Sent", Some(Role::Sent)),
                ],
            )
            .unwrap();
        store
            .add_headers("a", "Work", &[header(1, "x", 1)])
            .unwrap();
        let names: Vec<String> = store
            .folders("a")
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["INBOX", "Sent", "Trash", "Work"]);
        store
            .set_folders("a", &[folder("INBOX", Some(Role::Inbox))])
            .unwrap();
        assert_eq!(store.folders("a").unwrap().len(), 1);
        assert!(
            store.messages("a", "Work", 10).unwrap().is_empty(),
            "its mail went too"
        );
    }

    #[test]
    fn messages_come_newest_first_and_flags_update_in_place() {
        let mut store = Store::in_memory();
        store
            .add_headers(
                "a",
                "INBOX",
                &[header(1, "old", 100), header(2, "new", 200)],
            )
            .unwrap();
        let list = store.messages("a", "INBOX", 10).unwrap();
        assert_eq!(list[0].summary.subject, "new");
        store
            .set_flags(
                "a",
                "INBOX",
                &[(
                    1,
                    Flags {
                        seen: true,
                        flagged: true,
                        ..Flags::default()
                    },
                )],
            )
            .unwrap();
        let old = store.message("a", "INBOX", 1).unwrap().unwrap();
        assert!(old.flags.seen && old.flags.flagged);
        assert_eq!(store.count("a", "INBOX").unwrap(), (1, 2));
        store.remove("a", "INBOX", &[1]).unwrap();
        assert_eq!(store.uids("a", "INBOX", 0, 10).unwrap(), [2]);
    }

    #[test]
    fn bodies_give_a_preview_and_search_finds_words_anywhere() {
        let mut store = Store::in_memory();
        store
            .add_headers("a", "INBOX", &[header(5, "Quarterly report", 1)])
            .unwrap();
        let raw = b"From: alice@example.com\r\nSubject: Quarterly report\r\n\r\n\
Hi,\r\n\r\nthe numbers are in.\r\n> old quote\r\n-- \r\nAlice\r\n";
        store.save_body("a", "INBOX", 5, raw).unwrap();
        store
            .set_preview("a", "INBOX", 5, &mail::preview_of(raw))
            .unwrap();
        let m = store.message("a", "INBOX", 5).unwrap().unwrap();
        assert_eq!(m.preview, "Hi, the numbers are in.");
        assert_eq!(store.body("a", "INBOX", 5).unwrap().unwrap(), raw);
        let found = store.search("a", None, "quarterly numbers", 10).unwrap();
        assert_eq!(found.len(), 1);
        assert!(
            store
                .search("a", None, "quarterly missing", 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .search("a", Some("Sent"), "quarterly", 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            store.search("a", None, "%", 10).unwrap().is_empty(),
            "% is a letter"
        );
    }

    #[test]
    fn local_folders_number_their_mail_and_remember_pop_ids() {
        let mut store = Store::in_memory();
        let raw = b"From: bob@example.org\r\nSubject: hi\r\n\r\nyo\r\n";
        let one = store
            .add_local("p", "INBOX", raw, Flags::default(), Some("uidl-1"))
            .unwrap();
        let two = store
            .add_local("p", "INBOX", raw, Flags::default(), Some("uidl-2"))
            .unwrap();
        assert_eq!((one, two), (1, 2));
        assert_eq!(store.pop_seen("p").unwrap().len(), 2);
        let moved = store.move_local("p", "INBOX", &[1], "Trash").unwrap();
        assert_eq!(moved, [1]);
        store.mark_pop_removal("p", "Trash", &[1]).unwrap();
        assert_eq!(store.pop_removals("p").unwrap(), ["uidl-1"]);
        store
            .pop_forget("p", &HashSet::from(["uidl-2".to_string()]))
            .unwrap();
        assert_eq!(
            store.pop_seen("p").unwrap(),
            HashSet::from(["uidl-2".into()])
        );
    }

    #[test]
    fn addresses_complete_by_address_or_name() {
        let store = Store::in_memory();
        let alice = Addr {
            name: "Alice Rivera".into(),
            addr: "Alice@Example.com".into(),
        };
        let bob = Addr {
            name: "Bob".into(),
            addr: "bob@example.org".into(),
        };
        store
            .remember(&[alice.clone(), bob.clone()], false, 1)
            .unwrap();
        store.remember(&[alice], true, 2).unwrap();
        // A name seen in mail doesn't replace the one known.
        let spoofed = Addr {
            name: "evil@x.org, Bob".into(),
            addr: "bob@example.org".into(),
        };
        store.remember(&[spoofed], false, 3).unwrap();
        assert_eq!(store.complete("bob", 5).unwrap()[0].name, "Bob");
        // Nor a name sent on: Alice, written to, keeps hers when a reply-all
        // carries another sender's name for her.
        let renamed = Addr {
            name: "A. R. (old)".into(),
            addr: "alice@example.com".into(),
        };
        store.remember(&[renamed], true, 4).unwrap();
        assert_eq!(store.complete("riv", 5).unwrap()[0].name, "Alice Rivera");
        let found = store.complete("riv", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].addr, "alice@example.com");
        assert_eq!(store.complete("b", 5).unwrap()[0].addr, "bob@example.org");
        assert!(store.complete("", 5).unwrap().is_empty());
    }
}
