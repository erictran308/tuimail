//! Adding an account: a form with the address and password, the servers
//! filled in for providers tuimail knows (and guessed for others), checked
//! by logging in before the account is saved.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{CursorMove, TextArea};

use crate::account::{self, Account, PasswordFrom, Protocol, Security};
use crate::text;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Email,
    Password,
    Name,
    Protocol,
    Host,
    Port,
    Security,
    SmtpHost,
    SmtpPort,
    SmtpSecurity,
    Username,
}

pub const ROWS: [Row; 11] = [
    Row::Email,
    Row::Password,
    Row::Name,
    Row::Protocol,
    Row::Host,
    Row::Port,
    Row::Security,
    Row::SmtpHost,
    Row::SmtpPort,
    Row::SmtpSecurity,
    Row::Username,
];

impl Row {
    pub fn label(self) -> &'static str {
        match self {
            Row::Email => "Email address",
            Row::Password => "Password",
            Row::Name => "Your name",
            Row::Protocol => "Receive with",
            Row::Host => "Incoming server",
            Row::Port => "Port",
            Row::Security => "Security",
            Row::SmtpHost => "Outgoing server",
            Row::SmtpPort => "Port",
            Row::SmtpSecurity => "Security",
            Row::Username => "Username",
        }
    }

    /// Rows toggled with Space or the arrows, not typed in.
    pub fn is_choice(self) -> bool {
        matches!(self, Row::Protocol | Row::Security | Row::SmtpSecurity)
    }
}

pub struct Setup {
    pub email: TextArea<'static>,
    pub password: TextArea<'static>,
    pub name: TextArea<'static>,
    pub protocol: Protocol,
    pub host: TextArea<'static>,
    pub port: TextArea<'static>,
    pub security: Security,
    pub smtp_host: TextArea<'static>,
    pub smtp_port: TextArea<'static>,
    pub smtp_security: Security,
    pub username: TextArea<'static>,
    pub row: usize,
    /// What to know about the provider, or that the servers are a guess.
    pub note: String,
    pub error: Option<String>,
    /// Logging in to check, waiting for the answer.
    pub checking: bool,
    /// The address the servers were filled in for.
    filled_for: String,
    /// The servers were changed by hand: typing the address again doesn't
    /// replace them.
    servers_edited: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// Check the login, then save.
    Check,
    /// Save without checking.
    SaveAnyway,
    Cancel,
}

fn input(text: &str) -> TextArea<'static> {
    let mut input = TextArea::new(vec![text.to_string()]);
    input.move_cursor(CursorMove::End);
    input.set_cursor_line_style(ratatui::style::Style::default());
    input
}

fn value(input: &TextArea) -> String {
    input.lines().join("").trim().to_string()
}

impl Default for Setup {
    fn default() -> Self {
        let mut password = input("");
        password.set_mask_char('•');
        Self {
            email: input(""),
            password,
            name: input(""),
            protocol: Protocol::Imap,
            host: input(""),
            port: input(""),
            security: Security::Tls,
            smtp_host: input(""),
            smtp_port: input(""),
            smtp_security: Security::Tls,
            username: input(""),
            row: 0,
            note: String::new(),
            error: None,
            checking: false,
            filled_for: String::new(),
            servers_edited: false,
        }
    }
}

impl Setup {
    pub fn current(&self) -> Row {
        ROWS[self.row]
    }

    fn input_mut(&mut self, row: Row) -> Option<&mut TextArea<'static>> {
        Some(match row {
            Row::Email => &mut self.email,
            Row::Password => &mut self.password,
            Row::Name => &mut self.name,
            Row::Host => &mut self.host,
            Row::Port => &mut self.port,
            Row::SmtpHost => &mut self.smtp_host,
            Row::SmtpPort => &mut self.smtp_port,
            Row::Username => &mut self.username,
            _ => return None,
        })
    }

    /// Fills in the servers for the address typed, unless they were
    /// changed by hand.
    fn fill(&mut self) {
        let email = value(&self.email);
        if email == self.filled_for || !email.contains('@') {
            return;
        }
        self.filled_for = email.clone();
        if self.servers_edited {
            return;
        }
        let (guess, note) = account::guess(&email, self.protocol);
        self.host = input(&guess.host);
        self.port = input(&guess.port().to_string());
        self.security = guess.security;
        self.smtp_host = input(&guess.smtp_host);
        self.smtp_port = input(&guess.smtp_port().to_string());
        self.smtp_security = guess.smtp_security;
        self.note = note.to_string();
    }

    fn toggle(&mut self, row: Row, by: isize) {
        let cycle = |s: Security, by: isize| {
            let all = [Security::Tls, Security::Starttls, Security::None];
            let at = all.iter().position(|x| *x == s).unwrap_or(0) as isize;
            all[(at + by).rem_euclid(3) as usize]
        };
        match row {
            Row::Protocol => {
                self.protocol = match self.protocol {
                    Protocol::Imap => Protocol::Pop3,
                    Protocol::Pop3 => Protocol::Imap,
                };
                // Different servers for the other protocol.
                self.filled_for.clear();
                self.servers_edited = false;
                self.fill();
            }
            Row::Security => {
                self.security = cycle(self.security, by);
                self.servers_edited = true;
            }
            Row::SmtpSecurity => {
                self.smtp_security = cycle(self.smtp_security, by);
                self.servers_edited = true;
            }
            _ => {}
        }
    }

    fn step(&mut self, by: isize) {
        if self.current() == Row::Email {
            self.fill();
        }
        self.row = (self.row as isize + by).rem_euclid(ROWS.len() as isize) as usize;
    }

    pub fn paste(&mut self, text: &str) {
        let row = self.current();
        if let Some(input) = self.input_mut(row) {
            let line = if row == Row::Password {
                text.trim_matches(['\r', '\n']).to_string()
            } else {
                text::one_line(text, 300)
            };
            input.insert_str(line);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.checking {
            return Action::None;
        }
        let row = self.current();
        match key.code {
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Char('s') if ctrl => {
                self.fill();
                return Action::Check;
            }
            KeyCode::Char('f') if ctrl => {
                self.fill();
                return Action::SaveAnyway;
            }
            KeyCode::Tab | KeyCode::Down => self.step(1),
            KeyCode::BackTab | KeyCode::Up => self.step(-1),
            KeyCode::Enter => {
                if self.row + 1 == ROWS.len()
                    || (row == Row::Password && !self.filled_for.is_empty())
                {
                    self.fill();
                    return Action::Check;
                }
                self.step(1);
            }
            KeyCode::Left | KeyCode::Char('h') if row.is_choice() => self.toggle(row, -1),
            KeyCode::Right | KeyCode::Char('l' | ' ') if row.is_choice() => self.toggle(row, 1),
            _ => {
                let edits_server =
                    matches!(row, Row::Host | Row::Port | Row::SmtpHost | Row::SmtpPort);
                let numeric = matches!(row, Row::Port | Row::SmtpPort);
                if let KeyCode::Char(c) = key.code
                    && !ctrl
                    && (text::is_hidden(c) || (numeric && !c.is_ascii_digit()))
                {
                    return Action::None;
                }
                if let Some(input) = self.input_mut(row) {
                    let full = input.lines().join("").chars().count() >= 300
                        && matches!(key.code, KeyCode::Char(_));
                    let line_break = ctrl && matches!(key.code, KeyCode::Char('m' | 'j'));
                    if !full && !line_break {
                        input.input(key);
                    }
                    if edits_server {
                        self.servers_edited = true;
                    }
                }
                self.error = None;
            }
        }
        Action::None
    }

    pub fn password(&self) -> String {
        self.password.lines().join("")
    }

    /// The account the form describes, or what's wrong with it.
    pub fn account(&self, keychain: bool) -> Result<Account, String> {
        let email = value(&self.email);
        if !account::valid_address(&email) {
            return Err("Type your email address".into());
        }
        if self.password().is_empty() {
            return Err("Type your password (or app password)".into());
        }
        let port = |input: &TextArea, what: &str| -> Result<Option<u16>, String> {
            let text = value(input);
            if text.is_empty() {
                return Ok(None);
            }
            text.parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .map(Some)
                .ok_or_else(|| format!("The {what} port isn't a port number"))
        };
        let (guess, _) = account::guess(&email, self.protocol);
        let name = value(&self.name);
        let account = Account {
            name: email
                .rsplit_once('@')
                .map(|(_, d)| d.to_string())
                .unwrap_or_default(),
            email: email.clone(),
            display_name: name,
            protocol: self.protocol,
            host: value(&self.host),
            port: port(&self.port, "incoming")?,
            security: self.security,
            username: value(&self.username),
            auth: guess.auth,
            password: if keychain {
                PasswordFrom::Keychain
            } else {
                PasswordFrom::Ask
            },
            smtp_host: value(&self.smtp_host),
            smtp_port: port(&self.smtp_port, "outgoing")?,
            smtp_security: self.smtp_security,
            ..Account::default()
        };
        if account.auth == account::Auth::Oauth2 {
            return Err(
                "This provider needs OAuth2: add it to accounts.toml (see tuimail's README)".into(),
            );
        }
        if let Some(problem) = account.problem() {
            return Err(format!("The account can't be used: {problem}"));
        }
        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(setup: &mut Setup, text: &str) {
        for c in text.chars() {
            setup.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn the_servers_fill_in_from_the_address_and_the_form_makes_an_account() {
        let mut setup = Setup::default();
        typed(&mut setup, "me@gmail.com");
        setup.on_key(key(KeyCode::Tab));
        assert_eq!(value(&setup.host), "imap.gmail.com");
        assert_eq!(value(&setup.smtp_port), "465");
        assert!(setup.note.contains("app password"));
        assert!(setup.account(true).unwrap_err().contains("password"));
        typed(&mut setup, "abcd efgh");
        assert_eq!(
            setup.on_key(key(KeyCode::Enter)),
            Action::Check,
            "Enter on the password checks"
        );
        let account = setup.account(true).unwrap();
        assert_eq!(account.email, "me@gmail.com");
        assert_eq!(account.host, "imap.gmail.com");
        assert_eq!(account.port(), 993);
        assert_eq!(account.password, PasswordFrom::Keychain);
        assert_eq!(setup.account(false).unwrap().password, PasswordFrom::Ask);

        // POP3 instead: its servers.
        setup.row = ROWS.iter().position(|r| *r == Row::Protocol).unwrap();
        setup.on_key(key(KeyCode::Char(' ')));
        assert_eq!(value(&setup.host), "pop.gmail.com");
        // Ports take digits only.
        setup.row = ROWS.iter().position(|r| *r == Row::Port).unwrap();
        typed(&mut setup, "x");
        assert_eq!(value(&setup.port), "995");
    }

    #[test]
    fn servers_changed_by_hand_stay() {
        let mut setup = Setup::default();
        typed(&mut setup, "me@example.org");
        setup.on_key(key(KeyCode::Tab));
        assert_eq!(value(&setup.host), "imap.example.org");
        setup.row = ROWS.iter().position(|r| *r == Row::Host).unwrap();
        setup.host = input("mail.example.org");
        setup.on_key(key(KeyCode::Char('x')));
        setup.on_key(key(KeyCode::Backspace));
        setup.row = 0;
        typed(&mut setup, "m");
        setup.on_key(key(KeyCode::Tab));
        assert_eq!(value(&setup.host), "mail.example.org");
    }
}
