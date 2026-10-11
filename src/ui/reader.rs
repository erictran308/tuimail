//! The message being read: its subject and who it's from and to, its
//! attachments, then its text, scrolled.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::{border, bordered, fit, hint_spans, scrollbar};
use crate::app::{App, Focus};
use crate::mail::{self, Role};

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors.clone();
    let focused = app.focus == Focus::Reader;
    let mut block = bordered().border_style(border(focused, &c));
    if focused {
        block = block.title_bottom(
            Line::from(hint_spans(
                " `o` links · `A` files · `J K` next/previous ",
                &c,
            ))
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // One column of margin each side.
    let text_area = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    app.reader_width
        .set(text_area.width.saturating_sub(1).max(20));
    app.reader_rows.set(inner.height);
    let Some(reader) = app.reader.as_ref() else {
        let lines = vec![
            Line::default(),
            Line::from(Span::styled(
                "  Enter opens the message under the cursor",
                Style::new().fg(c.dim),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    };
    let width = usize::from(text_area.width);
    let m = &reader.message;
    let mut lines: Vec<Line> = Vec::new();
    // The subject, wrapped.
    let subject = if m.summary.subject.is_empty() {
        "(no subject)".to_string()
    } else {
        m.summary.subject.clone()
    };
    for part in textwrap::wrap(&subject, width.max(10)) {
        lines.push(Line::from(Span::styled(
            part.into_owned(),
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::default());
    let label = |name: &str| Span::styled(format!("{name:<6}"), Style::new().fg(c.subtext));
    let parsed = reader.parsed.as_ref();
    let from = parsed
        .and_then(|p| p.from.first().cloned())
        .unwrap_or_else(|| m.summary.from.clone());
    lines.push(Line::from(vec![
        label("From"),
        Span::styled(
            fit(from.short(), width.saturating_sub(6)),
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if from.name.is_empty() {
                String::new()
            } else {
                fit(
                    &format!(" <{}>", from.addr),
                    width.saturating_sub(6 + from.short().width()),
                )
            },
            Style::new().fg(c.subtext),
        ),
    ]));
    let address_line = |name: &str, list: &str, lines: &mut Vec<Line>| {
        if list.is_empty() {
            return;
        }
        let wrapped = textwrap::wrap(list, width.saturating_sub(6).max(10));
        for (i, part) in wrapped.iter().take(4).enumerate() {
            lines.push(Line::from(vec![
                label(if i == 0 { name } else { "" }),
                Span::styled(part.to_string(), Style::new().fg(c.text)),
            ]));
        }
        if wrapped.len() > 4 {
            lines.push(Line::from(vec![
                label(""),
                Span::styled(
                    format!("and more ({} lines)", wrapped.len() - 4),
                    Style::new().fg(c.dim),
                ),
            ]));
        }
    };
    address_line("To", &m.summary.to, &mut lines);
    address_line("Cc", &m.summary.cc, &mut lines);
    if let Some(reply_to) = parsed.and_then(|p| p.reply_to.first())
        && !reply_to.addr.eq_ignore_ascii_case(&from.addr)
    {
        address_line("Reply", &reply_to.display(), &mut lines);
    }
    let date = mail::long_date(m.summary.date);
    if !date.is_empty() {
        lines.push(Line::from(vec![
            label("Date"),
            Span::styled(date, Style::new().fg(c.text)),
        ]));
    }
    if let Some(parsed) = parsed
        && !parsed.attachments.is_empty()
    {
        let names: Vec<String> = parsed
            .attachments
            .iter()
            .map(|a| format!("{} ({})", a.name, mail::size(a.size)))
            .collect();
        let text = format!("{} {}", app.icons.attachment, names.join(", "));
        for (i, part) in textwrap::wrap(&text, width.saturating_sub(6).max(10))
            .iter()
            .take(3)
            .enumerate()
        {
            lines.push(Line::from(vec![
                label(if i == 0 { "Files" } else { "" }),
                Span::styled(part.to_string(), Style::new().fg(c.subtext)),
            ]));
        }
    }
    lines.push(Line::from(Span::styled(
        "─".repeat(width),
        Style::new().fg(c.border),
    )));
    match (&reader.body, &reader.error) {
        (_, Some(error)) => {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::new().fg(c.red),
            )));
        }
        (Some(body), None) => {
            if body.from_html {
                lines.push(Line::from(Span::styled(
                    fit("HTML as text, nothing fetched · H plain text", width),
                    Style::new().fg(c.dim),
                )));
                lines.push(Line::default());
            }
            for segs in &body.lines {
                let spans: Vec<Span> = segs
                    .iter()
                    .map(|seg| {
                        let style = match seg.role {
                            Role::Text => Style::new().fg(c.text),
                            Role::Quote(depth) if depth % 2 == 1 => Style::new().fg(c.quote),
                            Role::Quote(_) => Style::new().fg(c.quote_deeper),
                            Role::Link(_) => {
                                Style::new().fg(c.link).add_modifier(Modifier::UNDERLINED)
                            }
                            Role::Bold => Style::new().fg(c.text).add_modifier(Modifier::BOLD),
                            Role::Italic => Style::new().fg(c.text).add_modifier(Modifier::ITALIC),
                            Role::Code => Style::new().fg(c.ok),
                            Role::Note => Style::new().fg(c.dim),
                        };
                        Span::styled(seg.text.clone(), style)
                    })
                    .collect();
                lines.push(Line::from(spans));
            }
        }
        (None, None) => {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!("{} Downloading…", app.icons.loading),
                Style::new().fg(c.dim),
            )));
        }
    }
    let rows = usize::from(inner.height);
    let max = lines.len().saturating_sub(rows);
    if let Some(reader) = app.reader.as_mut() {
        reader.scroll = reader.scroll.min(max);
        let scroll = reader.scroll;
        // Lines wider than the pane (a long word, a table) are cut, not
        // wrapped: the layout was made for this width.
        frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), text_area);
        scrollbar(frame, area, max, scroll, &c);
    }
}

#[cfg(test)]
mod tests {
    use crate::app::Focus;
    use crate::app::tests::{app, open_first, with_mail};
    use crate::ui::tests::screen;

    #[tokio::test]
    async fn a_message_shows_its_headers_then_its_text() {
        let mut app = app();
        with_mail(&mut app);
        app.focus = Focus::List;
        screen(&mut app, 160, 40);
        open_first(&mut app).await;
        let shown = screen(&mut app, 160, 40);
        assert!(
            shown.contains("From  Alice Rivera <alice@example.com>"),
            "{shown}"
        );
        assert!(
            shown.contains("To    Sam Taylor <me@example.org>"),
            "{shown}"
        );
        assert!(shown.contains("Are we still on for noon?"), "{shown}");
        assert!(shown.contains("menu.pdf"), "the attachment: {shown}");
        // Narrow: the reader takes the window, then Esc goes back.
        let shown = screen(&mut app, 60, 30);
        assert!(shown.contains("Are we still on"), "{shown}");
    }
}
