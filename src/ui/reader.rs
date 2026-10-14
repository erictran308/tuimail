//! The message being read: its subject and who it's from and to, its
//! attachments, then its text, scrolled.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{border, bordered, fit, fit_middle, hint_spans, scrollbar};
use crate::app::{App, Focus};
use crate::mail::{self, Addr, Role};
use crate::text::{cols, wrap};
use crate::theme::Colors;

/// A sender as the reader shows it: the address always in full (cut in
/// the middle, so its domain shows, if even it doesn't fit), then the name
/// the sender chose in what room is left, quoted when it could pass for an
/// address. A name can never push the address out of view.
fn sender<'a>(label: Span<'a>, from: &Addr, room: usize, c: &Colors) -> Line<'a> {
    let named = !from.name.is_empty() && from.name != from.addr;
    if !named {
        return Line::from(vec![
            label,
            Span::styled(
                fit_middle(&from.addr, room),
                Style::new().fg(c.text).add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    let addr = format!("<{}>", fit_middle(&from.addr, room.saturating_sub(2)));
    let shown = from.display();
    let name = shown
        .strip_suffix(&format!(" <{}>", from.addr))
        .unwrap_or(&from.name)
        .to_string();
    let name_room = room.saturating_sub(cols(&addr) + 1);
    let mut spans = vec![label];
    if name_room >= 4 {
        spans.push(Span::styled(
            fit(&name, name_room),
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(addr, Style::new().fg(c.text)));
    Line::from(spans)
}

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
    for part in wrap(&subject, width.max(10)) {
        lines.push(Line::from(Span::styled(
            part.to_string(),
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::default());
    let label = |name: &str| Span::styled(format!("{name:<6}"), Style::new().fg(c.subtext));
    let parsed = reader.parsed.as_ref();
    let froms: Vec<Addr> = match parsed {
        Some(p) if !p.from.is_empty() => p.from.clone(),
        _ => vec![m.summary.from.clone()],
    };
    for (i, from) in froms.iter().take(4).enumerate() {
        let name = if i == 0 { "From" } else { "" };
        lines.push(sender(label(name), from, width.saturating_sub(6), &c));
    }
    if froms.len() > 4 {
        lines.push(Line::from(vec![
            label(""),
            Span::styled(
                format!("and {} more", froms.len() - 4),
                Style::new().fg(c.dim),
            ),
        ]));
    }
    let address_line = |name: &str, list: &str, lines: &mut Vec<Line>| {
        if list.is_empty() {
            return;
        }
        let wrapped = wrap(list, width.saturating_sub(6).max(10));
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
    // Where a reply goes when that isn't only the sender: every address,
    // those that aren't the sender's first, without the names (which could
    // pass for addresses, or push one out of view).
    if let Some(p) = parsed {
        let theirs = |a: &Addr| froms.iter().any(|f| f.addr.eq_ignore_ascii_case(&a.addr));
        let (same, other): (Vec<&Addr>, Vec<&Addr>) = p.reply_to.iter().partition(|a| theirs(a));
        if !other.is_empty() {
            let mut seen: Vec<String> = Vec::new();
            for a in other.iter().chain(&same) {
                let addr = a.addr.to_lowercase();
                if !seen.contains(&addr) {
                    seen.push(addr);
                }
            }
            address_line("Reply", &seen.join(", "), &mut lines);
        }
    }
    let date = mail::long_date(m.summary.date);
    if !date.is_empty() {
        lines.push(Line::from(vec![
            label("Date"),
            Span::styled(date, Style::new().fg(c.text)),
        ]));
    }
    if !m.flags.tags.is_empty() {
        let text: Vec<String> = m.flags.tags.iter().map(|t| format!("#{t}")).collect();
        for (i, part) in wrap(&text.join(" "), width.saturating_sub(6).max(10))
            .iter()
            .take(2)
            .enumerate()
        {
            lines.push(Line::from(vec![
                label(if i == 0 { "Tags" } else { "" }),
                Span::styled(part.to_string(), Style::new().fg(c.tag)),
            ]));
        }
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
        for (i, part) in wrap(&text, width.saturating_sub(6).max(10))
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
    use crate::store::Flags;
    use crate::ui::tests::screen;

    /// The reader on a message with these headers, at 160 columns.
    async fn reading(headers: &str) -> String {
        let mut app = app();
        with_mail(&mut app);
        let id = app.accounts[0].id.clone();
        let raw = format!(
            "{headers}\r\nTo: me@example.org\r\nSubject: Account review\r\n\
             Date: Mon, 1 Jan 2035 00:00:00 +0000\r\nMessage-ID: <x1@evil.example>\r\n\r\nhi\r\n"
        );
        app.store
            .add_local(&id, "INBOX", raw.as_bytes(), Flags::default(), None)
            .unwrap();
        app.reload();
        app.focus = Focus::List;
        screen(&mut app, 160, 40);
        open_first(&mut app).await;
        screen(&mut app, 160, 40)
    }

    #[tokio::test]
    async fn a_long_sender_name_never_hides_the_address() {
        let shown = reading(
            "From: \"service@paypal.com <service@paypal.com> on behalf of PayPal Account \
             Review Department, case 4471-2209-118\" <attacker@evil.example>",
        )
        .await;
        let row = shown.lines().find(|l| l.contains("From")).unwrap();
        assert!(row.contains("<attacker@evil.example>"), "{row}");

        // A name that could pass for an address is quoted, as in To and Cc.
        let shown = reading("From: \"PayPal <service@paypal.com>\" <attacker2@evil.example>").await;
        assert!(
            shown.contains("\"PayPal <service@paypal.com>\" <attacker2@evil.example>"),
            "{shown}"
        );

        // A long address keeps its domain.
        let shown = reading(
            "From: <service@paypal.com.account-verification-secure-login-center-7781-\
             and-some-more-padding.evil.example>",
        )
        .await;
        let row = shown.lines().find(|l| l.contains("From")).unwrap();
        assert!(row.contains("evil.example"), "{row}");
    }

    #[tokio::test]
    async fn halfwidth_sound_marks_never_hide_the_address() {
        // No columns to unicode-width, one each as drawn: measured the
        // first way, the name would push the address out of the row.
        for name in [
            format!(
                "PayPal Security{}",
                format!(" {}", "\u{FF9E}".repeat(10)).repeat(6)
            ),
            format!("PayPal Security {}", "\u{FF76}\u{FF9E}".repeat(40)),
        ] {
            let shown = reading(&format!("From: \"{name}\" <attacker@evil.example>")).await;
            let row = shown.lines().find(|l| l.contains("From")).unwrap();
            assert!(row.contains("<attacker@evil.example>"), "{row}");
        }
    }

    #[tokio::test]
    async fn every_reply_destination_is_shown() {
        let long = format!("Alice Rivera, Finance, {}", "x".repeat(90));
        let shown = reading(&format!(
            "From: \"Alice Rivera\" <alice@example.com>\r\nReply-To: \"Alice Rivera\" \
             <alice@example.com>, <evil@evil.example>, \"{long}\" <alice@example.com>"
        ))
        .await;
        let row = shown.lines().find(|l| l.contains("Reply")).expect(&shown);
        assert!(row.contains("evil@evil.example"), "{row}");
    }

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

    #[tokio::test]
    async fn tags_show_under_the_date() {
        let mut app = app();
        with_mail(&mut app);
        let (id, uid) = (app.accounts[0].id.clone(), app.items[0].message.uid);
        let flags = Flags {
            tags: vec!["travel".into(), "work".into()],
            ..Flags::default()
        };
        app.store.set_flags(&id, "INBOX", &[(uid, flags)]).unwrap();
        app.reload();
        open_first(&mut app).await;
        let shown = screen(&mut app, 160, 40);
        let date = shown
            .lines()
            .position(|l| l.contains("Date  "))
            .expect(&shown);
        let tags = shown
            .lines()
            .position(|l| l.contains("Tags  #travel #work"))
            .expect(&shown);
        assert_eq!(tags, date + 1, "{shown}");
    }
}
