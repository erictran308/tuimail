//! Messages: their headers summed up for the list, their bodies turned into
//! lines to show (HTML through html2text, without fetching anything it
//! points to), their attachments, and the quoted text of a reply.
//!
//! Everything here comes from a message's sender, so all of it is cleaned
//! (`text::clean`) and capped before it's shown or stored.

use std::borrow::Cow;

use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders};

use crate::text;

/// The longest subject, name or address list kept, in characters.
pub const MAX_SUBJECT: usize = 400;
pub const MAX_NAME: usize = 120;
/// The longest address SMTP carries (RFC 5321's path less its brackets).
pub const MAX_ADDRESS: usize = 254;
pub const MAX_ADDRESSES: usize = 2000;
/// Message ids are kept up to this long, and at most this many references.
const MAX_ID: usize = 250;
const MAX_REFERENCES: usize = 50;
/// The most HTML turned into text; past it the rest is cut, and said so.
const MAX_HTML: usize = 2 << 20;
/// The deepest nesting of HTML laid out as such; deeper is shown as its
/// text alone. Real mail stays far below it, and html2text's time and
/// stack grow with it (2M characters of nesting took 15 s to lay out).
const MAX_DEPTH: usize = 2_000;
/// The most plain text shown.
const MAX_TEXT: usize = 4 << 20;

/// A name and an address, as in From or To.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Addr {
    pub name: String,
    pub addr: String,
}

impl Addr {
    /// "Name <addr>", or the address alone. A name with a comma, `@`,
    /// angle brackets or the like is quoted, so it reads back as one name
    /// and never as more addresses (`"Doe, John" <john@x.y>`).
    pub fn display(&self) -> String {
        // An address that isn't a plain one (a sender can put a `"` in it)
        // always goes in brackets, where a `"` quotes nothing.
        let plain = crate::account::valid_address(&self.addr);
        if self.name.is_empty() || self.name == self.addr {
            return if plain {
                self.addr.clone()
            } else {
                format!("<{}>", self.addr)
            };
        }
        // Look-alikes of the characters that make a name read as an
        // address (fullwidth ＜＞＠, single angle quotes) count too.
        let special = |c: char| "()<>[]:;@\\,.\"＜＞＠‹›«»".contains(c);
        if self.name.chars().any(special) {
            let escaped = self.name.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\" <{}>", self.addr)
        } else {
            format!("{} <{}>", self.name, self.addr)
        }
    }

    /// The name, else the address: what the list shows.
    pub fn short(&self) -> &str {
        if self.name.is_empty() {
            &self.addr
        } else {
            &self.name
        }
    }
}

/// What the list needs of a message, from its headers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// Without the angle brackets.
    pub message_id: String,
    pub in_reply_to: String,
    /// Ids, separated by spaces, oldest first.
    pub references: String,
    /// Unix seconds; 0 if the message has no date.
    pub date: i64,
    pub from: Addr,
    /// "Name <addr>, …" lists.
    pub to: String,
    pub cc: String,
    pub subject: String,
    /// The message is multipart/mixed: it likely has attachments.
    pub attachments: bool,
}

/// A whole message, parsed.
#[derive(Clone, Debug, Default)]
pub struct Parsed {
    pub summary: Summary,
    pub from: Vec<Addr>,
    pub reply_to: Vec<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    /// The plain-text bodies, joined.
    pub text: Option<String>,
    /// The text is format=flowed: lines ending in a space go on.
    pub flowed: bool,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
    /// List-Unsubscribe's https or mailto link, if any.
    pub unsubscribe: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    /// Its place among the message's attachments (mail-parser's).
    pub index: u32,
    /// A file name safe to save under.
    pub name: String,
    pub mime: String,
    pub size: usize,
}

fn addrs(address: Option<&Address>) -> Vec<Addr> {
    let Some(address) = address else {
        return Vec::new();
    };
    address
        .iter()
        .filter_map(|a| {
            // Whole or not at all: a cut address is someone else's.
            let addr = a.address().filter(|a| a.chars().count() <= MAX_ADDRESS)?;
            let addr = text::one_line(addr, MAX_ADDRESS);
            let name = text::one_line(a.name().unwrap_or(""), MAX_NAME);
            // Quotes around a name are its spelling, not part of it.
            let name = name.trim_matches(['"', '\'']).trim().to_string();
            Some(Addr { name, addr })
        })
        .take(500)
        .collect()
}

fn list(addrs: &[Addr]) -> String {
    let joined = addrs
        .iter()
        .map(Addr::display)
        .collect::<Vec<_>>()
        .join(", ");
    text::one_line(&joined, MAX_ADDRESSES)
}

/// Message ids from In-Reply-To or References, without brackets.
fn ids(value: &HeaderValue) -> Vec<String> {
    let list: Vec<Cow<str>> = match value {
        HeaderValue::Text(id) => vec![id.clone()],
        HeaderValue::TextList(ids) => ids.clone(),
        _ => Vec::new(),
    };
    list.iter()
        .map(|id| id.trim().trim_matches(['<', '>']).to_string())
        .filter(|id| valid_id(id))
        .collect()
}

/// A message id tuimail keeps and sends back in a reply's headers: no
/// spaces, brackets or control characters, so it can't break a header.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'<' | b'>' | b'"' | b'\\'))
}

fn summary(message: &mail_parser::Message) -> Summary {
    let from = addrs(message.from()).into_iter().next().unwrap_or_default();
    let mut references = ids(message.references());
    if references.len() > MAX_REFERENCES {
        references.drain(..references.len() - MAX_REFERENCES);
    }
    let attachments = message
        .content_type()
        .is_some_and(|ct| ct.ctype() == "multipart" && ct.subtype() == Some("mixed"));
    Summary {
        message_id: message
            .message_id()
            .map(|id| id.trim().trim_matches(['<', '>']).to_string())
            .filter(|id| valid_id(id))
            .unwrap_or_default(),
        in_reply_to: ids(message.in_reply_to()).pop().unwrap_or_default(),
        references: references.join(" "),
        date: message.date().map(|d| d.to_timestamp()).unwrap_or(0),
        from,
        to: list(&addrs(message.to())),
        cc: list(&addrs(message.cc())),
        subject: text::one_line(message.subject().unwrap_or(""), MAX_SUBJECT),
        attachments,
    }
}

/// The summary of a message from its headers (or the whole message).
pub fn summarize(raw: &[u8]) -> Summary {
    MessageParser::default()
        .parse(raw)
        .map(|m| summary(&m))
        .unwrap_or_default()
}

/// Parses a whole message; `None` if it isn't one at all.
pub fn parse(raw: &[u8]) -> Option<Parsed> {
    let message = MessageParser::default().parse(raw)?;
    let join = |parts: Vec<&str>| -> Option<String> {
        let joined = parts.join("\n\n");
        (!joined.trim().is_empty()).then_some(joined)
    };
    let mut flowed = false;
    let mut texts = Vec::new();
    for part in message.text_bodies() {
        if part.is_text_html() {
            continue;
        }
        if let Some(text) = part.text_contents() {
            flowed |= part
                .content_type()
                .and_then(|ct| ct.attribute("format"))
                .is_some_and(|f| f.eq_ignore_ascii_case("flowed"));
            texts.push(text);
        }
    }
    let htmls: Vec<&str> = message
        .html_bodies()
        .filter(|p| p.is_text_html())
        .filter_map(|p| p.text_contents())
        .collect();
    let attachments = message
        .attachments()
        .enumerate()
        .map(|(i, part)| {
            let mime = mime_of(part);
            Attachment {
                index: i as u32,
                name: part_name(part, &mime),
                mime: text::one_line(&mime, 100),
                size: part.len(),
            }
        })
        .take(500)
        .collect();
    let unsubscribe = unsubscribe_link(message.list_unsubscribe());
    Some(Parsed {
        summary: summary(&message),
        from: addrs(message.from()),
        reply_to: addrs(message.reply_to()),
        to: addrs(message.to()),
        cc: addrs(message.cc()),
        text: join(texts),
        flowed,
        html: join(htmls),
        attachments,
        unsubscribe,
    })
}

/// The first https or mailto link of List-Unsubscribe.
fn unsubscribe_link(value: &HeaderValue) -> Option<String> {
    let links: Vec<String> = match value {
        HeaderValue::Address(address) => address
            .iter()
            .filter_map(|a| a.address().map(str::to_string))
            .collect(),
        HeaderValue::Text(t) => vec![t.to_string()],
        HeaderValue::TextList(list) => list.iter().map(|t| t.to_string()).collect(),
        _ => Vec::new(),
    };
    links
        .into_iter()
        .flat_map(|l| {
            l.split(',')
                .map(|s| s.trim().trim_matches(['<', '>']).to_string())
                .collect::<Vec<_>>()
        })
        .find(|l| l.starts_with("https://") || l.starts_with("mailto:"))
        .filter(|l| l.len() < 2000 && usable_link(l))
}

/// The start of a message's text for the list. Lays out HTML, so it runs
/// in the background ([`in_background`]).
pub fn preview_of(raw: &[u8]) -> String {
    parse(raw)
        .map(|p| crate::store::preview(&plain_text(&p)))
        .unwrap_or_default()
}

/// An attachment's type, lower case: `image/png`.
fn mime_of(part: &mail_parser::MessagePart) -> String {
    part.content_type()
        .map(|ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub),
            None => ct.ctype().to_string(),
        })
        .unwrap_or_else(|| "application/octet-stream".into())
        .to_ascii_lowercase()
}

/// An attachment's safe file name: the one listed, saved and opened
/// alike, so what was checked before opening is what's opened.
fn part_name(part: &mail_parser::MessagePart, mime: &str) -> String {
    let name = match part.attachment_name() {
        Some(name) => name.to_string(),
        None if part.message().is_some() => "forwarded message.eml".into(),
        None if mime.starts_with("image/") => {
            format!("image.{}", mime.trim_start_matches("image/"))
        }
        None => "attachment".into(),
    };
    safe_file_name(&name)
}

/// The bytes of attachment `index`, and its safe file name.
pub fn attachment(raw: &[u8], index: u32) -> Option<(String, Vec<u8>)> {
    let message = MessageParser::default().parse(raw)?;
    let part = message.attachment(index)?;
    Some((part_name(part, &mime_of(part)), part.contents().to_vec()))
}

/// A file name from a sender, made safe to save under: no folders, no
/// control or bidi characters (`invoice\u{202E}fdp.exe`), no leading dot or
/// trailing dot and space, nothing Windows reserves, at most 120
/// characters with the extension kept.
pub fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut cleaned: String = text::one_line(base, 400)
        .chars()
        .map(|c| match c {
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    cleaned = cleaned
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' '])
        .to_string();
    // Windows takes a device name before the first dot, trailing spaces
    // and dots dropped (`CON .txt` is the console), with COM and LPT
    // numbered 0-9 or ¹²³.
    let stem = cleaned.split('.').next().unwrap_or("");
    let stem = stem.trim_end_matches([' ', '.']).to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|n| {
            let mut chars = n.chars();
            matches!(
                (chars.next(), chars.next()),
                (Some('0'..='9' | '¹' | '²' | '³'), None)
            )
        })
    };
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT");
    if cleaned.is_empty() || reserved {
        cleaned = format!("attachment {cleaned}").trim().to_string();
    }
    if cleaned.chars().count() > 120 {
        let ext: String = match cleaned.rsplit_once('.') {
            Some((_, ext)) if ext.chars().count() <= 10 => format!(".{ext}"),
            _ => String::new(),
        };
        let keep = 120 - ext.chars().count();
        cleaned = format!("{}{ext}", text::first_chars(&cleaned, keep));
    }
    cleaned
}

/// How a piece of a line is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Text,
    /// Quoted text, 1 for `>`, 2 for `> >`…
    Quote(u8),
    /// Link number n (into [`Body::links`]).
    Link(usize),
    Bold,
    Italic,
    Code,
    /// What tuimail adds: "[image: …]", "(cut here)".
    Note,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Seg {
    pub text: String,
    pub role: Role,
}

/// A message's text, laid out for a width.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Body {
    pub lines: Vec<Vec<Seg>>,
    /// The links in it, in order, each once: `o` lists them.
    pub links: Vec<String>,
    /// It was laid out from the HTML part.
    pub from_html: bool,
}

impl Body {
    /// What link `n` says in the message ("Try it", "Unsubscribe"), to tell
    /// links to one site apart in the `o` list. Empty when it says nothing,
    /// or when what it says looks like an address: the list shows the real
    /// one beside it, and a link's text can claim any site.
    pub fn link_text(&self, n: usize) -> String {
        let mut pieces: Vec<&str> = Vec::new();
        for seg in self.lines.iter().flatten() {
            if seg.role != Role::Link(n) {
                continue;
            }
            // html2text marks headings with #: not part of what it says.
            let piece = seg.text.trim().trim_start_matches(['#', ' ']).trim();
            if !piece.is_empty() && !pieces.contains(&piece) {
                pieces.push(piece);
            }
        }
        let said = text::one_line(&pieces.join(" "), 60);
        let lower = said.to_lowercase();
        let looks_like_address = lower.contains("://")
            || lower.starts_with("www.")
            || lower.starts_with("mailto:")
            || (!said.contains(' ') && (said.contains('.') || said.contains('@')));
        if looks_like_address {
            String::new()
        } else {
            said
        }
    }

    /// The number of link `url`; `None` past [`MAX_LINKS`].
    fn link(&mut self, url: &str) -> Option<usize> {
        match self.links.iter().position(|l| l == url) {
            Some(i) => Some(i),
            None if self.links.len() >= MAX_LINKS => None,
            None => {
                self.links.push(url.to_string());
                Some(self.links.len() - 1)
            }
        }
    }

    /// The text of line `i`, plain.
    #[cfg(test)]
    pub fn line(&self, i: usize) -> String {
        self.lines[i].iter().map(|s| s.text.as_str()).collect()
    }
}

/// The message's text laid out `width` columns wide: the plain-text part,
/// or the HTML turned into text when there's no plain one or `prefer_plain`
/// is off.
pub fn render(parsed: &Parsed, width: usize, prefer_plain: bool) -> Body {
    let width = width.clamp(20, 400);
    let use_html = match (&parsed.text, &parsed.html) {
        (Some(_), Some(_)) => !prefer_plain,
        (None, Some(_)) => true,
        _ => false,
    };
    let mut body = match (use_html, &parsed.html, &parsed.text) {
        (true, Some(html), _) => render_html(html, width),
        (_, _, Some(text)) => render_text(text, width, parsed.flowed),
        _ => Body::default(),
    };
    if body
        .lines
        .iter()
        .all(|l| l.iter().all(|s| s.text.trim().is_empty()))
    {
        body.lines = vec![vec![Seg {
            text: "(no text)".into(),
            role: Role::Note,
        }]];
    }
    body
}

/// Plain text: lines wrapped to `width`, quoted lines marked by depth,
/// links found and numbered.
pub fn render_text(source: &str, width: usize, flowed: bool) -> Body {
    let mut body = Body::default();
    let (source, cut) = match source.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => (&source[..i], true),
        None => (source, false),
    };
    let cleaned = text::clean(source).replace('\t', "    ");
    let lines: Vec<String> = if flowed {
        unflow(&cleaned)
    } else {
        cleaned.lines().map(str::to_string).collect()
    };
    for line in lines {
        let (depth, rest) = quote_depth(&line);
        let role = if depth > 0 {
            Role::Quote(depth)
        } else {
            Role::Text
        };
        let prefix = if depth > 0 {
            format!("{} ", ">".repeat(usize::from(depth)))
        } else {
            String::new()
        };
        let room = width.saturating_sub(prefix.chars().count()).max(10);
        if rest.trim().is_empty() {
            body.lines.push(if prefix.is_empty() {
                Vec::new()
            } else {
                vec![Seg {
                    text: prefix.trim_end().to_string(),
                    role,
                }]
            });
            continue;
        }
        // Links are found in the whole line, then the line is wrapped at
        // spaces only, so a link that wraps stays one link.
        let links = find_urls(rest, &mut body);
        let options = textwrap::Options::new(room)
            .break_words(true)
            .word_separator(textwrap::WordSeparator::AsciiSpace);
        for wrapped in textwrap::wrap(rest, options) {
            let mut segs = Vec::new();
            if !prefix.is_empty() {
                segs.push(Seg {
                    text: prefix.clone(),
                    role,
                });
            }
            match offset_in(rest, &wrapped) {
                Some(at) => segs.extend(split_links(&wrapped, at, &links, role)),
                None => segs.extend(linkify(&wrapped, role, &mut body)),
            }
            body.lines.push(segs);
        }
    }
    if cut {
        body.lines.push(vec![Seg {
            text: "(the rest of this message is too long to show)".into(),
            role: Role::Note,
        }]);
    }
    body
}

/// RFC 3676 format=flowed: a line ending in a space goes on in the next
/// (unless it's the signature line), and a leading space was stuffed.
fn unflow(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut open: Option<(u8, String)> = None;
    for raw in text.lines() {
        let (depth, rest) = quote_depth_strict(raw);
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        let soft = rest.ends_with(' ') && rest != "-- ";
        match open.take() {
            Some((d, mut acc)) if d == depth => {
                acc.push_str(rest);
                if soft {
                    open = Some((d, acc));
                } else {
                    out.push(requote(d, &acc));
                }
            }
            previous => {
                if let Some((d, acc)) = previous {
                    out.push(requote(d, &acc));
                }
                if soft {
                    open = Some((depth, rest.to_string()));
                } else {
                    out.push(requote(depth, rest));
                }
            }
        }
    }
    if let Some((d, acc)) = open {
        out.push(requote(d, &acc));
    }
    out
}

fn requote(depth: u8, text: &str) -> String {
    if depth == 0 {
        text.to_string()
    } else {
        format!("{} {text}", ">".repeat(usize::from(depth)))
    }
}

/// Flowed text quotes with `>` and no spaces between them.
fn quote_depth_strict(line: &str) -> (u8, &str) {
    let depth = line.bytes().take_while(|&b| b == b'>').count();
    (depth.min(9) as u8, &line[depth..])
}

/// How deep `line` is quoted (`>`, `> >`, `>>`), and the rest of it.
fn quote_depth(line: &str) -> (u8, &str) {
    let mut depth = 0u8;
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start_matches(' ');
        // At most a few spaces before a `>`: deeper is indented text.
        if rest.len() - trimmed.len() > 3 {
            break;
        }
        match trimmed.strip_prefix('>') {
            Some(after) => {
                depth = depth.saturating_add(1);
                rest = after;
            }
            None => break,
        }
    }
    if depth > 0 {
        rest = rest.strip_prefix(' ').unwrap_or(rest);
    }
    (depth.min(9), rest)
}

/// Every URL in `line`: where it starts and ends, and its number in
/// `body`.
fn find_urls(line: &str, body: &mut Body) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some((start, end)) = find_url(&line[at..]) {
        if let Some(n) = body.link(&line[at + start..at + end]) {
            out.push((at + start, at + end, n));
        }
        at += end;
    }
    out
}

/// Where `piece` (a slice of `line`, as textwrap returns them) starts in
/// it.
fn offset_in(line: &str, piece: &str) -> Option<usize> {
    let start = piece.as_ptr() as usize;
    let base = line.as_ptr() as usize;
    (start >= base && start + piece.len() <= base + line.len()).then(|| start - base)
}

/// `piece`, which starts `at` bytes into its line, split where the line's
/// `links` begin and end.
fn split_links(piece: &str, at: usize, links: &[(usize, usize, usize)], role: Role) -> Vec<Seg> {
    let mut segs = Vec::new();
    let end = at + piece.len();
    let mut pos = at;
    for &(start, stop, n) in links {
        if stop <= pos || start >= end {
            continue;
        }
        let (start, stop) = (start.max(pos), stop.min(end));
        if start > pos {
            segs.push(Seg {
                text: piece[pos - at..start - at].to_string(),
                role,
            });
        }
        segs.push(Seg {
            text: piece[start - at..stop - at].to_string(),
            role: Role::Link(n),
        });
        pos = stop;
    }
    if pos < end {
        segs.push(Seg {
            text: piece[pos - at..].to_string(),
            role,
        });
    }
    segs
}

/// `line` split into text and links (http, https, mailto), each link
/// numbered in `body`.
fn linkify(line: &str, role: Role, body: &mut Body) -> Vec<Seg> {
    let mut segs = Vec::new();
    let mut rest = line;
    while let Some((start, end)) = find_url(rest) {
        if start > 0 {
            segs.push(Seg {
                text: rest[..start].to_string(),
                role,
            });
        }
        let url = &rest[start..end];
        segs.push(Seg {
            text: url.to_string(),
            role: body.link(url).map_or(role, Role::Link),
        });
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        segs.push(Seg {
            text: rest.to_string(),
            role,
        });
    }
    segs
}

/// The longest link: a longer one is left as text.
const MAX_LINK: usize = 4096;
/// The most links a message's text numbers.
const MAX_LINKS: usize = 1000;

/// Where the first URL in `text` starts and ends. One pass, so a long line
/// costs no more than its length.
fn find_url(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let scheme = ["https://", "http://", "mailto:"]
            .into_iter()
            .find(|scheme| {
                bytes
                    .get(i..i + scheme.len())
                    .is_some_and(|b| b.eq_ignore_ascii_case(scheme.as_bytes()))
            });
        // An ASCII scheme matched, so `i` is on a character boundary. Not
        // in the middle of a word ("xhttp://").
        let starts_word = || i == 0 || !text[..i].chars().last().is_some_and(char::is_alphanumeric);
        let Some(scheme) = scheme.filter(|_| starts_word()) else {
            i += 1;
            continue;
        };
        let tail = &text[i..];
        let mut end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
            .unwrap_or(tail.len());
        if end > MAX_LINK {
            i += end;
            continue;
        }
        // Punctuation that ends a sentence isn't part of the link, nor is a
        // closing bracket the link didn't open. Brackets are counted once.
        let count = |c: char| tail[..end].matches(c).count() as isize;
        let mut open = [count('('), count('['), count('{')];
        let mut close = [count(')'), count(']'), count('}')];
        while let Some(last) = tail[..end].chars().last() {
            let bracket = match last {
                ')' => Some(0),
                ']' => Some(1),
                '}' => Some(2),
                _ => None,
            };
            let drop = match bracket {
                Some(b) => open[b] < close[b],
                None => matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\''),
            };
            if !drop {
                break;
            }
            if let Some(b) = bracket {
                close[b] -= 1;
            }
            match last {
                '(' => open[0] -= 1,
                '[' => open[1] -= 1,
                '{' => open[2] -= 1,
                _ => {}
            }
            end -= last.len_utf8();
        }
        if end > scheme.len() {
            return Some((i, i + end));
        }
        i += scheme.len();
    }
    None
}

/// HTML laid out as text by html2text: links numbered, bold and italics
/// kept, pictures shown by their alt text only. Nothing is fetched.
pub fn render_html(html: &str, width: usize) -> Body {
    use html2text::render::RichAnnotation;
    let mut body = Body {
        from_html: true,
        ..Body::default()
    };
    let (html, cut) = match html.char_indices().nth(MAX_HTML) {
        Some((i, _)) => (&html[..i], true),
        None => (html, false),
    };
    if too_deep(html) {
        return render_text(&strip_tags(html), width, false);
    }
    // Too narrow is laid out anyway: html2text giving up there drops the
    // sender's whole nesting by recursion, which can overflow even the
    // render stack.
    let lines = html2text::config::rich()
        .link_footnotes(false)
        .no_table_borders()
        .allow_width_overflow()
        .lines_from_read(html.as_bytes(), width);
    let Ok(lines) = lines else {
        return render_text(&strip_tags(html), width, false);
    };
    let mut blank = 0;
    for line in lines {
        let mut segs: Vec<Seg> = Vec::new();
        for piece in line.tagged_strings() {
            let text = text::clean(&piece.s)
                .replace(['\n', '\r'], " ")
                .replace('\t', "    ");
            if text.is_empty() {
                continue;
            }
            let mut role = Role::Text;
            for annotation in &piece.tag {
                role = match annotation {
                    RichAnnotation::Link(url) if usable_link(url) => {
                        body.link(url).map_or(role, Role::Link)
                    }
                    RichAnnotation::Image(_) => Role::Note,
                    RichAnnotation::Strong => Role::Bold,
                    RichAnnotation::Emphasis => Role::Italic,
                    RichAnnotation::Code | RichAnnotation::Preformat(_) => Role::Code,
                    _ => role,
                };
            }
            match segs.last_mut() {
                Some(last) if last.role == role => last.text.push_str(&text),
                _ => segs.push(Seg { text, role }),
            }
        }
        // Newsletters stack empty rows for spacing: at most one in a row.
        let empty = segs.iter().all(|s| s.text.trim().is_empty());
        blank = if empty { blank + 1 } else { 0 };
        if blank <= 1 {
            body.lines.push(if empty { Vec::new() } else { segs });
        }
    }
    while body.lines.first().is_some_and(Vec::is_empty) {
        body.lines.remove(0);
    }
    if cut {
        body.lines.push(vec![Seg {
            text: "(the rest of this message is too long to show)".into(),
            role: Role::Note,
        }]);
    }
    body
}

/// A link tuimail will offer to open: http, https or mailto, no spaces or
/// control characters, not absurdly long.
pub fn usable_link(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:"))
        && url.len() <= 4096
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || text::is_hidden(c))
}

/// HTML without its tags: the fallback when html2text can't read it.
/// Whether HTML nests deeper than [`MAX_DEPTH`], counted in one pass over
/// its tags: an opening tag goes one in, a closing one out. Elements that
/// have no end (`<br>`) or whose end is usually left out (`<p>`, `<li>`,
/// table cells) don't count, so ordinary mail isn't taken for deep.
fn too_deep(html: &str) -> bool {
    const LOOSE: [&str; 22] = [
        "br", "img", "hr", "meta", "link", "input", "area", "base", "col", "embed", "source",
        "track", "wbr", "param", "p", "li", "td", "th", "tr", "option", "dt", "dd",
    ];
    let mut depth: usize = 0;
    let bytes = html.as_bytes();
    let mut i = 0;
    while let Some(at) = bytes[i..].iter().position(|&b| b == b'<') {
        i += at + 1;
        let closing = bytes.get(i) == Some(&b'/');
        let start = i + usize::from(closing);
        let len = bytes[start..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        if len == 0 {
            continue;
        }
        let name = html[start..start + len].to_ascii_lowercase();
        if LOOSE.contains(&name.as_str()) {
            continue;
        }
        let end = bytes[start..].iter().position(|&b| b == b'>');
        let self_closing = end.is_some_and(|e| e > 0 && bytes[start + e - 1] == b'/');
        if closing {
            depth = depth.saturating_sub(1);
        } else if !self_closing {
            depth += 1;
            if depth > MAX_DEPTH {
                return true;
            }
        }
    }
    false
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// The message's text as plain lines, for quoting in a reply or a
/// forward: the plain part, else the HTML as text.
pub fn plain_text(parsed: &Parsed) -> String {
    if let Some(text) = &parsed.text {
        let text = text::clean(text::first_chars(text, MAX_TEXT));
        return if parsed.flowed {
            unflow(&text).join("\n")
        } else {
            text
        };
    }
    let Some(html) = &parsed.html else {
        return String::new();
    };
    let html = text::first_chars(html, MAX_HTML);
    if too_deep(html) {
        return text::clean(&strip_tags(html));
    }
    html2text::config::plain()
        .link_footnotes(true)
        .no_table_borders()
        .allow_width_overflow()
        .string_from_read(html.as_bytes(), 76)
        .map(|t| text::clean(&t))
        .unwrap_or_else(|_| strip_tags(html))
}

/// `text` quoted for a reply: each line after `> `.
pub fn quote(text: &str) -> String {
    let mut out = String::new();
    for line in text.trim_end().lines() {
        if line.starts_with('>') {
            out.push('>');
        } else {
            out.push_str("> ");
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// A date for the list: the time today, "Mon 14" this week, "3 Oct" this
/// year, else the full date. In local time.
pub fn short_date(timestamp: i64, now: i64) -> String {
    use chrono::{Datelike, Local, TimeZone};
    let (Some(at), Some(today)) = (
        Local.timestamp_opt(timestamp, 0).single(),
        Local.timestamp_opt(now, 0).single(),
    ) else {
        return String::new();
    };
    if timestamp <= 0 {
        return String::new();
    }
    if at.date_naive() == today.date_naive() {
        at.format("%H:%M").to_string()
    } else if (0..6 * 86400).contains(&(now - timestamp)) {
        at.format("%a %H:%M").to_string()
    } else if at.year() == today.year() {
        at.format("%-d %b").to_string()
    } else {
        at.format("%Y-%m-%d").to_string()
    }
}

/// A date in full, for a message's header: "Sat 11 Oct 2026 10:42".
pub fn long_date(timestamp: i64) -> String {
    use chrono::{Local, TimeZone};
    if timestamp <= 0 {
        return String::new();
    }
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|d| d.format("%a %-d %b %Y %H:%M").to_string())
        .unwrap_or_default()
}

/// A size as people say it: 980 B, 12 KB, 3.4 MB.
pub fn size(bytes: usize) -> String {
    match bytes {
        0..1000 => format!("{bytes} B"),
        1000..1_000_000 => format!("{} KB", bytes / 1000),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &[u8] = b"From: \"Alice Rivera\" <alice@example.com>\r\n\
To: Bob <bob@example.org>, carol@example.net\r\n\
Cc: =?utf-8?q?J=C3=BCrgen?= <j@example.de>\r\n\
Subject: =?utf-8?b?THVuY2ggdG9tb3Jyb3c/?=\r\n\
Date: Sat, 11 Oct 2026 10:42:00 +0000\r\n\
Message-ID: <abc.123@example.com>\r\n\
In-Reply-To: <prev@example.org>\r\n\
References: <first@example.org> <prev@example.org>\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Hi Bob,\r\n\
\r\n\
see https://example.com/menu. Thanks!\r\n\
\r\n\
> Earlier: are we on?\r\n\
>> Even earlier\r\n";

    #[test]
    fn headers_are_decoded_and_summed_up() {
        let s = summarize(SIMPLE);
        assert_eq!(s.subject, "Lunch tomorrow?");
        assert_eq!(s.from.name, "Alice Rivera");
        assert_eq!(s.from.addr, "alice@example.com");
        assert_eq!(s.to, "Bob <bob@example.org>, carol@example.net");
        assert_eq!(s.cc, "Jürgen <j@example.de>");
        assert_eq!(s.message_id, "abc.123@example.com");
        assert_eq!(s.in_reply_to, "prev@example.org");
        assert_eq!(s.references, "first@example.org prev@example.org");
        assert_eq!(s.date, 1_791_715_320);
        assert!(!s.attachments);
    }

    #[test]
    fn plain_text_is_wrapped_with_quotes_and_links_marked() {
        let parsed = parse(SIMPLE).unwrap();
        let body = render(&parsed, 80, false);
        assert_eq!(body.line(0), "Hi Bob,");
        assert_eq!(body.line(2), "see https://example.com/menu. Thanks!");
        assert_eq!(body.links, ["https://example.com/menu"]);
        assert!(body.lines[2].iter().any(|s| s.role == Role::Link(0)));
        assert_eq!(body.line(4), "> Earlier: are we on?");
        assert_eq!(body.lines[4][0].role, Role::Quote(1));
        assert_eq!(body.line(5), ">> Even earlier");
        assert_eq!(body.lines[5][0].role, Role::Quote(2));
        // A link that wraps stays a link, on both lines.
        let wrapped = render(&parsed, 25, false);
        let link_rows: Vec<String> = wrapped
            .lines
            .iter()
            .flatten()
            .filter(|s| s.role == Role::Link(0))
            .map(|s| s.text.clone())
            .collect();
        assert_eq!(
            link_rows.concat(),
            "https://example.com/menu",
            "{link_rows:?}"
        );
        let narrow = render(&parsed, 20, false);
        assert!(
            narrow
                .lines
                .iter()
                .all(|l| { l.iter().map(|s| s.text.chars().count()).sum::<usize>() <= 20 })
        );
    }

    #[test]
    fn a_huge_line_of_links_lays_out_quickly() {
        let line = format!("{} end", "(https://x.example/a) ".repeat(20_000));
        let raw = format!("From: a@b.c\r\n\r\n{line}\r\n");
        let parsed = parse(raw.as_bytes()).unwrap();
        let started = std::time::Instant::now();
        let body = render(&parsed, 80, false);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(body.links, ["https://x.example/a"]);
        let long = format!("x https://y.example/{} z", "a".repeat(400_000));
        assert_eq!(find_url(&long), None, "too long to be a link");
    }

    #[test]
    fn urls_stop_at_punctuation_and_unopened_brackets() {
        fn url(t: &str) -> Option<&str> {
            find_url(t).map(|(s, e)| &t[s..e])
        }
        assert_eq!(url("go to https://a.b/c."), Some("https://a.b/c"));
        assert_eq!(url("(see https://a.b/c)"), Some("https://a.b/c"));
        assert_eq!(
            url("https://en.wikipedia.org/wiki/Rust_(language)"),
            Some("https://en.wikipedia.org/wiki/Rust_(language)")
        );
        assert_eq!(url("<mailto:x@y.z>"), Some("mailto:x@y.z"));
        assert_eq!(url("nohttps://x"), None);
        assert_eq!(url("https://"), None);
        assert_eq!(url("HTTPS://A.B/c"), Some("HTTPS://A.B/c"));
        assert_eq!(url("é https://a.b"), Some("https://a.b"));
    }

    #[test]
    fn html_is_laid_out_as_text_with_links_and_no_pictures_fetched() {
        let raw = b"From: a@b.c\r\nContent-Type: text/html\r\n\r\n\
<html><body><h1>Sale</h1><p>Hello <b>there</b>, <a href=\"https://shop.example/x\">click</a>\
<img src=\"https://tracker.example/p.gif\" alt=\"logo\"></p>\
<p></p><p></p><p></p><p>Bye</p></body></html>";
        let parsed = parse(raw).unwrap();
        assert!(parsed.text.is_none() && parsed.html.is_some());
        let body = render(&parsed, 60, false);
        assert!(body.from_html);
        assert_eq!(
            body.links,
            ["https://shop.example/x"],
            "the picture isn't a link"
        );
        let all: String = (0..body.lines.len()).map(|i| body.line(i) + "\n").collect();
        assert!(all.contains("Hello there"), "{all}");
        assert!(all.contains("click"));
        assert!(!all.contains("tracker.example"), "{all}");
        assert!(!all.contains("\n\n\n"), "blank rows squeezed: {all:?}");
        let bold = body
            .lines
            .iter()
            .flatten()
            .find(|s| s.text.contains("there"))
            .unwrap();
        assert_eq!(bold.role, Role::Bold);
    }

    #[test]
    fn a_link_says_what_its_text_says_unless_that_looks_like_an_address() {
        let raw = b"From: a@b.c\r\nContent-Type: text/html\r\n\r\n\
<h2><a href=\"https://r.example.com/1\">Aerial view</a></h2>\
<p><a href=\"https://r.example.com/2\">Try it</a></p>\
<p><a href=\"https://r.example.com/3\"><img src=\"x.png\" alt=\"Logo\"></a></p>\
<p><a href=\"https://evil.example/a\">paypal.com</a></p>\
<p><a href=\"https://evil.example/b\">https://paypal.com/login</a></p>\
<p><a href=\"https://r.example.com/2\">Try it</a></p>";
        let parsed = parse(raw).unwrap();
        let body = render(&parsed, 60, false);
        assert_eq!(body.links.len(), 5, "{:?}", body.links);
        let said: Vec<String> = (0..body.links.len()).map(|n| body.link_text(n)).collect();
        assert_eq!(said, ["Aerial view", "Try it", "", "", ""]);
    }

    #[test]
    fn windows_device_names_never_pass() {
        for name in [
            "CON .txt",
            "nul .pdf",
            "PRN .txt",
            "COM1 .txt",
            "LPT1 .txt",
            "COM\u{b9}.txt",
            "LPT\u{b2}.txt",
            "CONIN$.txt",
            "CONOUT$.txt",
            "CON\t.txt",
            "aux",
        ] {
            assert!(safe_file_name(name).starts_with("attachment"), "{name}");
        }
        for fine in ["CONSOLE.txt", "COM10.txt", "nullish.pdf", "report.pdf"] {
            assert!(!safe_file_name(fine).starts_with("attachment"), "{fine}");
        }
    }

    #[test]
    fn an_unnamed_attachment_is_saved_under_the_name_it_was_listed_by() {
        let raw = b"From: a@b.c\r\nSubject: x\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nsee\r\n\
--b\r\nContent-Type: image/png\r\nContent-Transfer-Encoding: base64\r\n\r\nPGh0bWw+PC9odG1sPg==\r\n--b--\r\n";
        let listed = parse(raw).unwrap().attachments[0].name.clone();
        assert_eq!(listed, "image.png");
        assert_eq!(attachment(raw, 0).unwrap().0, listed);
    }

    #[test]
    fn plain_is_preferred_only_when_asked() {
        let raw = b"From: a@b.c\r\nContent-Type: multipart/alternative; boundary=x\r\n\r\n\
--x\r\nContent-Type: text/plain\r\n\r\nplain version\r\n\
--x\r\nContent-Type: text/html\r\n\r\n<p>html version</p>\r\n--x--\r\n";
        let parsed = parse(raw).unwrap();
        assert!(render(&parsed, 60, false).line(0).contains("html version"));
        assert!(render(&parsed, 60, true).line(0).contains("plain version"));
    }

    #[test]
    fn attachments_are_listed_with_safe_names() {
        let raw =
            b"From: a@b.c\r\nSubject: files\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n\
--b\r\nContent-Type: application/pdf; name=\"../../etc/invoice\xe2\x80\xaefdp.exe\"\r\n\
Content-Disposition: attachment\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--b--\r\n";
        let parsed = parse(raw).unwrap();
        assert!(parsed.summary.attachments);
        assert_eq!(parsed.attachments.len(), 1);
        let a = &parsed.attachments[0];
        assert_eq!(a.name, "invoicefdp.exe");
        assert_eq!(a.mime, "application/pdf");
        assert_eq!(a.size, 5);
        let (name, data) = attachment(raw, 0).unwrap();
        assert_eq!(
            (name.as_str(), data.as_slice()),
            ("invoicefdp.exe", &b"hello"[..])
        );
    }

    #[test]
    fn file_names_cannot_escape_or_pose() {
        assert_eq!(safe_file_name("../../.bashrc"), "bashrc");
        assert_eq!(safe_file_name("C:\\Windows\\evil.dll"), "evil.dll");
        assert_eq!(safe_file_name("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(safe_file_name("CON.txt"), "attachment CON.txt");
        assert_eq!(safe_file_name("..."), "attachment");
        assert_eq!(safe_file_name("x\r\ny.txt"), "x y.txt");
        let long = format!("{}.pdf", "a".repeat(300));
        let safe = safe_file_name(&long);
        assert_eq!(safe.chars().count(), 120);
        assert!(safe.ends_with(".pdf"));
    }

    #[test]
    fn flowed_text_is_joined_back() {
        let raw = b"From: a@b.c\r\nContent-Type: text/plain; format=flowed\r\n\r\n\
This is a long \r\nline that was \r\nsplit.\r\n>quoted \r\n>more\r\n-- \r\nsig\r\n";
        let parsed = parse(raw).unwrap();
        assert!(parsed.flowed);
        let body = render(&parsed, 80, false);
        assert_eq!(body.line(0), "This is a long line that was split.");
        assert_eq!(body.line(1), "> quoted more");
        assert_eq!(body.line(2), "--", "the signature line, not joined");
    }

    #[test]
    fn replies_quote_every_line() {
        assert_eq!(quote("a\n\n> b\n"), "> a\n> \n>> b\n");
        let parsed = parse(SIMPLE).unwrap();
        assert!(plain_text(&parsed).starts_with("Hi Bob,"));
    }

    #[test]
    fn control_characters_in_a_body_never_reach_the_screen() {
        let raw = b"From: a@b.c\r\nSubject: x\x1b[2Jy\r\n\r\nline\x1b]52;c;ZXZpbA==\x07 end\r\n";
        let parsed = parse(raw).unwrap();
        assert!(!parsed.summary.subject.contains('\x1b'));
        let body = render(&parsed, 80, false);
        assert!(
            body.lines
                .iter()
                .flatten()
                .all(|s| !s.text.contains(['\x1b', '\x07']))
        );
    }

    #[test]
    fn deeply_nested_html_does_not_crash() {
        let html = format!("{}x{}", "<div>".repeat(20_000), "</div>".repeat(20_000));
        let raw = format!("From: a@b.c\r\nContent-Type: text/html\r\n\r\n{html}");
        let parsed = parse(raw.as_bytes()).unwrap();
        let handle = std::thread::Builder::new()
            .stack_size(crate::mail::RENDER_STACK)
            .spawn(move || render(&parsed, 80, false))
            .unwrap();
        let body = handle.join().unwrap();
        assert!(!body.lines.is_empty());
    }

    /// The deepest nesting MAX_HTML allows, behind quotes that are too
    /// wide for the width: html2text once gave up there and dropped the
    /// whole deep tree by recursion, which a 256 MB stack may not hold.
    #[test]
    fn the_deepest_html_allowed_is_laid_out_on_the_render_stack() {
        let quotes = "<blockquote>".repeat(200);
        let depth = (MAX_HTML - quotes.len() - 1) / 3;
        let html = format!("{quotes}{}x", "<b>".repeat(depth));
        assert!(html.chars().count() <= MAX_HTML);
        let raw = format!("From: a@b.c\r\nContent-Type: text/html\r\n\r\n{html}");
        let parsed = parse(raw.as_bytes()).unwrap();
        let handle = std::thread::Builder::new()
            .stack_size(crate::mail::RENDER_STACK)
            .spawn(move || (render(&parsed, 400, false), plain_text(&parsed)))
            .unwrap();
        let (body, plain) = handle.join().unwrap();
        assert!(!body.lines.is_empty());
        assert!(plain.contains('x'));
        // Ordinary depth is still laid out as HTML.
        assert!(!too_deep(&format!("{}x", "<div><p>".repeat(1_000))));
        assert!(too_deep(&"<div>".repeat(MAX_DEPTH + 1)));
        assert!(!too_deep(&"<div></div>".repeat(MAX_DEPTH * 2)));
    }

    #[test]
    fn sizes_and_dates_read_naturally() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(12_345), "12 KB");
        assert_eq!(size(3_400_000), "3.4 MB");
        let now = 1_791_715_320;
        assert_eq!(short_date(0, now), "");
        assert!(short_date(now - 400 * 86400, now).contains('-'));
        assert!(long_date(now).contains("2026"));
    }
}

/// The stack laying out a message gets: html2text recurses through the
/// HTML's nesting, which a sender chooses.
pub const RENDER_STACK: usize = 256 << 20;

thread_local! {
    static CONTAINED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` (parsing or laying out a message) so that a panic in it, which
/// a crafted message could cause in a library, only fails this one job:
/// `None`. The panic hook lets such a panic pass ([`panic_is_contained`]).
pub fn contained<T>(f: impl FnOnce() -> T) -> Option<T> {
    CONTAINED.with(|c| c.set(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok();
    CONTAINED.with(|c| c.set(false));
    result
}

/// Whether a panic happening now is inside [`contained`].
pub fn panic_is_contained() -> bool {
    CONTAINED.with(|c| c.get())
}

/// Runs `job` on a thread of its own with a big stack, its panics
/// contained, and hands its result (`None` if it panicked) to `done`.
pub fn in_background<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(Option<T>) + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name("tuimail-render".into())
        .stack_size(RENDER_STACK)
        .spawn(move || done(contained(job)));
    if let Err(e) = spawned {
        // No thread: nothing can be laid out, which `done` would say.
        let _ = e;
    }
}
