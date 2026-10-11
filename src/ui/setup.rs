//! The form for adding an account.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::{bordered, centered, hint_spans};
use crate::account::{Protocol, Security};
use crate::app::App;
use crate::setup::{ROWS, Row};

const LABEL: u16 = 18;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors.clone();
    let first = app.accounts.is_empty();
    let Some(setup) = app.setup.as_mut() else {
        return;
    };
    let rect = centered(area, 76, (ROWS.len() as u16) + 10);
    frame.render_widget(Clear, rect);
    let hint = if setup.checking {
        " Logging in to check… ".to_string()
    } else {
        " `Tab` next · `Ctrl-s` check and save · `Esc` cancel ".to_string()
    };
    let block = bordered()
        .border_style(Style::new().fg(c.accent))
        .title(Span::styled(
            format!(" {} Add an account ", app.icons.logo),
            Style::new().fg(c.text).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(hint_spans(&hint, &c)).right_aligned())
        .style(Style::new().bg(c.panel));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let width = inner.width.saturating_sub(2);
    let x = inner.x + 1;
    let mut y = inner.y;
    let intro = if first {
        "Welcome to tuimail. Your address and password first: the servers fill in."
    } else {
        "Your address and password: the servers fill in."
    };
    let line = |frame: &mut Frame, y: u16, spans: Vec<Span<'static>>| {
        if y < inner.y + inner.height {
            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect {
                    x,
                    y,
                    width,
                    height: 1,
                },
            );
        }
    };
    line(
        frame,
        y,
        vec![Span::styled(intro, Style::new().fg(c.subtext))],
    );
    y += 2;
    for (i, row) in ROWS.iter().enumerate() {
        if y >= inner.y + inner.height {
            break;
        }
        let focused = i == setup.row;
        if *row == Row::Protocol || *row == Row::SmtpHost {
            y += 1;
        }
        let label_style = if focused {
            Style::new().fg(c.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(c.subtext)
        };
        line(frame, y, vec![Span::styled(row.label(), label_style)]);
        let value = Rect {
            x: x + LABEL,
            y,
            width: width.saturating_sub(LABEL),
            height: 1,
        };
        let bg = if focused { c.selected } else { c.panel };
        if row.is_choice() {
            let text = match row {
                Row::Protocol => match setup.protocol {
                    Protocol::Imap => "IMAP (mail stays on the server)",
                    Protocol::Pop3 => "POP3 (mail is downloaded)",
                },
                Row::Security => security(setup.security),
                _ => security(setup.smtp_security),
            };
            let arrows = if focused {
                ("‹ ", " ›")
            } else {
                ("  ", "  ")
            };
            if y < inner.y + inner.height {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(arrows.0, Style::new().fg(c.accent)),
                        Span::styled(text, Style::new().fg(c.text)),
                        Span::styled(arrows.1, Style::new().fg(c.accent)),
                    ]))
                    .style(Style::new().bg(bg)),
                    value,
                );
            }
        } else if let checking = setup.checking
            && let Some(input) = setup_input(setup, *row)
        {
            input.set_style(Style::new().fg(c.text).bg(bg));
            input.set_cursor_style(if focused && !checking {
                Style::new().fg(c.bg).bg(c.accent)
            } else {
                Style::new().fg(c.text).bg(bg)
            });
            let placeholder = match row {
                Row::Name => "as recipients see it",
                Row::Username => "if not the address",
                _ => "",
            };
            input.set_placeholder_text(placeholder);
            input.set_placeholder_style(Style::new().fg(c.dim).bg(bg));
            if y < inner.y + inner.height {
                frame.render_widget(&*input, value);
            }
        }
        y += 1;
    }
    y += 1;
    if let Some(error) = &setup.error {
        for part in textwrap::wrap(error, usize::from(width)) {
            line(
                frame,
                y,
                vec![Span::styled(part.into_owned(), Style::new().fg(c.red))],
            );
            y += 1;
        }
    } else if !setup.note.is_empty() {
        for part in textwrap::wrap(&setup.note, usize::from(width)) {
            line(
                frame,
                y,
                vec![Span::styled(part.into_owned(), Style::new().fg(c.warning))],
            );
            y += 1;
        }
    }
}

fn setup_input(
    setup: &mut crate::setup::Setup,
    row: Row,
) -> Option<&mut ratatui_textarea::TextArea<'static>> {
    Some(match row {
        Row::Email => &mut setup.email,
        Row::Password => &mut setup.password,
        Row::Name => &mut setup.name,
        Row::Host => &mut setup.host,
        Row::Port => &mut setup.port,
        Row::SmtpHost => &mut setup.smtp_host,
        Row::SmtpPort => &mut setup.smtp_port,
        Row::Username => &mut setup.username,
        _ => return None,
    })
}

fn security(s: Security) -> &'static str {
    match s {
        Security::Tls => "TLS",
        Security::Starttls => "STARTTLS",
        Security::None => "none (this computer only)",
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::tests::app;
    use crate::setup::Setup;
    use crate::ui::tests::screen;

    #[tokio::test]
    async fn the_form_fills_in_the_servers_for_an_address() {
        let mut app = app();
        app.setup = Some(Setup::default());
        for c in "me@fastmail.com".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let shown = screen(&mut app, 100, 30);
        assert!(shown.contains("Add an account"), "{shown}");
        assert!(shown.contains("imap.fastmail.com"), "{shown}");
        assert!(shown.contains("smtp.fastmail.com"));
        assert!(shown.contains("app password"));
        for c in "secret".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let shown = screen(&mut app, 100, 30);
        assert!(!shown.contains("secret"), "the password is hidden");
        assert!(shown.contains("••••••"));
    }
}
