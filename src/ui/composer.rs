//! The composer: From, To, Cc, Bcc and Subject on a row each, the files
//! to send, then the text; addresses offered under the field being typed.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::{bordered, fit, hint_spans};
use crate::app::App;
use crate::composer::{Field, Kind};
use crate::mail;

/// The labels' column.
const LABEL: u16 = 9;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors.clone();
    let Some(composer) = app.composer.as_ref() else {
        return;
    };
    let title = match composer.kind {
        Kind::New => " New message ",
        Kind::Reply => " Reply ",
        Kind::Forward => " Forward ",
        Kind::Draft => " Draft ",
    };
    let hint = if composer.sending {
        " Sending… ".to_string()
    } else {
        " `Ctrl-s` send · `Ctrl-d` save draft · `Ctrl-o` attach · `Ctrl-e` editor · `Esc` close "
            .to_string()
    };
    let block = bordered()
        .border_style(Style::new().fg(c.accent))
        .title(Span::styled(
            format!(" {} {}", app.icons.compose, title.trim_start()),
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(hint_spans(&hint, &c)).right_aligned());
    let inner = block.inner(area).inner(Margin::new(1, 0));
    frame.render_widget(block, area);
    let account = app
        .accounts
        .get(composer.account)
        .map(|a| a.account.clone());
    let several = app.accounts.len() > 1;
    let Some(composer) = app.composer.as_mut() else {
        return;
    };
    let fields = composer.fields();
    let mut y = inner.y;
    let bottom = inner.y + inner.height;
    let mut field_rows: Vec<(Field, Rect)> = Vec::new();
    for field in fields.iter().copied().filter(|f| *f != Field::Body) {
        if y >= bottom {
            break;
        }
        let row = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: 1,
        };
        let focused = composer.field == field;
        let label_style = if focused {
            Style::new().fg(c.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(c.subtext)
        };
        frame.render_widget(
            Paragraph::new(Span::styled(field.label(), label_style)),
            Rect {
                width: LABEL.min(row.width),
                ..row
            },
        );
        let value = Rect {
            x: row.x + LABEL.min(row.width),
            width: row.width.saturating_sub(LABEL),
            ..row
        };
        let bg = if focused { c.selected } else { c.bg };
        match field {
            Field::From => {
                let text = account
                    .as_ref()
                    .map(|a| {
                        let name = a.display_name.trim();
                        if name.is_empty() {
                            a.email.clone()
                        } else {
                            format!("{name} <{}>", a.email)
                        }
                    })
                    .unwrap_or_default();
                let switch = if several && focused {
                    "  ← → another account"
                } else {
                    ""
                };
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(
                            fit(&text, usize::from(value.width)),
                            Style::new().fg(c.text),
                        ),
                        Span::styled(switch, Style::new().fg(c.dim)),
                    ]))
                    .style(Style::new().bg(bg)),
                    value,
                );
            }
            Field::Attachments => {
                let mut spans = Vec::new();
                for (i, a) in composer.attachments.iter().enumerate() {
                    let chosen = focused && i == composer.attachment;
                    let style = if chosen {
                        Style::new().fg(c.bg).bg(c.accent)
                    } else {
                        Style::new().fg(c.text)
                    };
                    spans.push(Span::styled(
                        format!(
                            " {} {} ({}) ",
                            app.icons.attachment,
                            a.name(),
                            mail::size(a.size() as usize)
                        ),
                        style,
                    ));
                    spans.push(Span::raw(" "));
                }
                if focused {
                    spans.push(Span::styled("x takes one off", Style::new().fg(c.dim)));
                }
                frame.render_widget(
                    Paragraph::new(Line::from(spans)).style(Style::new().bg(bg)),
                    value,
                );
            }
            _ => {
                let input = match field {
                    Field::To => &mut composer.to,
                    Field::Cc => &mut composer.cc,
                    Field::Bcc => &mut composer.bcc,
                    _ => &mut composer.subject,
                };
                input.set_style(Style::new().fg(c.text).bg(bg));
                input.set_cursor_style(if focused && !composer.sending {
                    Style::new().fg(c.bg).bg(c.accent)
                } else {
                    Style::new().fg(c.text).bg(bg)
                });
                frame.render_widget(&*input, value);
            }
        }
        field_rows.push((field, value));
        y += 1;
    }
    // A rule, then the text.
    if y < bottom {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "─".repeat(usize::from(inner.width)),
                Style::new().fg(c.border),
            )),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
        );
        y += 1;
    }
    let body = Rect {
        x: inner.x,
        y,
        width: inner.width,
        height: bottom.saturating_sub(y),
    };
    let focused = composer.field == Field::Body;
    composer.body.set_style(Style::new().fg(c.text).bg(c.bg));
    composer
        .body
        .set_cursor_style(if focused && !composer.sending {
            Style::new().fg(c.bg).bg(c.accent)
        } else {
            Style::new().fg(c.text).bg(c.bg)
        });
    if body.height > 0 {
        frame.render_widget(&composer.body, body);
    }
    // Suggestions under the address being typed.
    if !composer.suggestions.is_empty()
        && let Some((_, rect)) = field_rows.iter().find(|(f, _)| *f == composer.field)
    {
        let widest = composer
            .suggestions
            .iter()
            .map(|a| a.display().width())
            .max()
            .unwrap_or(0);
        let width = (widest as u16 + 4).min(rect.width).max(10);
        let height =
            (composer.suggestions.len() as u16 + 2).min(area.height.saturating_sub(rect.y + 1));
        let popup = Rect {
            x: rect.x,
            y: rect.y + 1,
            width,
            height,
        };
        frame.render_widget(Clear, popup);
        let block = bordered()
            .border_style(Style::new().fg(c.accent))
            .style(Style::new().bg(c.panel));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let lines: Vec<Line> = composer
            .suggestions
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let style = if i == composer.suggestion {
                    Style::new().fg(c.text).bg(c.selected)
                } else {
                    Style::new().fg(c.text)
                };
                Line::from(Span::styled(
                    fit(&format!(" {}", a.display()), usize::from(inner.width)),
                    style,
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::tests::{app, with_mail};
    use crate::ui::tests::screen;

    #[tokio::test]
    async fn the_composer_shows_its_fields_and_the_files() {
        let mut app = app();
        with_mail(&mut app);
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        let shown = screen(&mut app, 120, 30);
        assert!(shown.contains("New message"), "{shown}");
        assert!(
            shown.contains("From     Sam Taylor <me@example.org>"),
            "{shown}"
        );
        assert!(shown.contains("Subject"));
        assert!(shown.contains("Ctrl-s send"));
        for c in "bob".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let shown = screen(&mut app, 120, 30);
        assert!(
            shown.contains("Bob Stone <bob@example.org>"),
            "a suggestion: {shown}"
        );
        for width in [10, 40, 80] {
            screen(&mut app, width, 8);
        }
    }
}
