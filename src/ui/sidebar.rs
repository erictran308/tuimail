//! The sidebar: every inbox together, each account and its folders, and
//! adding an account.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{border, bordered, fit, scrollbar};
use crate::app::{App, Focus, Place, Row, folder_label};
use crate::store::Role;
use crate::text::cols;
use crate::worker::State;

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    let focused = app.focus == Focus::Sidebar;
    let logo = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("{} tuimail", app.icons.logo),
            Style::new().fg(c.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    let block = bordered().border_style(border(focused, c)).title(logo);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = app.sidebar_rows();
    let height = usize::from(inner.height);
    let width = usize::from(inner.width);
    // Scrolled as little as keeps the cursor in view.
    let mut scroll = app.sidebar_scroll.get().min(app.sidebar_selected);
    if app.sidebar_selected >= scroll + height {
        scroll = app.sidebar_selected + 1 - height;
    }
    app.sidebar_scroll.set(scroll);
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(i, row)| line(app, *row, i == app.sidebar_selected, focused, width))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    scrollbar(frame, area, rows.len().saturating_sub(height), scroll, c);
}

fn line(app: &App, row: Row, selected: bool, focused: bool, width: usize) -> Line<'static> {
    let c = &app.colors;
    let icons = &app.icons;
    let (indent, icon, label, count, mut style) = match row {
        Row::AllInboxes => {
            let unread = app.unread_inboxes();
            (
                0,
                icons.all,
                "All inboxes".to_string(),
                unread,
                Style::new().fg(c.text),
            )
        }
        Row::Account(i) => {
            let a = &app.accounts[i];
            let state = match &a.state {
                State::NeedPassword { .. } => " (password?)",
                State::Offline(_) => " (offline)",
                _ => "",
            };
            let fold = if a.expanded { "" } else { " …" };
            (
                0,
                icons.account,
                format!("{}{state}{fold}", app.account_name(i)),
                0,
                Style::new().fg(c.subtext).add_modifier(Modifier::BOLD),
            )
        }
        Row::Folder(i, f, depth) => {
            let folder = &app.accounts[i].folders[f];
            let icon = match folder.role {
                Some(Role::Inbox) => icons.inbox,
                Some(Role::Sent) => icons.sent,
                Some(Role::Drafts) => icons.drafts,
                Some(Role::Trash) => icons.trash,
                Some(Role::Junk) => icons.junk,
                Some(Role::Archive) => icons.archive,
                Some(Role::All) => icons.all,
                Some(Role::Flagged) => icons.flagged,
                None => icons.folder,
            };
            // Unread counts, except where unread mail doesn't matter;
            // Drafts counts what's in it.
            let count = match folder.role {
                Some(Role::Drafts) => folder.total,
                Some(Role::Sent | Role::Trash | Role::Junk | Role::All) => 0,
                _ => folder.unread,
            };
            let style = if folder.selectable {
                Style::new().fg(c.text)
            } else {
                Style::new().fg(c.dim)
            };
            (1 + depth, icon, folder_label(folder), count, style)
        }
        Row::Add => (
            0,
            "+",
            "Add an account".to_string(),
            0,
            Style::new().fg(c.dim),
        ),
    };
    let current = match (row, &app.place) {
        (Row::AllInboxes, Place::AllInboxes) => true,
        (Row::Folder(i, f, _), Place::Folder { account, folder }) => {
            i == *account && app.accounts[i].folders[f].name == *folder
        }
        _ => false,
    };
    if current {
        style = style.fg(c.accent).add_modifier(Modifier::BOLD);
    } else if count > 0 && matches!(row, Row::Folder(..) | Row::AllInboxes) {
        style = style.add_modifier(Modifier::BOLD);
    }
    let syncing = matches!(row, Row::Folder(i, f, _) if app.accounts[i].syncing.contains(&app.accounts[i].folders[f].name));
    let right = if syncing {
        format!(" {}", icons.loading)
    } else if count > 0 {
        format!(" {count}")
    } else {
        String::new()
    };
    let lead = format!("{}{icon} ", "  ".repeat(indent));
    // A column for the cursor's bar, one before the right edge.
    let room = width.saturating_sub(1 + cols(&lead) + cols(&right) + 1);
    let label = fit(&label, room);
    let gap = room.saturating_sub(cols(&label));
    let bar = if selected && focused {
        Span::styled("▌", Style::new().fg(c.accent))
    } else {
        Span::raw(" ")
    };
    let mut line = Line::from(vec![
        bar,
        Span::styled(
            lead,
            style
                .remove_modifier(Modifier::BOLD)
                .fg(if current { c.accent } else { c.subtext }),
        ),
        Span::styled(label, style),
        Span::raw(" ".repeat(gap)),
        Span::styled(
            right,
            Style::new().fg(if current { c.accent } else { c.subtext }),
        ),
        Span::raw(" "),
    ]);
    if selected {
        line = line.style(Style::new().bg(c.selected));
    }
    line
}
