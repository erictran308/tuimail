//! User settings, kept in `settings.toml` in the data directory. Accounts
//! are in `accounts.toml` beside it (`account.rs`).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::icons::IconMode;
use crate::notify::Notifications;
use crate::theme;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The theme in use: one of the built-in ones (picked in Settings: `O`).
    pub theme: String,
    /// Which icons: "auto" (Nerd Font icons in Ghostty, kitty and WezTerm,
    /// which have them built in), "nerd" or "plain". `TM_ICONS` overrides it.
    pub icons: IconMode,
    /// How new mail is announced: "auto" (what the terminal understands),
    /// "off", "bell", "osc9", "osc777" or "osc99".
    pub notifications: Notifications,
    /// Show the open message beside the list when the window is wide
    /// enough, instead of in its place.
    pub preview: bool,
    /// Show a message's plain-text part when it has one as well as HTML.
    /// Off: the HTML, turned into text (no pictures, nothing fetched).
    pub prefer_plain: bool,
    /// Mark a message read when it's opened.
    pub mark_read: bool,
    /// Ask before deleting mail.
    pub confirm_delete: bool,
    /// Ask before sending.
    pub confirm_send: bool,
    /// How often to look for new mail where the server can't say at once
    /// (no IDLE), and in folders other than the inbox, in minutes: at least
    /// 1, at most a day.
    pub check_minutes: u32,
    /// How many of each folder's newest messages are fetched at first;
    /// older ones as you scroll to them.
    pub fetch_count: u32,
    /// The sidebar's width, in columns.
    pub sidebar_width: u16,
    /// Where attachments are saved. Default: your Downloads folder.
    pub downloads: Option<PathBuf>,
    /// The editor Ctrl-e in the composer opens, with its arguments, the
    /// program as a full path: `["/usr/bin/vim"]`. Default: `$VISUAL` or
    /// `$EDITOR` found on `PATH`, else vi.
    pub editor: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: theme::DEFAULT.into(),
            icons: IconMode::Auto,
            notifications: Notifications::Auto,
            preview: true,
            prefer_plain: false,
            mark_read: true,
            confirm_delete: false,
            confirm_send: true,
            check_minutes: 5,
            fetch_count: 300,
            sidebar_width: 28,
            downloads: None,
            editor: Vec::new(),
        }
    }
}

impl Settings {
    /// Defaults if the file doesn't exist yet; an error if it can't be read.
    #[cfg(test)]
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self::load_checked(path)?.0)
    }

    /// The settings, and the names in the file tuimail doesn't know: a typo
    /// (`mark_raed = false`) would otherwise be ignored without a word.
    pub fn load_checked(path: &Path) -> Result<(Self, Vec<String>)> {
        let text = match config::read_small(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), Vec::new()));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read {}", config::shown(path)));
            }
        };
        let shown = config::shown(path);
        let table: toml::Table =
            toml::from_str(&text).with_context(|| format!("{shown} is invalid"))?;
        let unknown: Vec<String> = table
            .keys()
            .filter(|key| !KNOWN.contains(&key.as_str()))
            .map(|key| crate::text::one_line(key, 40))
            .collect();
        let settings: Self =
            toml::from_str(&text).with_context(|| format!("{shown} is invalid"))?;
        Ok((settings, unknown))
    }

    /// How often to look for new mail, in seconds.
    pub fn check_every(&self) -> u64 {
        u64::from(self.check_minutes.clamp(1, 24 * 60)) * 60
    }

    /// How many messages a folder starts with.
    pub fn fetch_count(&self) -> u32 {
        self.fetch_count.clamp(20, 5000)
    }

    pub fn sidebar_width(&self) -> u16 {
        self.sidebar_width.clamp(16, 50)
    }

    /// The icon mode in use: `TM_ICONS` if it names one, else the file's.
    pub fn icon_mode(&self) -> IconMode {
        config::var("TM_ICONS")
            .and_then(|v| IconMode::parse(&v))
            .unwrap_or(self.icons)
    }

    /// Where attachments go: the setting, else the Downloads folder, else
    /// home.
    pub fn downloads(&self) -> Option<PathBuf> {
        self.downloads
            .clone()
            .filter(|p| p.is_absolute())
            .or_else(dirs::download_dir)
            .or_else(dirs::home_dir)
    }

    /// Sets one setting in settings.toml, as the file is now: whatever was
    /// written there since tuimail started (by hand, or by another tuimail)
    /// stays. A file that no longer reads is left alone. Returns the
    /// settings the file now holds.
    pub fn save_one(path: &Path, key: &str, value: toml::Value) -> Result<Self> {
        let shown = config::shown(path);
        let mut table: toml::Table = match config::read_small(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("{shown} is invalid, so it wasn't changed"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(e) => return Err(e).with_context(|| format!("cannot read {shown}")),
        };
        table.insert(key.to_string(), value);
        let text = toml::to_string(&table)?;
        let settings: Self = toml::from_str(&text)?;
        write_private(path, &text).with_context(|| format!("cannot write {shown}"))?;
        Ok(settings)
    }
}

/// Writes `text` to `path` readable only by the user, and all at once: a new
/// file of this process's own (two tuimails saving at once can't remove or
/// publish each other's) renamed over the old one.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let new = path.with_extension(format!("{extension}.{}.new", std::process::id()));
    let write = || -> std::io::Result<()> {
        let _ = std::fs::remove_file(&new);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&new)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&new, path)
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&new);
    }
    result
}

/// The settings tuimail reads.
const KNOWN: [&str; 13] = [
    "theme",
    "icons",
    "notifications",
    "preview",
    "prefer_plain",
    "mark_read",
    "confirm_delete",
    "confirm_send",
    "check_minutes",
    "fetch_count",
    "sidebar_width",
    "downloads",
    "editor",
];

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_one_setting_keeps_what_the_file_gained_since_start() {
        let dir = std::env::temp_dir().join(format!("tuimail-save-one-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = path(&dir);
        std::fs::write(&file, "mark_read = false\nmark_raed = 1\n").unwrap();
        let saved = Settings::save_one(&file, "prefer_plain", true.into()).unwrap();
        assert!(!saved.mark_read && saved.prefer_plain);
        let (loaded, unknown) = Settings::load_checked(&file).unwrap();
        assert_eq!(loaded, saved);
        assert_eq!(
            unknown,
            ["mark_raed"],
            "an unknown name stays to be reported"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "only yours");
        }
        // A file that doesn't read is left as it is.
        std::fs::write(&file, "mark_read = \"no\"").unwrap();
        assert!(Settings::save_one(&file, "theme", "latte".into()).is_err());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "mark_read = \"no\""
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_means_defaults_and_odd_numbers_are_kept_in_range() {
        let dir = std::env::temp_dir().join(format!("tuimail-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = path(&dir);
        let _ = std::fs::remove_file(&file);
        assert_eq!(Settings::load(&file).unwrap(), Settings::default());
        std::fs::write(
            &file,
            "check_minutes = 0\nfetch_count = 1\nsidebar_width = 999",
        )
        .unwrap();
        let settings = Settings::load(&file).unwrap();
        assert_eq!(settings.check_every(), 60);
        assert_eq!(settings.fetch_count(), 20);
        assert_eq!(settings.sidebar_width(), 50);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
