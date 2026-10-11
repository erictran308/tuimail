//! The message list: a row per message, newest first; unread in bold with
//! a dot, starred with a star.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::{border, bordered, fit, pad, scrollbar};
use crate::app::{App, Focus, Item, Place, plural};
use crate::mail;
use crate::store::Role;

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let c = &app.colors;
    let focused = app.focus == Focus::List;
    let (title, right) = titles(app);
    let width = usize::from(area.width.saturating_sub(4));
    let right = fit(&right, width / 2);
    let title = fit(&title, width.saturating_sub(right.width() + 2));
    let block = bordered()
        .border_style(border(focused, c))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ))
        .title(
            Line::from(Span::styled(
                format!(" {right} "),
                Style::new().fg(c.subtext),
            ))
            .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = usize::from(inner.height);
    app.list_rows.set(inner.height);
    if app.items.is_empty() {
        let text = empty_text(app);
        let lines = vec![
            Line::default(),
            Line::from(Span::styled(format!("  {text}"), Style::new().fg(c.dim))),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    let mut scroll = app.list_scroll.get().min(app.selected);
    if app.selected >= scroll + height {
        scroll = app.selected + 1 - height;
    }
    app.list_scroll.set(scroll);
    let now = crate::app::now();
    let width = usize::from(inner.width);
    let lines: Vec<Line> = app
        .items
        .iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(i, item)| row(app, item, i == app.selected, focused, width, now))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    scrollbar(
        frame,
        area,
        app.items.len().saturating_sub(height),
        scroll,
        c,
    );
}

/// The list's title (where it is) and what's on the right (how many).
fn titles(app: &App) -> (String, String) {
    if let Some(query) = &app.search {
        let found = plural(app.items.len(), "message");
        return (format!("{} “{}”", app.icons.search, query), found);
    }
    match &app.place {
        Place::AllInboxes => {
            let unread = app.unread_inboxes();
            (
                "All inboxes".into(),
                unread_text(unread, app.items.len() as u32),
            )
        }
        Place::Folder { account, folder } => {
            let Some(a) = app.accounts.get(*account) else {
                return (String::new(), String::new());
            };
            let name = app.folder_name(*account, folder);
            let f = a.folders.iter().find(|f| &f.name == folder);
            let (unread, total) = f.map_or((0, app.items.len() as u32), |f| {
                (f.unread, f.total.max(app.items.len() as u32))
            });
            (
                format!("{name} · {}", a.account.shown_name()),
                unread_text(unread, total),
            )
        }
    }
}

fn unread_text(unread: u32, total: u32) -> String {
    let total = plural(total as usize, "message");
    if unread > 0 {
        format!("{unread} unread · {total}")
    } else {
        total
    }
}

fn empty_text(app: &App) -> String {
    if app.search.is_some() {
        return "Nothing found. Esc ends the search.".into();
    }
    if app.accounts.is_empty() {
        return "No account yet: press + to add one.".into();
    }
    let checking = match &app.place {
        Place::Folder { account, folder } => app
            .accounts
            .get(*account)
            .is_some_and(|a| a.syncing.contains(folder)),
        Place::AllInboxes => app.accounts.iter().any(|a| !a.syncing.is_empty()),
    };
    if checking {
        "Checking for mail…".into()
    } else {
        "No messages here.".into()
    }
}

fn row(
    app: &App,
    item: &Item,
    selected: bool,
    focused: bool,
    width: usize,
    now: i64,
) -> Line<'static> {
    let c = &app.colors;
    let icons = &app.icons;
    let m = &item.message;
    let unread = !m.flags.seen;
    let marked = app.marked.contains(&item.key());
    let role = app
        .accounts
        .get(item.account)
        .and_then(|a| a.folders.iter().find(|f| f.name == m.folder))
        .and_then(|f| f.role);
    // In Sent and Drafts, who it's to matters.
    let who = if matches!(role, Some(Role::Sent | Role::Drafts)) {
        let to = m.summary.to.split(',').next().unwrap_or("").trim();
        let to = to.split('<').next().unwrap_or(to).trim().trim_matches('"');
        format!("To {}", if to.is_empty() { "?" } else { to })
    } else {
        m.summary.from.short().to_string()
    };
    let date = mail::short_date(m.summary.date, now);
    let mark = if marked {
        Span::styled("✓ ", Style::new().fg(c.accent).add_modifier(Modifier::BOLD))
    } else if unread {
        Span::styled(format!("{} ", icons.unread), Style::new().fg(c.accent))
    } else {
        Span::raw("  ")
    };
    let flag = if m.flags.flagged {
        Span::styled(format!("{} ", icons.flagged), Style::new().fg(c.flagged))
    } else if m.flags.answered {
        Span::styled(format!("{} ", icons.replied), Style::new().fg(c.dim))
    } else {
        Span::raw("  ")
    };
    let clip = if m.summary.attachments {
        Span::styled(format!("{} ", icons.attachment), Style::new().fg(c.subtext))
    } else {
        Span::raw("  ")
    };
    let fixed = 1 + 2 + 2 + 2 + 1 + 11;
    let rest = width.saturating_sub(fixed);
    let who_width = (rest / 3).clamp(8, 26).min(rest);
    let text_width = rest.saturating_sub(who_width + 1);
    let strong = |style: Style| {
        if unread {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    };
    let who_style = strong(Style::new().fg(if unread { c.text } else { c.subtext }));
    let subject_style = strong(Style::new().fg(if unread { c.text } else { c.subtext }));
    // Search results from other folders say which.
    let folder = if app.search.is_some()
        || matches!(app.place, Place::AllInboxes) && app.accounts.len() > 1
    {
        let label = match app.place {
            Place::AllInboxes => app
                .accounts
                .get(item.account)
                .map(|a| a.account.shown_name())
                .unwrap_or_default(),
            _ => app.folder_name(item.account, &m.folder),
        };
        format!("[{}] ", fit(&label, 10))
    } else {
        String::new()
    };
    let subject = if m.summary.subject.is_empty() {
        "(no subject)".to_string()
    } else {
        m.summary.subject.clone()
    };
    let folder_width = folder.width().min(text_width);
    let folder = fit(&folder, folder_width);
    let subject = fit(&subject, text_width.saturating_sub(folder.width()));
    let mut spare = text_width.saturating_sub(folder.width() + subject.width());
    let preview = if spare > 4 && !m.preview.is_empty() {
        let p = fit(&format!(" · {}", m.preview), spare);
        spare -= p.width();
        p
    } else {
        String::new()
    };
    let bar = if selected && focused {
        Span::styled("▌", Style::new().fg(c.accent))
    } else {
        Span::raw(" ")
    };
    let mut line = Line::from(vec![
        bar,
        mark,
        flag,
        clip,
        Span::styled(pad(&who, who_width), who_style),
        Span::raw(" "),
        Span::styled(folder, Style::new().fg(c.dim)),
        Span::styled(subject, subject_style),
        Span::styled(preview, Style::new().fg(c.dim)),
        Span::raw(" ".repeat(spare)),
        Span::styled(
            format!(" {:>9} ", fit(&date, 9)),
            Style::new().fg(if unread { c.text } else { c.dim }),
        ),
    ]);
    if selected {
        line = line.style(Style::new().bg(c.selected));
    } else if marked {
        line = line.style(Style::new().bg(c.panel));
    }
    line
}

#[cfg(test)]
mod tests {
    use crate::app::Focus;
    use crate::app::tests::{app, with_mail};
    use crate::ui::tests::{drawn, screen};

    #[tokio::test]
    async fn messages_show_newest_first_unread_in_bold() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        let shown = screen(&mut app, 160, 30);
        assert!(shown.contains("Inbox · work"), "{shown}");
        let alice = shown
            .lines()
            .position(|l| l.contains("Lunch tomorrow?"))
            .expect(&shown);
        let bob = shown
            .lines()
            .position(|l| l.contains("Quarterly numbers"))
            .expect(&shown);
        assert!(alice < bob, "newest first");
        let buffer = drawn(&mut app, 160, 30);
        let line = shown.lines().nth(alice).unwrap();
        let x = line.chars().take_while(|&ch| ch != 'L').count() as u16;
        let cell = &buffer[(x, alice as u16)];
        assert!(
            cell.modifier.contains(ratatui::style::Modifier::BOLD),
            "unread"
        );
        assert!(shown.contains("2 unread"), "{shown}");
    }
}
