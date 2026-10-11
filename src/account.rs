//! Mail accounts, kept in `accounts.toml` in the data folder: one
//! `[[account]]` table each, written by the setup screen (`+`) or by hand.
//! Passwords are never in it: they're in the system's keychain, asked for
//! at start, or the output of a command of yours (`password`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{config, settings, text};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// Mail stays on the server, in folders, read and changed there.
    #[default]
    Imap,
    /// The inbox is downloaded; folders are on this computer only.
    Pop3,
}

/// How a connection is protected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// TLS from the start (ports 993, 995, 465).
    #[default]
    Tls,
    /// Plain at first, then TLS before anything is sent (ports 143, 110,
    /// 587). If the server won't, nothing is sent.
    Starttls,
    /// No encryption: only to a server on this computer, like a bridge.
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    /// A password (or an app password).
    #[default]
    Password,
    /// An OAuth2 access token (XOAUTH2), from `password_command`, which is
    /// run again for each connection, so a short-lived token stays fresh.
    Oauth2,
}

/// Where the password comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasswordFrom {
    /// The system's keychain (macOS Keychain, Windows Credential Manager,
    /// the Secret Service on Linux).
    #[default]
    Keychain,
    /// Asked for each time tuimail starts, kept in memory only.
    Ask,
    /// The first line `password_command` prints.
    Command,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    /// What the sidebar calls it.
    pub name: String,
    pub email: String,
    /// Your name, as recipients see it.
    pub display_name: String,
    pub protocol: Protocol,
    /// The IMAP or POP3 server.
    pub host: String,
    /// Its port, if not the usual one for `protocol` and `security`.
    pub port: Option<u16>,
    pub security: Security,
    /// The login name, if not `email`.
    pub username: String,
    pub auth: Auth,
    pub password: PasswordFrom,
    /// A program and its arguments that print the password or token:
    /// `["/opt/homebrew/bin/pass", "show", "mail/work"]`. The program as a
    /// full path; no shell runs it.
    pub password_command: Vec<String>,
    /// The SMTP server mail is sent through.
    pub smtp_host: String,
    pub smtp_port: Option<u16>,
    pub smtp_security: Security,
    /// The SMTP login name, if not `username`.
    pub smtp_username: String,
    /// Put a copy of what you send in the Sent folder. Gmail, Outlook and
    /// Proton's bridge do it themselves, so it's off for them unless set.
    pub save_sent: Option<bool>,
    /// A server certificate (PEM) to trust besides the system's, for a
    /// server whose certificate is its own, like Proton Mail Bridge's.
    pub certificate: Option<PathBuf>,
    /// Added under what you write, after "-- ".
    pub signature: String,
    /// POP3: leave mail on the server once downloaded (deleting it from
    /// tuimail's Trash deletes it there too).
    pub leave_on_server: bool,
    /// Folders, if they aren't found by what the server says they're for.
    pub sent_folder: String,
    pub drafts_folder: String,
    pub trash_folder: String,
    pub archive_folder: String,
    pub junk_folder: String,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            name: String::new(),
            email: String::new(),
            display_name: String::new(),
            protocol: Protocol::Imap,
            host: String::new(),
            port: None,
            security: Security::Tls,
            username: String::new(),
            auth: Auth::Password,
            password: PasswordFrom::Keychain,
            password_command: Vec::new(),
            smtp_host: String::new(),
            smtp_port: None,
            smtp_security: Security::Tls,
            smtp_username: String::new(),
            save_sent: None,
            certificate: None,
            signature: String::new(),
            leave_on_server: true,
            sent_folder: String::new(),
            drafts_folder: String::new(),
            trash_folder: String::new(),
            archive_folder: String::new(),
            junk_folder: String::new(),
        }
    }
}

impl Account {
    /// The key the cache and the keychain know the account by: its address,
    /// in lower case.
    pub fn id(&self) -> String {
        self.email.trim().to_lowercase()
    }

    /// The sidebar's name for it: `name`, else the address.
    pub fn shown_name(&self) -> String {
        let name = if self.name.trim().is_empty() {
            &self.email
        } else {
            &self.name
        };
        text::one_line(name, 60)
    }

    pub fn username(&self) -> &str {
        let name = self.username.trim();
        if name.is_empty() {
            self.email.trim()
        } else {
            name
        }
    }

    pub fn smtp_username(&self) -> &str {
        let name = self.smtp_username.trim();
        if name.is_empty() {
            self.username()
        } else {
            name
        }
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(match (self.protocol, self.security) {
            (Protocol::Imap, Security::Tls) => 993,
            (Protocol::Imap, _) => 143,
            (Protocol::Pop3, Security::Tls) => 995,
            (Protocol::Pop3, _) => 110,
        })
    }

    pub fn smtp_port(&self) -> u16 {
        self.smtp_port.unwrap_or(match self.smtp_security {
            Security::Tls => 465,
            Security::Starttls => 587,
            Security::None => 25,
        })
    }

    /// Whether to put a copy of sent mail in the Sent folder: the setting,
    /// else not for servers that do it themselves.
    pub fn save_sent(&self) -> bool {
        self.save_sent.unwrap_or_else(|| {
            let host = self.smtp_host.to_ascii_lowercase();
            !(host.ends_with("gmail.com")
                || host.ends_with("googlemail.com")
                || host.ends_with("office365.com")
                || host.ends_with("outlook.com")
                || (is_local(&host) && self.smtp_port() == 1025))
        })
    }

    /// What's wrong with it, if anything, as one line.
    pub fn problem(&self) -> Option<String> {
        if !valid_address(self.email.trim()) {
            return Some("its email address isn't one".into());
        }
        if !valid_host(&self.host) {
            return Some("its server (host) isn't a host name".into());
        }
        if !valid_host(&self.smtp_host) {
            return Some("its sending server (smtp_host) isn't a host name".into());
        }
        if self.security == Security::None && !is_local(&self.host) {
            return Some(format!(
                "security = \"none\" is only for a server on this computer, not {}",
                text::one_line(&self.host, 60)
            ));
        }
        if self.smtp_security == Security::None && !is_local(&self.smtp_host) {
            return Some(format!(
                "smtp_security = \"none\" is only for a server on this computer, not {}",
                text::one_line(&self.smtp_host, 60)
            ));
        }
        if [self.username(), self.smtp_username()]
            .iter()
            .any(|n| n.chars().any(char::is_control))
        {
            return Some("its username has control characters".into());
        }
        let needs_command = self.password == PasswordFrom::Command || self.auth == Auth::Oauth2;
        if needs_command {
            match self.password_command.first() {
                None => return Some("it needs a password_command".into()),
                Some(program) if !Path::new(program).is_absolute() => {
                    return Some("password_command must start with the program's full path".into());
                }
                Some(program) if config::maybe_remote(Path::new(program)) => {
                    return Some("password_command's program must be on this computer".into());
                }
                _ => {}
            }
        }
        if let Some(cert) = &self.certificate
            && (!cert.is_absolute() || config::maybe_remote(cert))
        {
            return Some("certificate must be a full path to a file on this computer".into());
        }
        None
    }
}

/// Whether `host` is this computer.
pub fn is_local(host: &str) -> bool {
    matches!(
        host.trim().to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]"
    )
}

/// A host name or IP address: letters, digits, `.`, `-`, `:` (IPv6) and
/// `[]`, nothing a command line or protocol could take for something else.
pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']' | '_'))
}

/// An address mail can go to: something@domain, without spaces, control
/// characters or the characters that end an address in a protocol line.
pub fn valid_address(addr: &str) -> bool {
    let Some((local, domain)) = addr.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && addr.len() <= 254
        && !addr
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ',' | ';'))
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

#[derive(Default, Serialize, Deserialize)]
struct AccountsFile {
    #[serde(default, rename = "account")]
    accounts: Vec<Account>,
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("accounts.toml")
}

/// The accounts in the file (none if there's no file yet), and warnings:
/// names tuimail doesn't know, accounts it can't use.
pub fn load(path: &Path) -> Result<(Vec<Account>, Vec<String>)> {
    let shown = config::shown(path);
    let text = match read_small(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(e).with_context(|| format!("cannot read {shown}")),
    };
    let file: AccountsFile =
        toml::from_str(&text).with_context(|| format!("{shown} is invalid"))?;
    let mut warnings = unknown_keys(&text);
    let mut accounts: Vec<Account> = Vec::new();
    for account in file.accounts {
        if let Some(problem) = account.problem() {
            warnings.push(format!(
                "the account {} can't be used: {problem}",
                account.shown_name()
            ));
        } else if accounts.iter().any(|a| a.id() == account.id()) {
            warnings.push(format!(
                "{} is in accounts.toml twice; the first is used",
                text::one_line(&account.email, 60)
            ));
        } else {
            accounts.push(account);
        }
    }
    Ok((accounts, warnings))
}

/// Reads a file tuimail keeps, refusing anything that isn't a plain file of
/// a sane size, so a link to `/dev/zero` or a pipe can't hang the start.
fn read_small(path: &Path) -> std::io::Result<String> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() || meta.len() > 1 << 20 {
        return Err(std::io::Error::other("not a plain file under 1 MB"));
    }
    std::fs::read_to_string(path)
}

/// Keys in `[[account]]` tables that tuimail doesn't read.
fn unknown_keys(text: &str) -> Vec<String> {
    let known = toml::Table::try_from(Account::default())
        .map(|t| t.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return Vec::new();
    };
    let mut unknown: Vec<String> = table
        .keys()
        .filter(|k| *k != "account")
        .map(|k| {
            format!(
                "accounts.toml: tuimail doesn't know {}",
                text::one_line(k, 40)
            )
        })
        .collect();
    let accounts = table.get("account").and_then(|a| a.as_array());
    for account in accounts.into_iter().flatten().filter_map(|a| a.as_table()) {
        // Options that are None aren't serialized, so they're added here.
        for key in account.keys() {
            let optional = ["port", "smtp_port", "save_sent", "certificate"];
            if !known.contains(key) && !optional.contains(&key.as_str()) {
                unknown.push(format!(
                    "accounts.toml: tuimail doesn't know {}",
                    text::one_line(key, 40)
                ));
            }
        }
    }
    unknown
}

/// Adds `account` to the file, or replaces the one with its address.
pub fn save(path: &Path, account: &Account) -> Result<()> {
    let shown = config::shown(path);
    let mut file: AccountsFile = match read_small(path) {
        Ok(text) => toml::from_str(&text)
            .with_context(|| format!("{shown} is invalid, so it wasn't changed"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AccountsFile::default(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {shown}")),
    };
    match file.accounts.iter_mut().find(|a| a.id() == account.id()) {
        Some(old) => *old = account.clone(),
        None => file.accounts.push(account.clone()),
    }
    write(path, &file)
}

/// Takes the account with address `id` out of the file.
pub fn remove(path: &Path, id: &str) -> Result<()> {
    let shown = config::shown(path);
    let text = read_small(path).with_context(|| format!("cannot read {shown}"))?;
    let mut file: AccountsFile = toml::from_str(&text)
        .with_context(|| format!("{shown} is invalid, so it wasn't changed"))?;
    file.accounts.retain(|a| a.id() != id);
    write(path, &file)
}

fn write(path: &Path, file: &AccountsFile) -> Result<()> {
    let mut text = String::from(
        "# tuimail's accounts. Passwords aren't kept here: see `password` in\n\
         # tuimail's README. Edit while tuimail isn't running.\n\n",
    );
    text.push_str(&toml::to_string(file)?);
    settings::write_private(path, &text)
        .with_context(|| format!("cannot write {}", config::shown(path)))
}

/// The usual servers of a mail provider.
#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub imap: (&'static str, u16, Security),
    pub pop3: (&'static str, u16, Security),
    pub smtp: (&'static str, u16, Security),
    /// What to know before logging in, shown on the setup screen.
    pub note: &'static str,
}

const TLS: Security = Security::Tls;
const STARTTLS: Security = Security::Starttls;

/// The servers for an address at a provider tuimail knows, by its domain.
pub fn preset(email: &str) -> Option<Preset> {
    let domain = email.rsplit_once('@')?.1.trim().to_ascii_lowercase();
    let p = |imap, pop3, smtp, note| {
        Some(Preset {
            imap,
            pop3,
            smtp,
            note,
        })
    };
    match domain.as_str() {
        "gmail.com" | "googlemail.com" => p(
            ("imap.gmail.com", 993, TLS),
            ("pop.gmail.com", 995, TLS),
            ("smtp.gmail.com", 465, TLS),
            "Gmail needs an app password: myaccount.google.com/apppasswords",
        ),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" | "outlook.de" | "hotmail.co.uk"
        | "hotmail.fr" | "live.co.uk" => p(
            ("outlook.office365.com", 993, TLS),
            ("outlook.office365.com", 995, TLS),
            ("smtp-mail.outlook.com", 587, STARTTLS),
            "Outlook.com allows only OAuth2: see OAuth2 in tuimail's README",
        ),
        "icloud.com" | "me.com" | "mac.com" => p(
            ("imap.mail.me.com", 993, TLS),
            ("imap.mail.me.com", 993, TLS),
            ("smtp.mail.me.com", 587, STARTTLS),
            "iCloud needs an app-specific password: account.apple.com",
        ),
        "yahoo.com" | "ymail.com" | "rocketmail.com" | "yahoo.co.uk" | "yahoo.fr" => p(
            ("imap.mail.yahoo.com", 993, TLS),
            ("pop.mail.yahoo.com", 995, TLS),
            ("smtp.mail.yahoo.com", 465, TLS),
            "Yahoo needs an app password: Account security → App passwords",
        ),
        "aol.com" => p(
            ("imap.aol.com", 993, TLS),
            ("pop.aol.com", 995, TLS),
            ("smtp.aol.com", 465, TLS),
            "AOL needs an app password: Account security → App passwords",
        ),
        "fastmail.com" | "fastmail.fm" => p(
            ("imap.fastmail.com", 993, TLS),
            ("pop.fastmail.com", 995, TLS),
            ("smtp.fastmail.com", 465, TLS),
            "Fastmail needs an app password: Settings → Privacy & Security",
        ),
        "proton.me" | "protonmail.com" | "pm.me" | "protonmail.ch" => p(
            ("127.0.0.1", 1143, STARTTLS),
            ("127.0.0.1", 1143, STARTTLS),
            ("127.0.0.1", 1025, STARTTLS),
            "Proton needs Proton Mail Bridge running; use the password it shows, \
             and its exported certificate (certificate in accounts.toml)",
        ),
        "zoho.com" | "zohomail.com" => p(
            ("imap.zoho.com", 993, TLS),
            ("pop.zoho.com", 995, TLS),
            ("smtp.zoho.com", 465, TLS),
            "",
        ),
        "gmx.com" | "gmx.net" | "gmx.de" => p(
            ("imap.gmx.net", 993, TLS),
            ("pop.gmx.net", 995, TLS),
            ("mail.gmx.net", 587, STARTTLS),
            "GMX: turn on IMAP/POP3 access in GMX's settings first",
        ),
        "web.de" => p(
            ("imap.web.de", 993, TLS),
            ("pop3.web.de", 995, TLS),
            ("smtp.web.de", 587, STARTTLS),
            "WEB.DE: turn on IMAP/POP3 access in WEB.DE's settings first",
        ),
        "yandex.com" | "yandex.ru" | "ya.ru" => p(
            ("imap.yandex.com", 993, TLS),
            ("pop.yandex.com", 995, TLS),
            ("smtp.yandex.com", 465, TLS),
            "Yandex needs an app password",
        ),
        "mail.ru" | "inbox.ru" | "list.ru" | "bk.ru" => p(
            ("imap.mail.ru", 993, TLS),
            ("pop.mail.ru", 995, TLS),
            ("smtp.mail.ru", 465, TLS),
            "Mail.ru needs a password for external apps",
        ),
        "posteo.de" | "posteo.net" => p(
            ("posteo.de", 993, TLS),
            ("posteo.de", 995, TLS),
            ("posteo.de", 465, TLS),
            "",
        ),
        "mailbox.org" => p(
            ("imap.mailbox.org", 993, TLS),
            ("pop3.mailbox.org", 995, TLS),
            ("smtp.mailbox.org", 465, TLS),
            "",
        ),
        "comcast.net" => p(
            ("imap.comcast.net", 993, TLS),
            ("mail.comcast.net", 995, TLS),
            ("smtp.comcast.net", 587, STARTTLS),
            "",
        ),
        _ => None,
    }
}

/// An account for `email` with the servers its provider uses, or a guess
/// (imap., pop. and smtp. in front of its domain) to check.
pub fn guess(email: &str, protocol: Protocol) -> (Account, &'static str) {
    let email = email.trim().to_string();
    let domain = email
        .rsplit_once('@')
        .map(|(_, d)| d.to_ascii_lowercase())
        .unwrap_or_default();
    let (incoming, smtp, note) = match preset(&email) {
        Some(p) => {
            let incoming = match protocol {
                Protocol::Imap => p.imap,
                Protocol::Pop3 => p.pop3,
            };
            (
                (incoming.0.to_string(), incoming.1, incoming.2),
                (p.smtp.0.to_string(), p.smtp.1, p.smtp.2),
                p.note,
            )
        }
        None => {
            let (prefix, port) = match protocol {
                Protocol::Imap => ("imap", 993),
                Protocol::Pop3 => ("pop", 995),
            };
            (
                (format!("{prefix}.{domain}"), port, TLS),
                (format!("smtp.{domain}"), 465, TLS),
                "These servers are a guess: check them with your provider",
            )
        }
    };
    let oauth = matches!(
        domain.as_str(),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com"
    );
    let account = Account {
        email,
        protocol,
        host: incoming.0,
        port: Some(incoming.1),
        security: incoming.2,
        smtp_host: smtp.0,
        smtp_port: Some(smtp.1),
        smtp_security: smtp.2,
        auth: if oauth { Auth::Oauth2 } else { Auth::Password },
        ..Account::default()
    };
    (account, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tuimail-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn known_providers_get_their_servers_and_others_a_guess() {
        let (gmail, note) = guess("me@gmail.com", Protocol::Imap);
        assert_eq!(gmail.host, "imap.gmail.com");
        assert_eq!((gmail.port(), gmail.security), (993, Security::Tls));
        assert_eq!(gmail.smtp_host, "smtp.gmail.com");
        assert!(!gmail.save_sent(), "Gmail saves sent mail itself");
        assert!(note.contains("app password"));

        let (pop, _) = guess("me@gmail.com", Protocol::Pop3);
        assert_eq!((pop.host.as_str(), pop.port()), ("pop.gmail.com", 995));

        let (other, note) = guess("me@example.org", Protocol::Imap);
        assert_eq!(other.host, "imap.example.org");
        assert_eq!(other.smtp_host, "smtp.example.org");
        assert!(other.save_sent());
        assert!(note.contains("guess"));

        let (outlook, _) = guess("me@outlook.com", Protocol::Imap);
        assert_eq!(outlook.auth, Auth::Oauth2);
        assert_eq!(outlook.smtp_port(), 587);
    }

    #[test]
    fn default_ports_follow_the_protocol_and_security() {
        let mut a = Account {
            email: "a@b.c".into(),
            ..Account::default()
        };
        assert_eq!(a.port(), 993);
        a.security = Security::Starttls;
        assert_eq!(a.port(), 143);
        a.protocol = Protocol::Pop3;
        assert_eq!(a.port(), 110);
        a.security = Security::Tls;
        assert_eq!(a.port(), 995);
        assert_eq!(a.smtp_port(), 465);
        a.smtp_security = Security::Starttls;
        assert_eq!(a.smtp_port(), 587);
        a.port = Some(2993);
        assert_eq!(a.port(), 2993);
        assert_eq!(a.username(), "a@b.c");
        a.username = "alice".into();
        assert_eq!((a.username(), a.smtp_username()), ("alice", "alice"));
    }

    #[test]
    fn accounts_that_could_leak_or_inject_are_refused() {
        let good = guess("me@example.org", Protocol::Imap).0;
        assert_eq!(good.problem(), None);
        let plain = Account {
            security: Security::None,
            ..good.clone()
        };
        assert!(plain.problem().unwrap().contains("this computer"));
        let local = Account {
            host: "127.0.0.1".into(),
            security: Security::None,
            ..good.clone()
        };
        assert_eq!(local.problem(), None);
        let injected = Account {
            host: "imap.example.org\r\nA1 LOGOUT".into(),
            ..good.clone()
        };
        assert!(injected.problem().is_some());
        let user = Account {
            username: "me\r\nA2 DELETE INBOX".into(),
            ..good.clone()
        };
        assert!(user.problem().is_some());
        let command = Account {
            password: PasswordFrom::Command,
            password_command: vec!["pass".into(), "show".into()],
            ..good.clone()
        };
        assert!(command.problem().unwrap().contains("full path"));
        assert!(!valid_address("a b@c.d"));
        assert!(!valid_address("<a@c.d>"));
        assert!(valid_address("a.b+tag@c.d"));
    }

    #[test]
    fn accounts_round_trip_through_the_file_and_unknown_keys_are_reported() {
        let dir = dir("accounts");
        let file = path(&dir);
        let _ = std::fs::remove_file(&file);
        assert!(load(&file).unwrap().0.is_empty());
        let mut work = guess("Me@Example.org", Protocol::Imap).0;
        work.name = "Work".into();
        save(&file, &work).unwrap();
        let home = guess("me@gmail.com", Protocol::Pop3).0;
        save(&file, &home).unwrap();
        work.signature = "Me".into();
        save(&file, &work).unwrap();
        let (accounts, warnings) = load(&file).unwrap();
        assert_eq!(accounts, [work.clone(), home.clone()], "{warnings:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(accounts[0].id(), "me@example.org");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        remove(&file, "me@gmail.com").unwrap();
        assert_eq!(load(&file).unwrap().0, [work]);

        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str("\n[[account]]\nemail = \"x@y.z\"\nhost = \"imap.y.z\"\nsmtp_host = \"smtp.y.z\"\npasword = \"oops\"\n");
        std::fs::write(&file, text).unwrap();
        let (accounts, warnings) = load(&file).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(warnings, ["accounts.toml: tuimail doesn't know pasword"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
