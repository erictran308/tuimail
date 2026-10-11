//! Writing mail: the fields of a message being written, turned into a
//! MIME message to send (mail-builder), and the starting text of a reply
//! or a forward.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as BuilderAddress;

use crate::account::{self, Account};
use crate::mail::{self, Addr, Parsed};
use crate::text;

/// The biggest attachment, and all of a message's together.
pub const MAX_ATTACHMENT: u64 = 40 << 20;
pub const MAX_TOTAL: u64 = 60 << 20;

/// A file to send with the message.
#[derive(Clone, Debug, PartialEq)]
pub enum Attachment {
    /// A file on this computer, read when the message is built.
    File(PathBuf),
    /// Bytes already in hand: a forwarded message's attachments, or a
    /// draft's.
    Data {
        name: String,
        mime: String,
        bytes: Arc<Vec<u8>>,
    },
}

impl Attachment {
    pub fn name(&self) -> String {
        match self {
            Attachment::File(path) => mail::safe_file_name(
                &path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            Attachment::Data { name, .. } => name.clone(),
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            Attachment::File(path) => std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            Attachment::Data { bytes, .. } => bytes.len() as u64,
        }
    }

    /// Its type, its name and its bytes.
    fn read(&self) -> Result<(String, String, Vec<u8>)> {
        match self {
            Attachment::File(path) => {
                let shown = crate::config::shown(path);
                let meta =
                    std::fs::metadata(path).with_context(|| format!("cannot read {shown}"))?;
                if !meta.is_file() {
                    bail!("{shown} isn't a file");
                }
                if meta.len() > MAX_ATTACHMENT {
                    bail!(
                        "{shown} is too big to send ({})",
                        mail::size(meta.len() as usize)
                    );
                }
                let bytes = std::fs::read(path).with_context(|| format!("cannot read {shown}"))?;
                let name = self.name();
                Ok((mime_of(&name).to_string(), name, bytes))
            }
            Attachment::Data { name, mime, bytes } => {
                Ok((mime.clone(), name.clone(), bytes.as_ref().clone()))
            }
        }
    }
}

/// A file's type, by its extension.
pub fn mime_of(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" | "md" | "log" | "csv" => "text/plain",
        "html" | "htm" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "json" => "application/json",
        "xml" => "application/xml",
        "ics" => "text/calendar",
        "eml" => "message/rfc822",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// A message being written: plain data, edited by the composer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Draft {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: String,
    /// Ids separated by spaces.
    pub references: String,
    pub attachments: Vec<Attachment>,
}

/// A message ready to go.
pub struct Built {
    pub raw: Vec<u8>,
    /// The envelope: who it's from and every recipient (To, Cc and Bcc).
    pub from: String,
    pub rcpts: Vec<String>,
    /// Everyone it's to, for address completion later.
    pub to: Vec<Addr>,
}

/// Addresses as typed: "Name <addr>, addr; …". Each must be an address;
/// the first that isn't is the error.
pub fn parse_addresses(input: &str) -> Result<Vec<Addr>, String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut angle = false;
    let mut items = Vec::new();
    let mut escaped = false;
    for c in input.chars() {
        if escaped {
            escaped = false;
            current.push(c);
            continue;
        }
        match c {
            '\\' if quoted => {
                escaped = true;
                current.push(c);
            }
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            '<' if !quoted => {
                angle = true;
                current.push(c);
            }
            '>' if !quoted => {
                angle = false;
                current.push(c);
            }
            ',' | ';' | '\n' if !quoted && !angle => items.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    items.push(current);
    for item in items {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (name, addr) = match (item.rfind('<'), item.rfind('>')) {
            (Some(open), Some(close)) if open < close => (
                unquote_name(item[..open].trim()),
                item[open + 1..close].trim(),
            ),
            _ => (String::new(), item),
        };
        if !account::valid_address(addr) {
            return Err(text::one_line(item, 80));
        }
        out.push(Addr {
            name: text::one_line(&name, mail::MAX_NAME),
            addr: addr.to_string(),
        });
    }
    Ok(out)
}

/// A name as written before an address: its quotes and `\` escapes taken
/// out.
fn unquote_name(name: &str) -> String {
    let Some(inner) = name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) else {
        return name.to_string();
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

fn builder_list(addrs: &[Addr]) -> BuilderAddress<'static> {
    let list: Vec<BuilderAddress<'static>> = addrs
        .iter()
        .map(|a| {
            if a.name.is_empty() {
                BuilderAddress::from(a.addr.clone())
            } else {
                BuilderAddress::from((a.name.clone(), a.addr.clone()))
            }
        })
        .collect();
    BuilderAddress::new_list(list)
}

/// A new Message-ID at the sender's domain: random, so it says nothing
/// about this computer.
pub fn new_message_id(from: &str) -> String {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    let _ = ring::rand::SystemRandom::new().fill(&mut bytes);
    let random: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let domain = from
        .rsplit_once('@')
        .map(|(_, d)| d)
        .filter(|d| account::valid_host(d))
        .unwrap_or("tuimail.invalid");
    format!("{random}@{domain}")
}

/// Builds the message `draft` describes, from `account`, dated `date`. With
/// `keep_bcc` the Bcc header stays (the copy in Sent or Drafts); the one
/// sent never has it.
pub fn build(
    draft: &Draft,
    account: &Account,
    message_id: &str,
    date: i64,
    keep_bcc: bool,
) -> Result<Built> {
    let field = |input: &str, what: &str| {
        parse_addresses(input)
            .map_err(|bad| anyhow::anyhow!("{what}: “{bad}” isn't an email address"))
    };
    let to = field(&draft.to, "To")?;
    let cc = field(&draft.cc, "Cc")?;
    let bcc = field(&draft.bcc, "Bcc")?;
    let from_addr = account.email.trim().to_string();
    let from_name = text::one_line(&account.display_name, mail::MAX_NAME);
    let subject = text::one_line(&draft.subject, mail::MAX_SUBJECT);
    let mut builder = MessageBuilder::new()
        .message_id(message_id.to_string())
        .date(date)
        .subject(subject)
        .header(
            "User-Agent",
            mail_builder::headers::raw::Raw::new("tuimail"),
        );
    builder = if from_name.is_empty() {
        builder.from(BuilderAddress::from(from_addr.clone()))
    } else {
        builder.from(BuilderAddress::from((from_name, from_addr.clone())))
    };
    if !to.is_empty() {
        builder = builder.to(builder_list(&to));
    }
    if !cc.is_empty() {
        builder = builder.cc(builder_list(&cc));
    }
    if keep_bcc && !bcc.is_empty() {
        builder = builder.bcc(builder_list(&bcc));
    }
    if !draft.in_reply_to.is_empty() {
        builder = builder.in_reply_to(draft.in_reply_to.clone());
    }
    let references: Vec<String> = draft
        .references
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if !references.is_empty() {
        builder = builder.references(references);
    }
    // Lines end in CRLF, and the text in a line break.
    let mut body = text::clean(&draft.body)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if !body.ends_with('\n') {
        body.push('\n');
    }
    builder = builder.text_body(body.replace('\n', "\r\n"));
    let mut total = 0;
    for attachment in &draft.attachments {
        let (mime, name, bytes) = attachment.read()?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL {
            bail!(
                "the attachments are too big together (over {})",
                mail::size(MAX_TOTAL as usize)
            );
        }
        builder = builder.attachment(mime, name, bytes);
    }
    let raw = builder.write_to_vec().context("cannot build the message")?;
    let mut rcpts: Vec<String> = to
        .iter()
        .chain(&cc)
        .chain(&bcc)
        .map(|a| a.addr.clone())
        .collect();
    rcpts.sort_by_key(|a| a.to_lowercase());
    rcpts.dedup_by_key(|a| a.to_lowercase());
    Ok(Built {
        raw,
        from: from_addr,
        rcpts,
        to: to.into_iter().chain(cc).chain(bcc).collect(),
    })
}

/// "Re: subject", without piling up Re:s.
pub fn reply_subject(subject: &str) -> String {
    let lower = subject.trim_start().to_lowercase();
    if lower.starts_with("re:") || lower.starts_with("aw:") || lower.starts_with("sv:") {
        subject.trim().to_string()
    } else {
        format!("Re: {}", subject.trim())
    }
}

pub fn forward_subject(subject: &str) -> String {
    let lower = subject.trim_start().to_lowercase();
    if lower.starts_with("fwd:") || lower.starts_with("fw:") {
        subject.trim().to_string()
    } else {
        format!("Fwd: {}", subject.trim())
    }
}

fn joined(addrs: &[Addr]) -> String {
    addrs
        .iter()
        .map(Addr::display)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The signature, after "-- ", if the account has one.
pub fn signature(account: &Account) -> String {
    let sig = account.signature.trim_end();
    if sig.is_empty() {
        String::new()
    } else {
        format!("\n\n-- \n{sig}\n")
    }
}

/// The text a reply or forward starts with: a line to write on, the
/// signature, then `rest`.
fn start_with(account: &Account, rest: &str) -> String {
    let sig = signature(account);
    let sig = sig.trim_start_matches('\n').trim_end();
    if sig.is_empty() {
        format!("\n\n{rest}")
    } else {
        format!("\n\n{sig}\n\n{rest}")
    }
}

/// A reply to `parsed`: to its Reply-To or sender; with `all`, also to its
/// other recipients, never to yourself.
pub fn reply(parsed: &Parsed, account: &Account, all: bool) -> Draft {
    let me = account.email.trim().to_lowercase();
    let not_me = |a: &&Addr| a.addr.to_lowercase() != me;
    let mut to: Vec<Addr> = if parsed.reply_to.is_empty() {
        parsed.from.clone()
    } else {
        parsed.reply_to.clone()
    };
    // Replying to your own message goes to whom it was to.
    if to.iter().all(|a| a.addr.to_lowercase() == me) && !parsed.to.is_empty() {
        to = parsed.to.clone();
    }
    let mut cc = Vec::new();
    if all {
        for a in parsed.to.iter().chain(&parsed.cc).filter(not_me) {
            let known = |list: &[Addr]| list.iter().any(|b| b.addr.eq_ignore_ascii_case(&a.addr));
            if !known(&to) && !known(&cc) {
                cc.push(a.clone());
            }
        }
    }
    let s = &parsed.summary;
    let mut references: Vec<&str> = s.references.split_whitespace().collect();
    if !s.message_id.is_empty() && !references.contains(&s.message_id.as_str()) {
        references.push(&s.message_id);
    }
    let when = mail::long_date(s.date);
    let who = s.from.display();
    let intro = if when.is_empty() {
        format!("{who} wrote:")
    } else {
        format!("On {when}, {who} wrote:")
    };
    let quoted = mail::quote(&mail::plain_text(parsed));
    Draft {
        to: joined(&to),
        cc: joined(&cc),
        subject: reply_subject(&s.subject),
        body: start_with(account, &format!("{intro}\n{quoted}")),
        in_reply_to: s.message_id.clone(),
        references: references.join(" "),
        ..Draft::default()
    }
}

/// A forward of `parsed`, with its text below and its attachments
/// (taken from `raw`) attached again.
pub fn forward(parsed: &Parsed, raw: &[u8], account: &Account) -> Draft {
    let s = &parsed.summary;
    let mut header = String::from("---------- Forwarded message ----------\n");
    header.push_str(&format!("From: {}\n", s.from.display()));
    let when = mail::long_date(s.date);
    if !when.is_empty() {
        header.push_str(&format!("Date: {when}\n"));
    }
    header.push_str(&format!("Subject: {}\n", s.subject));
    if !s.to.is_empty() {
        header.push_str(&format!("To: {}\n", s.to));
    }
    if !s.cc.is_empty() {
        header.push_str(&format!("Cc: {}\n", s.cc));
    }
    let attachments = parsed
        .attachments
        .iter()
        .filter_map(|a| {
            let (name, bytes) = mail::attachment(raw, a.index)?;
            Some(Attachment::Data {
                name,
                mime: a.mime.clone(),
                bytes: Arc::new(bytes),
            })
        })
        .collect();
    Draft {
        subject: forward_subject(&s.subject),
        body: start_with(
            account,
            &format!("{header}\n{}", mail::plain_text(parsed).trim_end()),
        ),
        attachments,
        ..Draft::default()
    }
}

/// A draft saved earlier, to write on: its fields, text and attachments.
pub fn from_saved(parsed: &Parsed, raw: &[u8]) -> Draft {
    let joined = |list: &[Addr]| {
        list.iter()
            .map(Addr::display)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let bcc = mail_parser::MessageParser::default()
        .parse(raw)
        .map(|m| {
            m.bcc()
                .map(|b| {
                    b.iter()
                        .filter_map(|a| {
                            Some(Addr {
                                name: text::one_line(a.name().unwrap_or(""), mail::MAX_NAME),
                                addr: text::one_line(a.address()?, mail::MAX_NAME),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let mut draft = forward(parsed, raw, &Account::default());
    draft.to = joined(&parsed.to);
    draft.cc = joined(&parsed.cc);
    draft.bcc = joined(&bcc);
    draft.subject = parsed.summary.subject.clone();
    draft.body = parsed
        .text
        .clone()
        .map(|t| text::clean(&t))
        .unwrap_or_default();
    draft.in_reply_to = parsed.summary.in_reply_to.clone();
    draft.references = parsed.summary.references.clone();
    draft
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Account {
        Account {
            email: "me@example.org".into(),
            display_name: "Sam Taylor".into(),
            signature: "Sam".into(),
            ..Account::default()
        }
    }

    #[test]
    fn addresses_are_read_as_people_type_them() {
        let list =
            parse_addresses("Alice <alice@example.com>, bob@example.org; \"Doe, Jane\" <jane@x.y>")
                .unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].name, "Alice");
        assert_eq!(list[1].addr, "bob@example.org");
        assert_eq!(list[2].name, "Doe, Jane");
        assert_eq!(parse_addresses("  ").unwrap(), []);
        // Names come back as they were, however odd, and never as more
        // addresses.
        for name in [
            "Doe, John",
            "evil@x.org, Boss",
            "a \"quoted\" \\ name",
            "<x@y.z>",
        ] {
            let a = Addr {
                name: name.into(),
                addr: "bob@corp.example".into(),
            };
            let back = parse_addresses(&format!("{}, c@d.e", a.display())).unwrap();
            assert_eq!(back.len(), 2, "{name}");
            assert_eq!(back[0], a, "{name}");
        }
        assert_eq!(parse_addresses("alice, bob@x.y").unwrap_err(), "alice");
        assert!(parse_addresses("a@b.c>\r\nBcc: evil@x.y").is_err());
    }

    #[test]
    fn a_built_message_reads_back_and_keeps_bcc_out_of_what_is_sent() {
        let draft = Draft {
            to: "Alice <alice@example.com>".into(),
            cc: "carol@example.net".into(),
            bcc: "boss@example.org".into(),
            subject: "Lunch? 🍜".into(),
            body: "Hi Alice,\n\nnoon?\n".into(),
            in_reply_to: "abc@example.com".into(),
            references: "first@x.y abc@example.com".into(),
            attachments: vec![Attachment::Data {
                name: "menu.txt".into(),
                mime: "text/plain".into(),
                bytes: Arc::new(b"soup".to_vec()),
            }],
        };
        let id = new_message_id("me@example.org");
        assert!(id.ends_with("@example.org") && id.len() > 40);
        let built = build(&draft, &me(), &id, 1_791_715_320, false).unwrap();
        assert_eq!(built.from, "me@example.org");
        assert_eq!(
            built.rcpts,
            ["alice@example.com", "boss@example.org", "carol@example.net"]
        );
        let text = String::from_utf8_lossy(&built.raw);
        assert!(!text.contains("boss@example.org"), "no Bcc in what's sent");
        assert!(
            text.contains("Date: Sun, 11 Oct 2026 10:42:00 +0000"),
            "UTC: {text}"
        );
        assert!(!text.to_lowercase().contains("x-mailer"));
        let parsed = mail::parse(&built.raw).unwrap();
        assert_eq!(parsed.summary.subject, "Lunch? 🍜");
        assert_eq!(parsed.summary.message_id, id);
        assert_eq!(parsed.summary.in_reply_to, "abc@example.com");
        assert_eq!(parsed.summary.references, "first@x.y abc@example.com");
        assert_eq!(parsed.from[0].name, "Sam Taylor");
        assert!(parsed.text.unwrap().contains("noon?"));
        assert_eq!(parsed.attachments[0].name, "menu.txt");
        let kept = build(&draft, &me(), &id, 1_791_715_320, true).unwrap();
        assert!(String::from_utf8_lossy(&kept.raw).contains("boss@example.org"));
        let bad = Draft {
            to: "not an address".into(),
            ..Draft::default()
        };
        assert!(build(&bad, &me(), &id, 0, false).is_err());
    }

    #[test]
    fn replies_go_to_the_right_people_and_quote_the_message() {
        let raw = b"From: Alice <alice@example.com>\r\nTo: me@example.org, bob@example.com\r\n\
Cc: Carol <carol@example.net>, ME@example.org\r\nSubject: Re: Plans\r\n\
Message-ID: <m2@example.com>\r\nReferences: <m1@example.com>\r\n\
Date: Sat, 11 Oct 2026 10:42:00 +0000\r\n\r\nSee you there.\r\n";
        let parsed = mail::parse(raw).unwrap();
        let one = reply(&parsed, &me(), false);
        assert_eq!(one.to, "Alice <alice@example.com>");
        assert_eq!(one.cc, "");
        assert_eq!(one.subject, "Re: Plans", "no Re: Re:");
        assert_eq!(one.in_reply_to, "m2@example.com");
        assert_eq!(one.references, "m1@example.com m2@example.com");
        assert!(
            one.body.contains("wrote:\n> See you there."),
            "{}",
            one.body
        );
        assert!(
            one.body.starts_with("\n\n-- \nSam\n\nOn "),
            "the signature first: {:?}",
            one.body
        );
        let all = reply(&parsed, &me(), true);
        assert_eq!(
            all.cc, "bob@example.com, Carol <carol@example.net>",
            "never me"
        );

        let fwd = forward(&parsed, raw, &me());
        assert_eq!(fwd.subject, "Fwd: Re: Plans");
        assert!(fwd.body.contains("From: Alice <alice@example.com>"));
        assert!(fwd.body.contains("See you there."));
        assert!(fwd.to.is_empty());
    }

    #[test]
    fn types_come_from_extensions() {
        assert_eq!(mime_of("Report.PDF"), "application/pdf");
        assert_eq!(mime_of("noext"), "application/octet-stream");
    }
}
