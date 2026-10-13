//! AI agents' way to the user's mail. `tuimail mcp` (`mcp.rs`), the MCP
//! server an agent starts, asks the running tuimail over a socket in the
//! data folder, and tuimail answers (`App::on_agent`): it reads from its
//! cache, and changes mail as the user's own keys do, on the screen, kept
//! as changes that can be undone (`z`, `Z`), and asking the user first
//! unless the settings say not to. Nothing is ever deleted for good and
//! nothing is sent: there's no call for it.
//!
//! tuimail listens only while the "AI agents" setting is on. The socket is
//! in the data folder (only the user's), is made only the user's, and a
//! connection is answered only if it comes from a program of the same
//! user. A request is one line of JSON, at most 64 KB, and so is its
//! answer (at most 4 MB).

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::text;

/// The socket's name in the data folder; the demo's (`tuimail --demo`,
/// reached with `tuimail mcp --demo`), so agents can be tried on made-up
/// mail.
pub const SOCKET: &str = "agent.sock";
pub const DEMO_SOCKET: &str = "demo-agent.sock";
/// The most messages one call names.
pub const MAX_IDS: usize = 50;
/// The most messages a search returns.
pub const MAX_RESULTS: usize = 50;
/// How much of a message's text is returned, unless asked for less.
pub const TEXT_CHARS: usize = 8_000;
/// The most of a message's text that's returned.
pub const MAX_TEXT_CHARS: usize = 20_000;
/// The most messages agents change in an hour, and move to Trash.
pub const CHANGES_PER_HOUR: usize = 300;
pub const TRASH_PER_HOUR: usize = 100;
/// How long a change waits for the user's `y`.
pub const ASK_FOR: Duration = Duration::from_secs(5 * 60);

/// What `tuimail mcp` asks, for the program named `client` (as it calls
/// itself: shown to the user, never trusted).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub client: String,
    pub call: Call,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum Call {
    ListAccounts,
    ListFolders(FolderArgs),
    SearchMessages(SearchArgs),
    ReadMessage(ReadArgs),
    MarkRead(ReadFlagArgs),
    Star(StarArgs),
    SetTag(TagArgs),
    MoveMessages(MoveArgs),
    MarkSpam(SpamArgs),
    MoveToTrash(IdsArgs),
    ListChanges(ChangesArgs),
    UndoChange(UndoArgs),
}

impl Call {
    /// Whether it changes mail.
    pub fn changes(&self) -> bool {
        matches!(
            self,
            Call::MarkRead(_)
                | Call::Star(_)
                | Call::SetTag(_)
                | Call::MoveMessages(_)
                | Call::MarkSpam(_)
                | Call::MoveToTrash(_)
                | Call::UndoChange(_)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FolderArgs {
    /// The account, by its name or address (from list_accounts).
    pub account: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// Only this account (its name or address). Default: every account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Only this folder: its `folder` from list_folders, or a role (inbox,
    /// archive, sent, drafts, junk, trash). Default: with words, every
    /// folder; without, the inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Words that must all be in the sender, recipients, subject or the
    /// start of the text; `tag:work` asks for a tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Only unread messages.
    #[serde(default)]
    pub unread_only: bool,
    /// Only starred messages.
    #[serde(default)]
    pub starred_only: bool,
    /// At most this many, newest first (up to 50; default 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// The message's id, from search_messages.
    pub id: String,
    /// At most this many characters of its text (default 8000, up to
    /// 20000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFlagArgs {
    /// Message ids from search_messages (at most 50).
    pub ids: Vec<String>,
    /// true: mark read; false: mark unread.
    pub read: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StarArgs {
    /// Message ids from search_messages (at most 50).
    pub ids: Vec<String>,
    /// true: star; false: take the star off.
    pub starred: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TagArgs {
    /// Message ids from search_messages (at most 50).
    pub ids: Vec<String>,
    /// One word: letters, digits, - _ or . ("receipts", "travel").
    pub tag: String,
    /// true: put the tag on; false: take it off.
    pub on: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveArgs {
    /// Message ids from search_messages (at most 50).
    pub ids: Vec<String>,
    /// Where to: a `folder` from list_folders, or a role (inbox, archive).
    /// Trash is move_to_trash's, junk mark_spam's.
    pub folder: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpamArgs {
    /// Message ids from search_messages (at most 50).
    pub ids: Vec<String>,
    /// true: move to Junk; false: back from Junk to the inbox.
    pub spam: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IdsArgs {
    /// Message ids from search_messages (at most 50).
    pub ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangesArgs {
    /// At most this many, newest first (default 20, up to 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UndoArgs {
    /// The change's id, from a change's answer or list_changes.
    pub change: i64,
}

/// What tuimail answers: what was asked for, or why not.
pub type Answer = Result<serde_json::Value, String>;

/// What an agent is called on the screen: as it calls itself, cleaned.
pub fn shown_client(client: &str) -> String {
    let name = text::one_line(client, 40);
    if name.is_empty() {
        "An AI agent".into()
    } else {
        name
    }
}

/// A message's id for agents: its account, folder, UIDVALIDITY and UID,
/// so it means that message and no other (read back with [`unhandle`]).
pub fn handle(account: &str, folder: &str, uidvalidity: u32, uid: u32) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!("{account}\n{folder}\n{uidvalidity}\n{uid}"))
}

/// What a [`handle`] names: account id, folder, UIDVALIDITY, UID.
pub fn unhandle(id: &str) -> Option<(String, String, u32, u32)> {
    if id.len() > 2000 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(id.trim())
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let mut parts = text.split('\n');
    let (account, folder) = (parts.next()?, parts.next()?);
    let uidvalidity = parts.next()?.parse().ok()?;
    let uid = parts.next()?.parse().ok()?;
    if parts.next().is_some() || account.is_empty() || folder.is_empty() {
        return None;
    }
    Some((account.into(), folder.into(), uidvalidity, uid))
}

/// A message's text for an agent: links shown by their site only, so a
/// sender's link isn't one to follow (the user opens links, with `o`).
pub fn hide_links(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some((start, end)) = crate::mail::find_url(rest) {
        out.push_str(&rest[..start]);
        let host = crate::app::link_host(&rest[start..end]);
        out.push_str(&format!("[link: {host}]"));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Where the socket is.
pub fn socket(data_dir: &Path, demo: bool) -> PathBuf {
    data_dir.join(if demo { DEMO_SOCKET } else { SOCKET })
}

#[cfg(not(unix))]
pub use other::{Listener, ask, listen};
#[cfg(all(unix, test))]
pub use unix::NOT_LISTENING;
#[cfg(unix)]
pub use unix::{Listener, ask, listen};

/// Windows: not yet (a named pipe, made the user's alone, would do).
#[cfg(not(unix))]
mod other {
    use std::path::Path;

    use anyhow::{Result, bail};
    use tokio::sync::mpsc::UnboundedSender;

    use super::*;
    use crate::app::AppEvent;

    pub struct Listener;

    pub fn listen(_: &Path, _: UnboundedSender<AppEvent>) -> Result<Listener> {
        bail!("AI agents can't reach tuimail on Windows yet")
    }

    pub async fn ask(_: &Path, _: &Request) -> Answer {
        Err("tuimail mcp isn't available on Windows yet".into())
    }
}

#[cfg(unix)]
mod unix {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use anyhow::{Context, Result, bail};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::{Semaphore, mpsc::UnboundedSender, oneshot};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::app::AppEvent;

    /// The longest request read, and answer.
    pub const MAX_REQUEST: usize = 64 << 10;
    pub const MAX_ANSWER: usize = 4 << 20;
    /// How long `tuimail mcp` waits for an answer: a little longer than a
    /// change waits for the user.
    const ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(5 * 60 + 30);

    /// Why `tuimail mcp` gets no answer.
    pub const NOT_LISTENING: &str = "tuimail isn't open, or its AI agents setting is off. \
        Ask the user to open tuimail and turn on AI agents in its settings (O).";

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    enum Wire {
        Ok(serde_json::Value),
        Error(String),
    }

    /// tuimail listening for agents; dropping it stops, and removes the
    /// socket.
    pub struct Listener {
        path: PathBuf,
        task: JoinHandle<()>,
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Listens at `path`; each request reaches the app as
    /// [`AppEvent::Agent`].
    pub fn listen(path: &Path, tx: UnboundedSender<AppEvent>) -> Result<Listener> {
        // A socket left by a tuimail that's gone is removed; one another
        // tuimail answers on is left to it.
        if std::fs::symlink_metadata(path).is_ok() {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                bail!("another tuimail is answering agents");
            }
            std::fs::remove_file(path).context("cannot remove the old agent socket")?;
        }
        // Socket paths are short: 104 bytes on macOS, 108 on Linux.
        if path.as_os_str().len() >= 100 {
            bail!("the data folder's path is too long for agents to reach tuimail through it");
        }
        let listener = UnixListener::bind(path).context("cannot listen for agents")?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        // A few at a time: one agent's calls come one by one.
        let slots = Arc::new(Semaphore::new(8));
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                // SAFETY: geteuid has no preconditions.
                let me = unsafe { libc::geteuid() };
                if !stream.peer_cred().is_ok_and(|c| c.uid() == me) {
                    continue;
                }
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, tx).await;
                    drop(slot);
                });
            }
        });
        Ok(Listener {
            path: path.to_path_buf(),
            task,
        })
    }

    /// Answers one request.
    async fn serve(stream: UnixStream, tx: UnboundedSender<AppEvent>) -> Result<()> {
        let (read, mut write) = stream.into_split();
        let mut line = Vec::new();
        let mut reader = BufReader::new(read.take(MAX_REQUEST as u64));
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            reader.read_until(b'\n', &mut line),
        )
        .await??;
        let answer = match serde_json::from_slice::<Request>(&line) {
            Ok(request) => {
                let (reply, answer) = oneshot::channel();
                tx.send(AppEvent::Agent { request, reply })?;
                match tokio::time::timeout(ASK_FOR + std::time::Duration::from_secs(15), answer)
                    .await
                {
                    Ok(Ok(answer)) => answer,
                    _ => Err("tuimail didn't answer in time".into()),
                }
            }
            Err(_) => Err("that isn't a request tuimail understands".into()),
        };
        let wire = match answer {
            Ok(value) => Wire::Ok(value),
            Err(error) => Wire::Error(error),
        };
        let mut out = serde_json::to_vec(&wire)?;
        out.truncate(MAX_ANSWER);
        out.push(b'\n');
        write.write_all(&out).await?;
        Ok(())
    }

    /// Asks the tuimail listening at `path` (`tuimail mcp`'s side).
    pub async fn ask(path: &Path, request: &Request) -> Answer {
        let mut stream = UnixStream::connect(path)
            .await
            .map_err(|_| NOT_LISTENING.to_string())?;
        let mut out = serde_json::to_vec(request).map_err(|e| e.to_string())?;
        if out.len() >= MAX_REQUEST {
            return Err("the request is too long".into());
        }
        out.push(b'\n');
        stream
            .write_all(&out)
            .await
            .map_err(|_| NOT_LISTENING.to_string())?;
        let mut line = Vec::new();
        let mut reader = BufReader::new(stream.take(MAX_ANSWER as u64 + 2));
        tokio::time::timeout(ANSWER_WITHIN, reader.read_until(b'\n', &mut line))
            .await
            .map_err(|_| "tuimail didn't answer in time".to_string())?
            .map_err(|e| e.to_string())?;
        match serde_json::from_slice::<Wire>(&line) {
            Ok(Wire::Ok(value)) => Ok(value),
            Ok(Wire::Error(error)) => Err(error),
            Err(_) => Err("tuimail's answer didn't read".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_names_one_message_and_nothing_else_reads_as_one() {
        let id = handle("me@example.org", "Archive/2026", 7, 42);
        assert_eq!(
            unhandle(&id),
            Some(("me@example.org".into(), "Archive/2026".into(), 7, 42))
        );
        for bad in ["", "x", &"A".repeat(3000), "bWVAZXhhbXBsZS5vcmcKSU5CT1gKNw"] {
            assert_eq!(unhandle(bad), None, "{bad}");
        }
    }

    #[test]
    fn links_show_only_their_site() {
        assert_eq!(
            hide_links("Reset it: https://evil.example/r?t=1 or mailto:x@y.z now"),
            "Reset it: [link: evil.example] or [link: y.z] now"
        );
        assert_eq!(hide_links("no links"), "no links");
    }

    #[test]
    fn calls_read_as_json_and_nothing_unknown_does() {
        let request: Request = serde_json::from_str(
            r#"{"client":"Claude Code","call":{"tool":"move_to_trash","ids":["a"]}}"#,
        )
        .unwrap();
        assert_eq!(
            request.call,
            Call::MoveToTrash(IdsArgs {
                ids: vec!["a".into()]
            })
        );
        assert!(request.call.changes());
        for bad in [
            r#"{"client":"x","call":{"tool":"delete","ids":["a"]}}"#,
            r#"{"client":"x","call":{"tool":"move_to_trash","ids":["a"],"forever":true}}"#,
            r#"{"client":"x","call":{"tool":"list_accounts"},"admin":true}"#,
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_nameless_agent_is_still_named() {
        assert_eq!(shown_client(""), "An AI agent");
        assert_eq!(shown_client("Claude\u{1b}[31m Code"), "Claude[31m Code");
    }
}
