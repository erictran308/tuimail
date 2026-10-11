//! Notifications for new mail, sent to the terminal as escape codes: the
//! terminal shows them as the system's own notifications, which also works
//! over SSH. Which code depends on the terminal ([`detect`]). The same code
//! as tuigram's.
//!
//! Only unread mail that arrives in an inbox while tuimail runs is told
//! about; [`Notifier`] batches what arrives together.

use std::io::Write;
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::text;

/// How notifications reach the terminal, set in `settings.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Notifications {
    /// Picked from the terminal tuimail runs in.
    #[default]
    Auto,
    Off,
    /// The bell: most terminals bounce the dock icon or mark the tab.
    Bell,
    /// Text only: iTerm2, Ghostty, kitty, WezTerm, foot.
    Osc9,
    /// A title and text: Ghostty, WezTerm, Konsole, foot, kitty, Windows
    /// Terminal (once `compatibility.allowOSC777` is on).
    Osc777,
    /// A title and text, shown only while the window is in the background:
    /// kitty, foot, Konsole.
    Osc99,
}

impl Notifications {
    /// `Auto` turned into what the terminal understands.
    pub fn resolve(self, env: impl Fn(&str) -> Option<String>) -> Notifications {
        match self {
            Notifications::Auto => detect(env),
            other => other,
        }
    }
}

/// The best notification code for the terminal, from what it says about
/// itself in the environment. Inside tmux, `TERM_PROGRAM` is tmux, but the
/// other variables come from the terminal tmux was started in.
pub fn detect(env: impl Fn(&str) -> Option<String>) -> Notifications {
    let is = |name: &str, value: &str| env(name).is_some_and(|v| v == value);
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    let term = env("TERM").unwrap_or_default();
    if set("KITTY_WINDOW_ID")
        || term == "xterm-kitty"
        || term.starts_with("foot")
        || set("KONSOLE_VERSION")
    {
        Notifications::Osc99
    } else if is("TERM_PROGRAM", "ghostty")
        || set("GHOSTTY_RESOURCES_DIR")
        || is("TERM_PROGRAM", "WezTerm")
        || set("WEZTERM_PANE")
        || set("WT_SESSION")
    {
        Notifications::Osc777
    } else if is("TERM_PROGRAM", "iTerm.app") || is("LC_TERMINAL", "iTerm2") {
        Notifications::Osc9
    } else {
        Notifications::Bell
    }
}

/// One notification, ready to send.
#[derive(Debug, PartialEq)]
pub struct Alert {
    pub title: String,
    pub body: String,
}

/// Whether tuimail runs inside tmux, which needs codes passed through.
pub fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|v| !v.is_empty())
}

/// The escape code that shows `alert`, or `None` if nothing should go out.
/// `id` tells notifications apart, for terminals that track them. Inside
/// tmux, codes other than the bell must be passed through to the terminal
/// (which needs `set -g allow-passthrough on`).
pub fn escape(method: Notifications, alert: &Alert, id: u64, tmux: bool) -> Option<String> {
    // The codes end at a control character, and a `;` separates fields.
    let title = plain(&alert.title).replace(';', ",");
    let body = plain(&alert.body).replace(';', ",");
    let code = match method {
        Notifications::Auto | Notifications::Off => return None,
        // tmux rings the terminal's bell itself.
        Notifications::Bell => return Some("\x07".into()),
        Notifications::Osc9 => format!("\x1b]9;{title}: {body}\x07"),
        Notifications::Osc777 => format!("\x1b]777;notify;{title};{body}\x07"),
        Notifications::Osc99 => {
            let base64 = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
            format!(
                "\x1b]99;i={id}:d=0:o=unfocused:e=1;{}\x1b\\\
                 \x1b]99;i={id}:d=1:p=body:e=1;{}\x1b\\",
                base64(&title),
                base64(&body)
            )
        }
    };
    Some(if tmux {
        format!("\x1bPtmux;{}\x1b\\", code.replace('\x1b', "\x1b\x1b"))
    } else {
        code
    })
}

/// Sets the window title: "(3) tuimail" with three unread messages in the
/// inboxes.
pub fn title(unread: u32) -> String {
    if unread > 0 {
        format!("\x1b]2;({unread}) tuimail\x07")
    } else {
        "\x1b]2;tuimail\x07".into()
    }
}

/// Writes a code to the terminal, between frames.
pub fn send(code: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(code.as_bytes());
    let _ = out.flush();
}

/// Saves the window title, for [`RESTORE_TITLE`] at exit.
pub const SAVE_TITLE: &str = "\x1b[22;0t";
/// Puts back the title [`SAVE_TITLE`] saved. Terminals that can't clear it
/// instead, so it doesn't stay "tuimail".
pub const RESTORE_TITLE: &str = "\x1b]2;\x07\x1b[23;0t";

/// `text` on one line, without control characters.
fn plain(text: &str) -> String {
    text::clean(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// At most `max` characters of `text`, ending with `…` if cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Notifications go out at most this often; what arrives meanwhile waits
/// and goes out together.
const GAP: Duration = Duration::from_secs(3);
const MAX_TITLE: usize = 60;
const MAX_BODY: usize = 150;

/// A new message to tell the user about.
#[derive(Clone, Debug)]
pub struct Note {
    /// The account's name, when there's more than one.
    pub account: String,
    pub from: String,
    pub subject: String,
}

#[derive(Default)]
pub struct Notifier {
    pending: Vec<Note>,
    last_sent: Option<Instant>,
}

impl Notifier {
    pub fn add(&mut self, note: Note) {
        self.pending.push(note);
    }

    /// What to send now, if anything: everything waiting, as one.
    pub fn due(&mut self, now: Instant) -> Option<Alert> {
        if self.pending.is_empty() || self.last_sent.is_some_and(|sent| now < sent + GAP) {
            return None;
        }
        let notes = std::mem::take(&mut self.pending);
        self.last_sent = Some(now);
        Some(alert(&notes))
    }
}

/// One notification for all of `notes`, which isn't empty.
fn alert(notes: &[Note]) -> Alert {
    let (title, body) = match notes {
        [note] => {
            let title = if note.account.is_empty() {
                note.from.clone()
            } else {
                format!("{} · {}", note.from, note.account)
            };
            (title, note.subject.clone())
        }
        _ => {
            let mut senders: Vec<&str> = Vec::new();
            for note in notes {
                if !senders.contains(&note.from.as_str()) {
                    senders.push(&note.from);
                }
            }
            (format!("{} new messages", notes.len()), senders.join(", "))
        }
    };
    Alert {
        title: cut(&plain(&title), MAX_TITLE),
        body: cut(&plain(&body), MAX_BODY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn note(from: &str, subject: &str) -> Note {
        Note {
            account: String::new(),
            from: from.into(),
            subject: subject.into(),
        }
    }

    fn alert(title: &str, body: &str) -> Alert {
        Alert {
            title: title.into(),
            body: body.into(),
        }
    }

    #[test]
    fn the_terminal_is_recognized_from_its_environment() {
        let detect_with = |vars: &[(&str, &str)]| {
            let vars: HashMap<String, String> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            detect(|name| vars.get(name).cloned())
        };
        assert_eq!(
            detect_with(&[("TERM_PROGRAM", "ghostty")]),
            Notifications::Osc777
        );
        assert_eq!(
            detect_with(&[("TERM_PROGRAM", "tmux"), ("GHOSTTY_RESOURCES_DIR", "/x")]),
            Notifications::Osc777,
            "inside tmux"
        );
        assert_eq!(
            detect_with(&[("TERM", "xterm-kitty")]),
            Notifications::Osc99
        );
        assert_eq!(
            detect_with(&[("LC_TERMINAL", "iTerm2")]),
            Notifications::Osc9
        );
        assert_eq!(
            detect_with(&[("TERM_PROGRAM", "Apple_Terminal")]),
            Notifications::Bell
        );
        assert_eq!(
            Notifications::Off.resolve(|_| Some("ghostty".into())),
            Notifications::Off,
            "a setting wins"
        );
    }

    #[test]
    fn each_code_carries_the_title_and_text() {
        let hi = alert("Alice", "Lunch?");
        assert_eq!(
            escape(Notifications::Osc777, &hi, 1, false).unwrap(),
            "\x1b]777;notify;Alice;Lunch?\x07"
        );
        assert_eq!(
            escape(Notifications::Osc9, &hi, 1, false).unwrap(),
            "\x1b]9;Alice: Lunch?\x07"
        );
        assert_eq!(escape(Notifications::Bell, &hi, 1, false).unwrap(), "\x07");
        assert_eq!(escape(Notifications::Off, &hi, 1, false), None);
        assert!(
            escape(Notifications::Osc99, &hi, 7, false)
                .unwrap()
                .starts_with("\x1b]99;i=7:d=0")
        );
    }

    #[test]
    fn text_from_senders_cannot_end_the_code_early() {
        let sneaky = alert("Eve;x", "a\x07b\x1b]0;pwned\x1b\\c\nd;e");
        assert_eq!(
            escape(Notifications::Osc777, &sneaky, 1, false).unwrap(),
            "\x1b]777;notify;Eve,x;ab]0,pwned\\c d,e\x07"
        );
    }

    #[test]
    fn inside_tmux_codes_are_passed_through() {
        let hi = alert("Alice", "hi");
        assert_eq!(
            escape(Notifications::Osc777, &hi, 1, true).unwrap(),
            "\x1bPtmux;\x1b\x1b]777;notify;Alice;hi\x07\x1b\\"
        );
    }

    #[test]
    fn mail_arriving_together_makes_one_notification_spaced_out() {
        let mut notifier = Notifier::default();
        let now = Instant::now();
        notifier.add(note("Alice", "Lunch?"));
        assert_eq!(notifier.due(now).unwrap(), alert("Alice", "Lunch?"));
        assert_eq!(notifier.due(now), None, "nothing left");

        notifier.add(note("Bob", "one"));
        notifier.add(note("Carol", "two"));
        notifier.add(note("Bob", "three"));
        assert_eq!(notifier.due(now + Duration::from_secs(1)), None, "too soon");
        assert_eq!(
            notifier.due(now + GAP).unwrap(),
            alert("3 new messages", "Bob, Carol")
        );
        let work = Note {
            account: "Work".into(),
            ..note("Dana", "Q3")
        };
        notifier.add(work);
        assert_eq!(
            notifier.due(now + GAP + GAP).unwrap(),
            alert("Dana · Work", "Q3")
        );
    }
}
