//! The icons the UI draws. Plain Unicode symbols come from whatever
//! fallback fonts the system has, each at its own size; terminals with Nerd
//! Font icons built in (Ghostty, kitty, WezTerm) draw those at one size.

use serde::{Deserialize, Serialize};

/// Which icons to draw: `icons` in settings.toml, or `TM_ICONS`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconMode {
    /// Nerd Font icons in terminals known to have them built in.
    #[default]
    Auto,
    /// Nerd Font icons: the terminal has them built in, or uses a Nerd Font.
    Nerd,
    /// Plain Unicode symbols, which every font setup can show.
    Plain,
}

impl IconMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "nerd" => Self::Nerd,
            "plain" => Self::Plain,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Icons {
    pub logo: &'static str,
    pub account: &'static str,
    pub inbox: &'static str,
    pub sent: &'static str,
    pub drafts: &'static str,
    pub trash: &'static str,
    pub junk: &'static str,
    pub archive: &'static str,
    pub all: &'static str,
    pub folder: &'static str,
    pub flagged: &'static str,
    pub unread: &'static str,
    pub replied: &'static str,
    pub attachment: &'static str,
    pub search: &'static str,
    pub loading: &'static str,
    pub offline: &'static str,
    pub compose: &'static str,
}

/// Font Awesome's, as every Nerd Font has them.
pub const NERD: Icons = Icons {
    logo: "\u{f0e0}",
    account: "\u{f007}",
    inbox: "\u{f01c}",
    sent: "\u{f1d8}",
    drafts: "\u{f040}",
    trash: "\u{f1f8}",
    junk: "\u{f05e}",
    archive: "\u{f187}",
    all: "\u{f0e0}",
    folder: "\u{f07b}",
    flagged: "\u{f005}",
    unread: "\u{f111}",
    replied: "\u{f112}",
    attachment: "\u{f0c6}",
    search: "\u{f002}",
    loading: "\u{f110}",
    offline: "\u{f127}",
    compose: "\u{f040}",
};

pub const PLAIN: Icons = Icons {
    logo: "✉",
    account: "@",
    inbox: "↓",
    sent: "↑",
    drafts: "✎",
    trash: "✗",
    junk: "⊘",
    archive: "▣",
    all: "≡",
    folder: "▸",
    flagged: "★",
    unread: "●",
    replied: "↩",
    attachment: "+",
    search: "⌕",
    loading: "⋯",
    offline: "!",
    compose: "✎",
};

impl Icons {
    /// The icons for `mode`, looking at the environment for `Auto`.
    pub fn pick(mode: IconMode, env: impl Fn(&str) -> Option<String>) -> Self {
        match mode {
            IconMode::Nerd => NERD,
            IconMode::Plain => PLAIN,
            IconMode::Auto if nerd_terminal(&env) => NERD,
            IconMode::Auto => PLAIN,
        }
    }
}

/// The terminal drawing the window has Nerd Font icons built in. Ghostty
/// and WezTerm set variables their child processes keep, even inside tmux
/// or another multiplexer, which passes the characters on to them.
fn nerd_terminal(env: &impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| env(name).filter(|v| !v.is_empty());
    let program = set("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase();
    program == "ghostty"
        || program == "wezterm"
        || set("TERM").is_some_and(|t| t == "xterm-ghostty" || t == "xterm-kitty")
        || set("KITTY_WINDOW_ID").is_some()
        || set("GHOSTTY_RESOURCES_DIR").is_some()
        || set("WEZTERM_EXECUTABLE").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn ghostty_gets_nerd_icons_even_inside_a_multiplexer() {
        let inside = env(&[
            ("TERM_PROGRAM", "tmux"),
            ("GHOSTTY_RESOURCES_DIR", "/Applications/Ghostty.app"),
        ]);
        assert_eq!(Icons::pick(IconMode::Auto, inside), NERD);
        let apple = env(&[("TERM_PROGRAM", "Apple_Terminal")]);
        assert_eq!(Icons::pick(IconMode::Auto, apple), PLAIN);
        let apple = env(&[("TERM_PROGRAM", "Apple_Terminal")]);
        assert_eq!(Icons::pick(IconMode::Nerd, apple), NERD, "the settings win");
        assert_eq!(IconMode::parse("Plain"), Some(IconMode::Plain));
    }

    #[test]
    fn every_icon_takes_one_column() {
        for icons in [NERD, PLAIN] {
            for icon in [
                icons.logo,
                icons.account,
                icons.inbox,
                icons.sent,
                icons.drafts,
                icons.trash,
                icons.junk,
                icons.archive,
                icons.all,
                icons.folder,
                icons.flagged,
                icons.unread,
                icons.replied,
                icons.attachment,
                icons.search,
                icons.loading,
                icons.offline,
                icons.compose,
            ] {
                assert_eq!(icon.width(), 1, "{icon:?}");
            }
        }
    }
}
