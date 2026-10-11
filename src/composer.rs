//! The composer: the fields of a message being written, edited in place
//! (ratatui-textarea), with addresses completed from those you've written
//! to or heard from.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{CursorMove, TextArea};

use crate::compose::{Attachment, Draft};
use crate::mail::Addr;
use crate::store::Store;
use crate::text;

/// The longest field, and the longest text, in characters.
const MAX_FIELD: usize = 10_000;
const MAX_BODY: usize = 2_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    From,
    To,
    Cc,
    Bcc,
    Subject,
    Attachments,
    Body,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::From => "From",
            Field::To => "To",
            Field::Cc => "Cc",
            Field::Bcc => "Bcc",
            Field::Subject => "Subject",
            Field::Attachments => "Files",
            Field::Body => "",
        }
    }
}

/// What the message is, for the composer's title and what happens once
/// it's sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    New,
    Reply,
    Forward,
    Draft,
}

pub struct Composer {
    /// The account it's from (an index into the app's accounts).
    pub account: usize,
    pub kind: Kind,
    pub to: TextArea<'static>,
    pub cc: TextArea<'static>,
    pub bcc: TextArea<'static>,
    pub subject: TextArea<'static>,
    pub body: TextArea<'static>,
    pub field: Field,
    pub attachments: Vec<Attachment>,
    /// The attachment the cursor is on, in the Files row.
    pub attachment: usize,
    pub in_reply_to: String,
    pub references: String,
    /// The message replied to: (account, folder, UID), marked answered once
    /// this is sent.
    pub replying: Option<(usize, String, u32)>,
    /// The saved draft this one replaces: (folder, UID), deleted once this
    /// is sent or saved again.
    pub draft_of: Option<(String, u32)>,
    /// Addresses offered for the word being typed in To, Cc or Bcc.
    pub suggestions: Vec<Addr>,
    pub suggestion: usize,
    /// Sent, waiting for the server to say it took it.
    pub sending: bool,
    /// Saved in Drafts, waiting for the server (the request's id).
    pub saving: Option<u64>,
    /// The fields as they started, to tell whether anything changed.
    start: Draft,
}

/// What a key in the composer asks the app to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Send,
    SaveDraft,
    /// Esc: close, asking first if something was written.
    Close,
    Attach,
    Edit,
    /// From: the next or previous account.
    SwitchAccount(isize),
}

fn line(text: &str) -> TextArea<'static> {
    let mut input = TextArea::new(vec![text.replace(['\r', '\n'], " ")]);
    input.move_cursor(CursorMove::End);
    input.set_cursor_line_style(ratatui::style::Style::default());
    input
}

fn value(input: &TextArea) -> String {
    input.lines().join("\n")
}

impl Composer {
    pub fn new(account: usize, kind: Kind, draft: Draft) -> Self {
        let mut body = TextArea::new(draft.body.split('\n').map(str::to_string).collect());
        body.set_cursor_line_style(ratatui::style::Style::default());
        body.set_wrap_mode(ratatui_textarea::WrapMode::WordOrGlyph);
        let field = if draft.to.trim().is_empty() {
            Field::To
        } else {
            Field::Body
        };
        let mut composer = Self {
            account,
            kind,
            to: line(&draft.to),
            cc: line(&draft.cc),
            bcc: line(&draft.bcc),
            subject: line(&draft.subject),
            body,
            field,
            attachments: draft.attachments.clone(),
            attachment: 0,
            in_reply_to: draft.in_reply_to.clone(),
            references: draft.references.clone(),
            replying: None,
            draft_of: None,
            suggestions: Vec::new(),
            suggestion: 0,
            sending: false,
            saving: None,
            start: draft,
        };
        composer.start = composer.draft();
        composer
    }

    /// The message as written so far.
    pub fn draft(&self) -> Draft {
        Draft {
            to: value(&self.to),
            cc: value(&self.cc),
            bcc: value(&self.bcc),
            subject: value(&self.subject),
            body: value(&self.body),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            attachments: self.attachments.clone(),
        }
    }

    /// Whether anything was written since it opened.
    pub fn changed(&self) -> bool {
        self.draft() != self.start
    }

    /// The fields in order, the Files row only when there are files.
    pub fn fields(&self) -> Vec<Field> {
        let mut fields = vec![
            Field::From,
            Field::To,
            Field::Cc,
            Field::Bcc,
            Field::Subject,
        ];
        if !self.attachments.is_empty() {
            fields.push(Field::Attachments);
        }
        fields.push(Field::Body);
        fields
    }

    fn step(&mut self, by: isize) {
        let fields = self.fields();
        let at = fields.iter().position(|f| *f == self.field).unwrap_or(0) as isize;
        let next = (at + by).rem_euclid(fields.len() as isize) as usize;
        self.field = fields[next];
        self.suggestions.clear();
    }

    fn input(&mut self) -> Option<&mut TextArea<'static>> {
        match self.field {
            Field::To => Some(&mut self.to),
            Field::Cc => Some(&mut self.cc),
            Field::Bcc => Some(&mut self.bcc),
            Field::Subject => Some(&mut self.subject),
            Field::Body => Some(&mut self.body),
            Field::From | Field::Attachments => None,
        }
    }

    fn is_address(&self) -> bool {
        matches!(self.field, Field::To | Field::Cc | Field::Bcc)
    }

    /// The word being typed in an address field: what follows the last
    /// comma or semicolon.
    fn typed_word(&self) -> String {
        let text = match self.field {
            Field::To => value(&self.to),
            Field::Cc => value(&self.cc),
            Field::Bcc => value(&self.bcc),
            _ => return String::new(),
        };
        text.rsplit([',', ';'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    }

    /// Looks up addresses for the word being typed.
    fn suggest(&mut self, store: &Store) {
        let word = self.typed_word();
        self.suggestions = if word.chars().count() >= 1 && !word.contains('<') {
            store.complete(&word, 6).unwrap_or_default()
        } else {
            Vec::new()
        };
        // An address typed out in full needs no suggestion.
        if self.suggestions.len() == 1 && self.suggestions[0].addr.eq_ignore_ascii_case(&word) {
            self.suggestions.clear();
        }
        self.suggestion = 0;
    }

    /// Puts the chosen suggestion in place of the word being typed.
    fn accept(&mut self) {
        let Some(addr) = self.suggestions.get(self.suggestion).cloned() else {
            return;
        };
        let Some(input) = self.input() else {
            return;
        };
        let text = value(input);
        let keep = text
            .rfind([',', ';'])
            .map(|i| format!("{} ", &text[..=i]))
            .unwrap_or_default();
        let replaced = format!("{}{}, ", keep.trim_start(), addr.display());
        *input = line(&replaced);
        self.suggestions.clear();
    }

    /// Adds pasted text where the cursor is: in a one-line field, on one
    /// line.
    pub fn paste(&mut self, pasted: &str) {
        let is_body = self.field == Field::Body;
        let Some(input) = self.input() else {
            return;
        };
        let room =
            if is_body { MAX_BODY } else { MAX_FIELD }.saturating_sub(value(input).chars().count());
        let pasted = text::first_chars(pasted, room);
        let cleaned = if is_body {
            text::clean(pasted)
                .replace("\r\n", "\n")
                .replace('\r', "\n")
        } else {
            text::one_line(pasted, room)
        };
        input.insert_str(cleaned);
    }

    pub fn on_key(&mut self, key: KeyEvent, store: &Store) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.sending {
            return Action::None;
        }
        // Suggestions take the arrows, Tab and Enter while they show.
        if !self.suggestions.is_empty() {
            match key.code {
                KeyCode::Down | KeyCode::Char('n') if key.code == KeyCode::Down || ctrl => {
                    self.suggestion = (self.suggestion + 1) % self.suggestions.len();
                    return Action::None;
                }
                KeyCode::Up | KeyCode::Char('p') if key.code == KeyCode::Up || ctrl => {
                    self.suggestion = self
                        .suggestion
                        .checked_sub(1)
                        .unwrap_or(self.suggestions.len() - 1);
                    return Action::None;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    self.accept();
                    return Action::None;
                }
                KeyCode::Esc => {
                    self.suggestions.clear();
                    return Action::None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('s') if ctrl => return Action::Send,
            KeyCode::Char('d') if ctrl => return Action::SaveDraft,
            KeyCode::Char('a') if ctrl && self.field != Field::Body => return Action::Attach,
            KeyCode::Char('o') if ctrl => return Action::Attach,
            KeyCode::Char('e') if ctrl => return Action::Edit,
            KeyCode::Esc => return Action::Close,
            KeyCode::Tab => {
                self.step(1);
                return Action::None;
            }
            KeyCode::BackTab => {
                self.step(-1);
                return Action::None;
            }
            _ => {}
        }
        match self.field {
            Field::From => match key.code {
                KeyCode::Left | KeyCode::Char('h') => return Action::SwitchAccount(-1),
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                    return Action::SwitchAccount(1);
                }
                KeyCode::Down | KeyCode::Enter => self.step(1),
                _ => {}
            },
            Field::Attachments => match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    self.attachment = self.attachment.saturating_sub(1);
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.attachment =
                        (self.attachment + 1).min(self.attachments.len().saturating_sub(1));
                }
                KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => {
                    if self.attachment < self.attachments.len() {
                        self.attachments.remove(self.attachment);
                    }
                    self.attachment = self
                        .attachment
                        .min(self.attachments.len().saturating_sub(1));
                    if self.attachments.is_empty() {
                        self.field = Field::Body;
                    }
                }
                KeyCode::Up => self.step(-1),
                KeyCode::Down | KeyCode::Enter => self.step(1),
                _ => {}
            },
            Field::Body => {
                if key.code == KeyCode::Up && self.body.cursor().0 == 0 {
                    self.step(-1);
                    return Action::None;
                }
                let long = value(&self.body).len() >= MAX_BODY;
                if !(long && matches!(key.code, KeyCode::Char(_) | KeyCode::Enter)) {
                    self.body.input(key);
                }
            }
            _ => {
                match key.code {
                    KeyCode::Enter | KeyCode::Down => {
                        self.step(1);
                        return Action::None;
                    }
                    KeyCode::Up => {
                        self.step(-1);
                        return Action::None;
                    }
                    _ => {}
                }
                let address = self.is_address();
                if let Some(input) = self.input() {
                    let full = value(input).chars().count() >= MAX_FIELD
                        && matches!(key.code, KeyCode::Char(_));
                    // Ctrl-m and Ctrl-j would add a line break to a one-line field.
                    let line_break = ctrl && matches!(key.code, KeyCode::Char('m' | 'j'));
                    if !full && !line_break {
                        input.input(key);
                    }
                }
                if address {
                    self.suggest(store);
                }
            }
        }
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(composer: &mut Composer, text: &str, store: &Store) {
        for c in text.chars() {
            composer.on_key(key(KeyCode::Char(c)), store);
        }
    }

    #[test]
    fn fields_are_written_in_turn_and_addresses_completed() {
        let store = Store::in_memory();
        store
            .remember(
                &[Addr {
                    name: "Alice Rivera".into(),
                    addr: "alice@example.com".into(),
                }],
                true,
                1,
            )
            .unwrap();
        let mut c = Composer::new(0, Kind::New, Draft::default());
        assert_eq!(c.field, Field::To);
        assert!(!c.changed());
        typed(&mut c, "ali", &store);
        assert_eq!(c.suggestions.len(), 1);
        c.on_key(key(KeyCode::Tab), &store);
        assert_eq!(value(&c.to), "Alice Rivera <alice@example.com>, ");
        assert_eq!(c.field, Field::To, "Tab took the suggestion");
        c.on_key(key(KeyCode::Enter), &store);
        assert_eq!(c.field, Field::Cc);
        c.on_key(key(KeyCode::Tab), &store);
        c.on_key(key(KeyCode::Tab), &store);
        assert_eq!(c.field, Field::Subject);
        typed(&mut c, "Lunch", &store);
        c.on_key(key(KeyCode::Enter), &store);
        assert_eq!(c.field, Field::Body);
        typed(&mut c, "Hi", &store);
        c.on_key(key(KeyCode::Enter), &store);
        typed(&mut c, "noon?", &store);
        let draft = c.draft();
        assert_eq!(draft.subject, "Lunch");
        assert_eq!(draft.body, "Hi\nnoon?");
        assert!(c.changed());
        assert_eq!(c.on_key(ctrl('s'), &store), Action::Send);
        assert_eq!(c.on_key(key(KeyCode::Esc), &store), Action::Close);
    }

    #[test]
    fn a_reply_starts_in_the_text_and_pastes_stay_on_one_line_in_fields() {
        let draft = Draft {
            to: "bob@example.org".into(),
            body: "\n\n> quoted".into(),
            ..Draft::default()
        };
        let mut c = Composer::new(0, Kind::Reply, draft);
        assert_eq!(c.field, Field::Body);
        c.field = Field::Subject;
        c.paste("two\nlines\x1b[2J");
        assert_eq!(value(&c.subject), "two lines[2J");
        c.field = Field::Body;
        c.paste("a\r\nb");
        assert!(value(&c.body).contains("a\nb"));
    }

    #[test]
    fn files_can_be_dropped_from_the_files_row() {
        let store = Store::in_memory();
        let draft = Draft {
            to: "x@y.z".into(),
            attachments: vec![
                Attachment::File("/tmp/a.txt".into()),
                Attachment::File("/tmp/b.txt".into()),
            ],
            ..Draft::default()
        };
        let mut c = Composer::new(0, Kind::New, draft);
        assert!(c.fields().contains(&Field::Attachments));
        c.field = Field::Attachments;
        c.on_key(key(KeyCode::Right), &store);
        c.on_key(key(KeyCode::Char('x')), &store);
        assert_eq!(c.attachments, [Attachment::File("/tmp/a.txt".into())]);
        c.on_key(key(KeyCode::Char('x')), &store);
        assert!(c.attachments.is_empty());
        assert_eq!(c.field, Field::Body);
    }
}
