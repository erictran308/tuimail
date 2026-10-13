//! Drawing: the sidebar of accounts and folders, the message list, the
//! message being read, the composer and the popups, laid out as tuigram's
//! panes (round-bordered boxes, the one with the keys lit).

mod composer;
mod list;
mod reader;
mod setup;
mod sidebar;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Confirm, Focus, HelpMenu, HelpTab, PickerKind, Place, PromptKind, plural};
use crate::notify::Notifications;
use crate::settings::Settings;
use crate::theme::{self, Colors};
use crate::worker::State;

/// Below this width one pane shows at a time.
const NARROW: u16 = 70;
/// From this width (beside the sidebar), the message shows beside the list.
const WIDE: u16 = 100;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let c = app.colors.clone();
    frame.render_widget(Block::new().style(Style::new().bg(c.bg).fg(c.text)), area);
    let [body, status] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    if app.setup.is_some() {
        setup::draw(frame, app, body);
    } else if app.composer.is_some() {
        composer::draw(frame, app, body);
    } else {
        panes(frame, app, body);
    }
    status_bar(frame, app, status);
    if app.picker.is_some() {
        picker(frame, app, body);
    }
    if app.prompt.is_some() {
        prompt(frame, app, body);
    }
    if app.help.is_some() {
        help(frame, app, area);
    }
    if app.confirm.is_some() {
        confirm(frame, app, area);
    }
}

/// The sidebar, the list and the reader, as many as fit.
fn panes(frame: &mut Frame, app: &mut App, body: Rect) {
    if body.width < NARROW {
        app.wide.set(false);
        let shown = match app.focus {
            Focus::Reader if app.reader.is_some() => Focus::Reader,
            Focus::Sidebar => Focus::Sidebar,
            _ => Focus::List,
        };
        match shown {
            Focus::Sidebar => sidebar::draw(frame, app, body),
            Focus::List => list::draw(frame, app, body),
            Focus::Reader => reader::draw(frame, app, body),
        }
        return;
    }
    let side = app.settings.sidebar_width().min(body.width / 3);
    let [side_area, rest] =
        Layout::horizontal([Constraint::Length(side), Constraint::Fill(1)]).areas(body);
    sidebar::draw(frame, app, side_area);
    let wide = rest.width >= WIDE;
    app.wide.set(wide);
    if wide && app.settings.preview {
        let list_width = (rest.width / 2).max(40);
        let [list_area, reader_area] =
            Layout::horizontal([Constraint::Length(list_width), Constraint::Fill(1)]).areas(rest);
        list::draw(frame, app, list_area);
        reader::draw(frame, app, reader_area);
    } else if app.focus == Focus::Reader && app.reader.is_some() {
        reader::draw(frame, app, rest);
    } else {
        list::draw(frame, app, rest);
    }
}

/// A box with a round border all round, as tuigram's panes.
pub(super) fn bordered<'a>() -> Block<'a> {
    Block::bordered().border_type(BorderType::Rounded)
}

/// The border of what has the keys is in the accent color.
pub(super) fn border(focused: bool, c: &Colors) -> Style {
    Style::new().fg(if focused { c.accent } else { c.border })
}

/// Key hints with the keys in backticks, like "`Enter` open · `r` reply",
/// as tuigram writes them: the keys stand out in the accent color, and
/// what they do is dim.
pub(super) fn hint_spans(hints: &str, c: &Colors) -> Vec<Span<'static>> {
    let key = Style::new().fg(c.accent).add_modifier(Modifier::BOLD);
    let text = Style::new().fg(c.dim);
    hints
        .split('`')
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(i, part)| Span::styled(part.to_string(), if i % 2 == 1 { key } else { text }))
        .collect()
}

/// As many of `hints` (separated by " · ") as fit in `width`, whole, as
/// `hint_spans`; cut with `…` if not even the first fits.
pub(super) fn hints_fitting(hints: &str, width: usize, c: &Colors) -> Vec<Span<'static>> {
    let shown = |items: &[&str]| items.join(" · ").replace('`', "").width();
    let mut items: Vec<&str> = hints.split(" · ").collect();
    while items.len() > 1 && shown(&items) > width {
        items.pop();
    }
    if shown(&items) > width {
        let plain = items.join(" · ").replace('`', "");
        return vec![Span::styled(fit(&plain, width), Style::new().fg(c.dim))];
    }
    hint_spans(&items.join(" · "), c)
}

/// `text` cut to `width` columns, with `…` if it was longer.
pub fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    out
}

/// `text` cut in the middle to `width` columns, with `…`: its start and
/// its end stay (an address's name and its domain).
pub fn fit_middle(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width < 3 {
        return fit(text, width);
    }
    let tail = fit_end(text, (width - 1) / 2 + 1);
    let head = fit(text, width - tail.width() + 1);
    format!("{}{}", head.trim_end_matches('…'), tail)
}

/// `text` cut to `width` columns from the left, with `…`: what it ends
/// with stays (a host's site).
pub fn fit_end(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        used += w;
        kept.push(ch);
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

/// `text` cut or padded to exactly `width` columns.
pub fn pad(text: &str, width: usize) -> String {
    let cut = fit(text, width);
    let used = cut.width();
    format!("{cut}{}", " ".repeat(width.saturating_sub(used)))
}

/// A vertical scrollbar along the right edge of `area` (inside its border).
pub(super) fn scrollbar(frame: &mut Frame, area: Rect, max: usize, position: usize, c: &Colors) {
    if max == 0 {
        return;
    }
    let mut state = ScrollbarState::new(max).position(position);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            // The border stays as the track; the thumb is drawn on it.
            .thumb_symbol("┃")
            .track_symbol(None)
            .thumb_style(Style::new().fg(c.dim)),
        area.inner(Margin::new(0, 1)),
        &mut state,
    );
}

/// A box in the middle of `area`.
pub(super) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// A message, or the keys for where you are; on the right, how the account
/// shown is doing.
fn status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    let width = area.width as usize;
    let right = connection(app);
    let right_width = right.iter().map(Span::width).sum::<usize>();
    let room = width.saturating_sub(right_width);
    let left = match &app.status {
        Some(status) if app.prompt.is_none() => {
            let color = if status.error { c.red } else { c.ok };
            vec![Span::styled(
                fit(&format!(" {}", status.text), room),
                Style::new().fg(color),
            )]
        }
        _ => hints_fitting(&format!(" {}", hints(app)), room, c),
    };
    let used: usize = left.iter().map(Span::width).sum();
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(room.saturating_sub(used))));
    spans.extend(right);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(c.bg)),
        area,
    );
}

/// The keys that work where you are.
fn hints(app: &App) -> &'static str {
    if let Some(prompt) = &app.prompt {
        return match prompt.kind {
            PromptKind::Search => "`Enter` search · `Esc` cancel · `Ctrl-u` clear",
            PromptKind::Password(_) => "`Enter` log in · `Esc` later",
            PromptKind::Attach => "`Enter` attach · `Esc` cancel",
            PromptKind::Tag => "`Enter` tag · `Esc` cancel",
        };
    }
    if app.picker.is_some() || app.help.is_some() || app.confirm.is_some() {
        return "";
    }
    if app.setup.is_some() {
        return "`Tab` next · `Ctrl-s` check and save · `Esc` cancel";
    }
    if app.composer.is_some() {
        return "`Tab` next field · `Ctrl-s` send · `Ctrl-d` save draft · `Ctrl-o` attach · `Ctrl-e` editor · `Esc` close";
    }
    match app.focus {
        Focus::Sidebar => {
            "`Enter` open · `j k` move · `Tab` messages · `c` write · `+` add account · `?` help"
        }
        Focus::List if !app.marked.is_empty() => {
            "`Space` mark · `d` delete · `e` archive · `m` move · `t` tag · `u` read/unread · `s` star · `Esc` unmark"
        }
        Focus::List if app.search.is_some() => {
            "`Enter` open · `Esc` end search · `/` search again · `r` reply · `d` delete · `?` help"
        }
        Focus::List => {
            "`Enter` open · `c` write · `r` reply · `a` all · `f` forward · `d` delete · `e` archive · `s` star · `t` tag · `/` search · `?` help"
        }
        Focus::Reader => {
            "`j k` scroll · `J K` next/previous · `r` reply · `a` all · `f` forward · `o` links · `A` files · `H` html/text · `Esc` back"
        }
    }
}

/// How the account on screen is doing: checking, offline, or waiting for
/// its password.
fn connection(app: &App) -> Vec<Span<'static>> {
    let c = &app.colors;
    let account = match &app.place {
        Place::Folder { account, .. } => Some(*account),
        Place::AllInboxes => None,
    };
    let states: Vec<usize> = match account {
        Some(i) => vec![i],
        None => (0..app.accounts.len()).collect(),
    };
    for i in states {
        let Some(a) = app.accounts.get(i) else {
            continue;
        };
        let name = fit(&a.account.shown_name(), 24);
        let (text, color) = match &a.state {
            State::NeedPassword { .. } => (format!(" {name}: password needed · P "), c.warning),
            State::Offline(_) => (format!(" {} {name} offline ", app.icons.offline), c.red),
            _ if !a.syncing.is_empty() => (format!(" {} checking ", app.icons.loading), c.dim),
            _ => continue,
        };
        return vec![Span::styled(text, Style::new().fg(color))];
    }
    if app.demo {
        return vec![Span::styled(" demo ", Style::new().fg(c.dim))];
    }
    Vec::new()
}

/// The one-line prompt, in a box over the bottom of the window.
fn prompt(frame: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors.clone();
    let label = match app.prompt.as_ref().map(|p| p.kind) {
        Some(PromptKind::Search) => format!(" {} Search ", app.icons.search),
        Some(PromptKind::Password(i)) => format!(
            " Password for {} ",
            app.accounts
                .get(i)
                .map(|a| fit(&a.account.email, 50))
                .unwrap_or_default()
        ),
        Some(PromptKind::Attach) => " Attach a file: its full path, or drop it here ".into(),
        Some(PromptKind::Tag) => " New tag: one word, like work or receipts ".into(),
        None => return,
    };
    let width = area.width.min(80);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + area.height.saturating_sub(4),
        width,
        height: 3.min(area.height),
    };
    frame.render_widget(Clear, rect);
    let block = bordered()
        .border_style(border(true, &c))
        .title(Span::styled(
            label,
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ))
        .style(Style::new().bg(c.panel));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let Some(prompt) = app.prompt.as_mut() else {
        return;
    };
    prompt.input.set_style(Style::new().fg(c.text).bg(c.panel));
    prompt
        .input
        .set_cursor_style(Style::new().fg(c.bg).bg(c.accent));
    let input_area = inner.inner(Margin::new(1, 0));
    frame.render_widget(&prompt.input, input_area);
}

/// A list to pick from: folders to move to, links, attachments.
fn picker(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    let Some(picker) = &app.picker else {
        return;
    };
    let hint = match picker.kind {
        PickerKind::MoveTo { .. } => " `Enter` move here · `Esc` cancel ",
        PickerKind::Links => " `Enter` open · `y` copy · `Esc` close ",
        PickerKind::Attachments => " `Enter` open · `s` save to Downloads · `Esc` close ",
        PickerKind::Tag { .. } => " `Enter` put on or take off · `Esc` cancel ",
        PickerKind::Changes(_) => " `Enter` undo · `Esc` close ",
    };
    // Links in columns: the site (from the whole URL, never from what the
    // link says), what it says, then the URL.
    let columns =
        matches!(picker.kind, PickerKind::Links) && picker.labels.len() == picker.items.len();
    let label_width = |column: fn(&(String, String)) -> &String| {
        picker
            .labels
            .iter()
            .map(|l| column(l).width())
            .max()
            .unwrap_or(0)
    };
    let (hosts, saids) = if columns {
        (label_width(|l| &l.0), label_width(|l| &l.1))
    } else {
        (0, 0)
    };
    let gap = |w: usize| if w > 0 { w + 2 } else { 0 };
    let widest =
        picker.items.iter().map(|i| i.width()).max().unwrap_or(0) + gap(hosts) + gap(saids);
    let width = (widest + 6)
        .max(picker.title.width() + 4)
        .max(hint.replace('`', "").width() + 4)
        .min(usize::from(area.width).saturating_sub(4)) as u16;
    let height = (picker.items.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(3);
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block = bordered()
        .border_style(border(true, c))
        .title(Span::styled(
            picker.title.clone(),
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(hint_spans(hint, c)).right_aligned())
        .style(Style::new().bg(c.panel));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let rows = usize::from(inner.height);
    let scroll = picker.selected.saturating_sub(rows.saturating_sub(1));
    let room = usize::from(inner.width).saturating_sub(2);
    let (host_width, said_width) = (hosts.min(room / 3), saids.min(room / 3));
    let lines: Vec<Line> = picker
        .items
        .iter()
        .enumerate()
        .skip(scroll)
        .take(rows)
        .map(|(i, item)| {
            let chosen = i == picker.selected;
            let style = if chosen {
                Style::new().fg(c.text).bg(c.selected)
            } else {
                Style::new().fg(c.text)
            };
            let item = match picker.kind {
                PickerKind::Links if columns => {
                    let (host, said) = &picker.labels[i];
                    let mut row = format!("{}  ", pad(&fit_end(host, host_width), host_width));
                    if said_width > 0 {
                        row.push_str(&format!("{}  ", pad(said, said_width)));
                    }
                    row.push_str(item);
                    fit(&row, room)
                }
                // A link is cut from the left: its site stays in view.
                PickerKind::Links if item.width() > room => {
                    let host = crate::app::link_host(item);
                    fit(&format!("{host}  {item}"), room)
                }
                _ => fit(item, room),
            };
            Line::from(vec![
                Span::styled(if chosen { "▌" } else { " " }, Style::new().fg(c.accent)),
                Span::styled(pad(&format!("{item} "), room + 1), style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    scrollbar(
        frame,
        rect,
        picker.items.len().saturating_sub(rows),
        scroll,
        c,
    );
}

/// The question waiting for `y`, in a popup in the middle.
fn confirm(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    let Some(confirm) = &app.confirm else {
        return;
    };
    let (title, text, hint) = match confirm {
        Confirm::Agent { id } => {
            let Some(ask) = app.agent_asks.iter().find(|a| a.id == *id) else {
                return;
            };
            // Who asks is what it calls itself: said as such.
            let mut text = format!(
                "{} (an AI agent, as it calls itself) wants to {}:",
                fit(&ask.who, 40),
                ask.what
            );
            for line in &ask.which {
                text.push_str(&format!("\n  {}", fit(line, 70)));
            }
            text.push_str("\nYou can undo it later with z.");
            (
                " An AI agent asks ",
                text,
                " `y` let it · any other key says no ",
            )
        }
        Confirm::DeleteForever { uids, .. } => (
            " Delete for good? ",
            format!(
                "{} will be deleted from the server. This can't be undone.",
                plural(uids.len(), "message")
            ),
            " `y` delete · any other key keeps them ",
        ),
        Confirm::Delete { uids, .. } => (
            " Move to Trash? ",
            format!("{} will go to Trash.", plural(uids.len(), "message")),
            " `y` move · any other key keeps them ",
        ),
        Confirm::Send {
            subject,
            recipients,
            attachments,
            ..
        } => {
            // Every address it goes to (Bcc too) and every file, as asked
            // about: what `y` sends.
            let mut text = if subject.is_empty() {
                format!(
                    "It has no subject. Send it to {} anyway?",
                    plural(recipients.len(), "recipient")
                )
            } else {
                format!(
                    "Send “{}” to {}?",
                    fit(subject, 50),
                    plural(recipients.len(), "recipient")
                )
            };
            const SHOWN: usize = 8;
            for r in recipients.iter().take(SHOWN) {
                text.push_str(&format!("\n  {}", fit_middle(r, 60)));
            }
            if recipients.len() > SHOWN {
                text.push_str(&format!("\n  and {} more", recipients.len() - SHOWN));
            }
            if !attachments.is_empty() {
                text.push_str(&format!("\n{}:", plural(attachments.len(), "file")));
                for a in attachments.iter().take(4) {
                    text.push_str(&format!("\n  {}", fit_middle(a, 60)));
                }
                if attachments.len() > 4 {
                    text.push_str(&format!("\n  and {} more", attachments.len() - 4));
                }
            }
            (" Send? ", text, " `y` send · any other key keeps writing ")
        }
        Confirm::CloseComposer => (
            " Close the message? ",
            "Keep it in Drafts to finish later, or throw it away.".to_string(),
            " `y` save draft · `n` discard · any other key keeps writing ",
        ),
        Confirm::OpenAttachment { name, .. } => (
            " Open this file? ",
            format!(
                "“{}” is a kind of file that can run things on your computer. Open it only if you trust the sender.",
                fit(name, 60)
            ),
            " `y` open · any other key cancels ",
        ),
        Confirm::OpenLink { url } => (
            " Unsubscribe? ",
            format!(
                "Open the list's unsubscribe page on {}?\n  {}",
                fit_middle(&crate::app::link_host(url), 50),
                fit_middle(url, 60)
            ),
            " `y` open · any other key cancels ",
        ),
        Confirm::Quit { waiting } => (
            " Quit? ",
            format!(
                "{} haven't reached the server yet (it's offline or busy). Quit anyway, and they're lost? Deletes for good are made at the next start.",
                plural(*waiting, "change")
            ),
            " `y` quit · any other key stays ",
        ),
        Confirm::RemoveAccount(i) => (
            " Remove the account? ",
            format!(
                "{} goes from tuimail, with the mail kept here and its saved password. Mail on the server stays.",
                app.accounts
                    .get(*i)
                    .map(|a| a.account.shown_name())
                    .unwrap_or_default()
            ),
            " `y` remove · any other key keeps it ",
        ),
    };
    let hint_width = hint.replace('`', "").width();
    let width = (text.width().max(hint_width).max(title.width()) + 6)
        .min(70)
        .min(usize::from(area.width)) as u16;
    let wrapped: Vec<String> = textwrap::wrap(&text, usize::from(width).saturating_sub(4))
        .into_iter()
        .map(|l| l.into_owned())
        .collect();
    let height = (wrapped.len() as u16 + 4).min(area.height);
    let popup = centered(area, width, height);
    frame.render_widget(Clear, popup);
    let block = bordered()
        .border_style(border(true, c))
        .title(Span::styled(
            title,
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(hint_spans(hint, c)).right_aligned())
        .style(Style::new().bg(c.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let mut lines = vec![Line::default()];
    lines.extend(
        wrapped
            .into_iter()
            .map(|l| Line::from(Span::styled(format!(" {l}"), Style::new().fg(c.text)))),
    );
    frame.render_widget(Paragraph::new(lines), inner);
}

const HELP: [(&str, &str); 31] = [
    ("←↓↑→  h j k l", "move; Enter or → opens"),
    ("Tab", "folders → messages → message"),
    ("gg  G  Ctrl-u Ctrl-d", "first, last, half a page"),
    ("c", "write a message"),
    ("r  /  a  /  f", "reply  /  reply to all  /  forward"),
    ("d", "delete: to Trash; in Trash, for good"),
    ("e  /  !", "archive  /  junk"),
    ("m", "move to a folder"),
    ("t", "tag: put a tag on or take it off; tag:work searches"),
    (
        "z  /  Z",
        "undo the last change  /  the changes, to undo one",
    ),
    ("s  /  u", "star  /  read or unread"),
    (
        "Space  /  *",
        "mark a message  /  all; actions take the marked",
    ),
    ("/", "search: here, and the server's folder"),
    ("Esc", "end the search, unmark, back"),
    ("J  K", "next, previous message (reading)"),
    ("Space  b", "a page down, up (reading)"),
    ("o", "the message's links: open, or y copy"),
    ("A", "its attachments: open, or s save"),
    ("H", "its HTML or plain text"),
    ("U", "unsubscribe from a mailing list"),
    ("R", "check for mail now"),
    ("+  /  x  /  P", "add an account / remove it / its password"),
    ("Ctrl-s", "send (writing)"),
    ("Ctrl-d", "save in Drafts and close (writing)"),
    ("Ctrl-o", "attach a file; or drop it on the window"),
    ("Ctrl-e", "write the text in your editor"),
    (
        "Tab  ↑↓",
        "the next field (writing); Tab takes a suggestion",
    ),
    ("←→ on From", "send from another account"),
    ("x on Files", "take a file off"),
    ("?  /  O", "these keys  /  settings and themes"),
    ("q", "quit"),
];

/// The `?` popup, centered: a tab with every key, and one with the
/// settings. Both are as big as the bigger one, so the tabs don't move.
fn help(frame: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors.clone();
    let Some(menu) = app.help.as_mut() else {
        return;
    };
    let keys = key_lines(&c);
    let (settings, cursor) = settings_lines(&app.settings, menu.selected, &c);
    let widest = |lines: &[Line]| lines.iter().map(Line::width).max().unwrap_or(0);
    let width = (widest(&keys).max(widest(&settings)) + 3).min(area.width as usize) as u16;
    let height = (keys.len().max(settings.len()) as u16 + 2).min(area.height);
    let popup = centered(area, width, height);
    let tab = |label: &'static str, active: bool| {
        if active {
            Span::styled(
                label,
                Style::new()
                    .fg(c.bg)
                    .bg(c.accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(label, Style::new().fg(c.dim))
        }
    };
    let tabs = Line::from(vec![
        Span::raw(" "),
        tab(" Keys ", menu.tab == HelpTab::Keys),
        Span::raw(" "),
        tab(" Settings ", menu.tab == HelpTab::Settings),
        Span::raw(" "),
    ]);
    let hint = match menu.tab {
        HelpTab::Keys => " `j k` scroll · `Tab` settings · `Esc` close ",
        HelpTab::Settings if menu.selected >= HelpMenu::THEMES => {
            " `Enter` use · `Tab` keys · `Esc` close "
        }
        HelpTab::Settings => " `Enter` on/off · `Tab` keys · `Esc` close ",
    };
    frame.render_widget(Clear, popup);
    let block = bordered()
        .border_style(border(true, &c))
        .title(tabs)
        .title_bottom(Line::from(hint_spans(hint, &c)).right_aligned())
        .style(Style::new().bg(c.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = usize::from(inner.height);
    let (lines, scroll) = match menu.tab {
        HelpTab::Keys => (keys, &mut menu.scroll),
        HelpTab::Settings => {
            let scroll = &mut menu.settings_scroll;
            *scroll = (*scroll).min(cursor.saturating_sub(1));
            let cursor_end = if menu.selected == HelpMenu::LAST {
                settings.len() - 1
            } else {
                cursor
            };
            if cursor_end >= *scroll + rows {
                *scroll = cursor_end + 1 - rows;
            }
            (settings, scroll)
        }
    };
    let max = lines.len().saturating_sub(rows);
    *scroll = (*scroll).min(max);
    frame.render_widget(Paragraph::new(lines).scroll((*scroll as u16, 0)), inner);
    scrollbar(frame, popup, max, *scroll, &c);
}

fn key_lines(c: &Colors) -> Vec<Line<'static>> {
    let key_width = HELP.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
    HELP.iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<key_width$}  "), Style::new().fg(c.accent)),
                Span::styled(*what, Style::new().fg(c.text)),
            ])
        })
        .collect()
}

/// The settings tab, and the line of the row `selected`.
fn settings_lines(settings: &Settings, selected: usize, c: &Colors) -> (Vec<Line<'static>>, usize) {
    let mut lines = Vec::new();
    let mut cursor = 0;
    let mut add = |lines: &mut Vec<Line<'static>>, row: usize, mark: &'static str, label: &str| {
        let chosen = row == selected;
        let bar = if chosen {
            Span::styled("▌", Style::new().fg(c.accent))
        } else {
            Span::raw(" ")
        };
        let line = Line::from(vec![
            bar,
            Span::styled(mark, Style::new().fg(c.accent)),
            Span::styled(label.to_string(), Style::new().fg(c.text)),
        ]);
        if chosen {
            cursor = lines.len();
            lines.push(line.style(Style::new().bg(c.selected)));
        } else {
            lines.push(line);
        }
    };
    let heading = |text: &'static str| {
        Line::from(Span::styled(
            text,
            Style::new().fg(c.dim).add_modifier(Modifier::BOLD),
        ))
    };
    let check = |on: bool| if on { " [✓] " } else { " [ ] " };
    lines.push(heading(" Reading"));
    add(
        &mut lines,
        HelpMenu::PREVIEW,
        check(settings.preview),
        "Show the message beside the list (wide windows)",
    );
    add(
        &mut lines,
        HelpMenu::PREFER_PLAIN,
        check(settings.prefer_plain),
        "Show plain text rather than HTML when a message has both",
    );
    add(
        &mut lines,
        HelpMenu::MARK_READ,
        check(settings.mark_read),
        "Mark messages read when they're shown",
    );
    add(
        &mut lines,
        HelpMenu::THREADS,
        check(settings.threads),
        "Group conversations (threads)",
    );
    lines.push(Line::default());
    lines.push(heading(" Asking first"));
    add(
        &mut lines,
        HelpMenu::CONFIRM_DELETE,
        check(settings.confirm_delete),
        "Before moving mail to Trash",
    );
    add(
        &mut lines,
        HelpMenu::CONFIRM_SEND,
        check(settings.confirm_send),
        "Before sending",
    );
    lines.push(Line::default());
    lines.push(heading(" New mail"));
    add(
        &mut lines,
        HelpMenu::NOTIFICATIONS,
        check(settings.notifications != Notifications::Off),
        "Notifications (through the terminal)",
    );
    lines.push(Line::default());
    lines.push(heading(" AI agents"));
    add(
        &mut lines,
        HelpMenu::AGENTS,
        check(settings.agents),
        "Let AI agents read and sort your mail (tuimail mcp)",
    );
    add(
        &mut lines,
        HelpMenu::CONFIRM_AGENTS,
        check(settings.confirm_agents),
        "Ask before each change an agent makes",
    );
    lines.push(Line::default());
    lines.push(heading(" Theme"));
    for (i, (id, name)) in theme::names().iter().enumerate() {
        let mark = if *id == settings.theme {
            " ● "
        } else {
            " ○ "
        };
        add(&mut lines, HelpMenu::THEMES + i, mark, name);
    }
    (lines, cursor)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::app::tests::{app, with_mail};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    pub fn drawn(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    pub fn screen(app: &mut App, width: u16, height: u16) -> String {
        let buffer = drawn(app, width, height);
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn text_is_cut_to_fit_with_an_ellipsis() {
        assert_eq!(fit("hello", 10), "hello");
        assert_eq!(fit("hello world", 6), "hello…");
        assert_eq!(fit("日本語テキスト", 5), "日本…");
        assert_eq!(fit("abc", 0), "");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("abcdef", 4), "abc…");
    }

    #[test]
    fn key_hints_show_their_keys_in_the_accent_color_and_fit_whole() {
        let c = Colors::named(theme::DEFAULT);
        let spans = hint_spans(" `Enter` open · `r` reply", &c);
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, [" ", "Enter", " open · ", "r", " reply"]);
        assert_eq!(spans[1].style.fg, Some(c.accent));
        let hints = " `Enter` open · `r` reply · `f` forward";
        let text = |spans: Vec<Span>| {
            spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(
            text(hints_fitting(hints, 40, &c)),
            " Enter open · r reply · f forward"
        );
        assert_eq!(text(hints_fitting(hints, 21, &c)), " Enter open · r reply");
        assert_eq!(text(hints_fitting(hints, 20, &c)), " Enter open");
        assert_eq!(text(hints_fitting(hints, 5, &c)), " Ent…");
    }

    #[tokio::test]
    async fn every_size_of_window_draws() {
        let mut app = app();
        with_mail(&mut app);
        for width in [1, 20, 50, 69, 70, 99, 130, 200] {
            for height in [1, 3, 10, 30] {
                screen(&mut app, width, height);
            }
        }
        app.help = Some(HelpMenu::new(HelpTab::Settings));
        for width in [1, 30, 80, 200] {
            screen(&mut app, width, 20);
        }
    }

    #[tokio::test]
    async fn the_help_lists_the_keys_then_the_settings() {
        let mut app = app();
        app.help = Some(HelpMenu::new(HelpTab::Keys));
        let shown = screen(&mut app, 120, 40);
        assert!(shown.contains("reply to all"), "{shown}");
        assert!(!shown.contains("Before sending"));
        app.help = Some(HelpMenu::new(HelpTab::Settings));
        let shown = screen(&mut app, 120, 40);
        assert!(shown.contains("Before sending"), "{shown}");
        assert!(shown.contains("Catppuccin Mocha"));
        let line = shown
            .lines()
            .find(|l| l.contains("Catppuccin Mocha"))
            .unwrap();
        assert!(line.contains('●'), "the theme in use");
    }
}
