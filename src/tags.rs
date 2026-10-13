//! Tags: words put on messages to sort them ("work", "receipts"). An IMAP
//! account keeps them on the server as keywords, so other programs see
//! them too; a POP3 account's are kept here.
//!
//! A tag is spelled as Thunderbird spells its keywords, so tags made there
//! show here and the other way round: in lower case, in IMAP's modified
//! UTF-7 (`utf7`), and Thunderbird's first five tags (`$label1` …) by their
//! names. Keywords servers and programs use for themselves (`$Junk`,
//! `$Forwarded`, `$MDNSent`, `NonJunk`, …) aren't tags and aren't shown.
//!
//! Keywords come from the server and, through it, from anyone who can set
//! them: a tag is shown only if it reads back as one could be typed here
//! (letters, digits, `-`, `_`, `.`), and a message shows at most
//! [`MAX_TAGS`] of them.

use crate::utf7;

/// The longest tag, in characters.
pub const MAX_TAG: usize = 40;
/// The most tags a message shows.
pub const MAX_TAGS: usize = 20;

/// Thunderbird's own tags, by the keywords it gives them.
const THUNDERBIRD: [(&str, &str); 5] = [
    ("$label1", "important"),
    ("$label2", "work"),
    ("$label3", "personal"),
    ("$label4", "todo"),
    ("$label5", "later"),
];

/// Keywords that mean junk or not, which spam filters set: never tags.
const RESERVED: [&str; 3] = ["junk", "nonjunk", "notjunk"];

/// A tag as typed, made one: trimmed, in lower case, spaces as `-`.
/// `None` if it has anything but letters, digits, `-`, `_` and `.`, is
/// longer than [`MAX_TAG`], or is a keyword that isn't a tag.
pub fn normalize(typed: &str) -> Option<String> {
    let tag = typed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    let ok = !tag.is_empty()
        && tag.chars().count() <= MAX_TAG
        && tag
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !RESERVED.contains(&tag.as_str());
    ok.then_some(tag)
}

/// The keyword that puts `tag` (made with [`normalize`]) on a message.
pub fn keyword(tag: &str) -> Option<String> {
    let keyword = match THUNDERBIRD.iter().find(|(_, name)| *name == tag) {
        Some((keyword, _)) => (*keyword).to_string(),
        None => utf7::encode(tag),
    };
    is_atom(&keyword).then_some(keyword)
}

/// Every keyword that shows as `tag`: what takes it off a message, and
/// what finds it. Thunderbird's tags have two spellings ("work" is
/// `$label2`, and `work` when set by a program that doesn't know).
pub fn keywords(tag: &str) -> Vec<String> {
    let mut all: Vec<String> = keyword(tag).into_iter().collect();
    let plain = utf7::encode(tag);
    if !all.contains(&plain) && is_atom(&plain) {
        all.push(plain);
    }
    all
}

/// The tag a keyword from the server shows as, if it's one.
pub fn from_keyword(keyword: &str) -> Option<String> {
    if let Some((_, name)) = THUNDERBIRD
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(keyword))
    {
        return Some((*name).to_string());
    }
    if keyword.starts_with(['$', '\\']) || !is_atom(keyword) {
        return None;
    }
    let decoded = utf7::decode(keyword).unwrap_or_else(|| keyword.to_string());
    // Only a keyword that reads as it would be typed: anything else would
    // show as one tag and be written back as another.
    normalize(&decoded).filter(|tag| *tag == decoded.to_lowercase())
}

/// A message's tags from its keywords: sorted, each once, at most
/// [`MAX_TAGS`].
pub fn from_keywords<'a>(keywords: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut tags: Vec<String> = keywords.filter_map(from_keyword).collect();
    tidy(&mut tags);
    tags
}

/// Sorts `tags`, drops repeats, and keeps the first [`MAX_TAGS`].
pub fn tidy(tags: &mut Vec<String>) {
    tags.sort();
    tags.dedup();
    tags.truncate(MAX_TAGS);
}

/// Tags as the cache keeps them: separated by spaces.
pub fn to_stored(tags: &[String]) -> String {
    tags.join(" ")
}

/// Tags read back from the cache, checked again: the file could have been
/// changed.
pub fn from_stored(stored: &str) -> Vec<String> {
    let mut tags: Vec<String> = stored
        .split_whitespace()
        .take(MAX_TAGS * 4)
        .filter_map(|t| normalize(t).filter(|n| n == t))
        .collect();
    tidy(&mut tags);
    tags
}

/// Whether `keyword` can go in an IMAP command as it is: an atom (RFC
/// 3501), printable ASCII without `(){%*"\]`, and not too long.
pub fn is_atom(keyword: &str) -> bool {
    !keyword.is_empty()
        && keyword.len() <= 200
        && keyword
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"(){%*\"\\]".contains(&b))
}

/// The tag a search word asks for (`tag:work`), if it's one.
pub fn searched(word: &str) -> Option<String> {
    let (prefix, tag) = word.split_at_checked(4)?;
    if !prefix.eq_ignore_ascii_case("tag:") {
        return None;
    }
    normalize(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_tags_are_made_one_spelling_or_refused() {
        for (typed, tag) in [
            ("Work", Some("work")),
            ("  to read ", Some("to-read")),
            ("Công_việc", Some("công_việc")),
            ("v1.2", Some("v1.2")),
            ("", None),
            ("a b(c)", None),
            ("$label1", None),
            ("Junk", None),
            ("tab\there", Some("tab-here")),
            ("new\u{202E}line", None),
        ] {
            assert_eq!(normalize(typed).as_deref(), tag, "{typed:?}");
        }
        assert_eq!(
            normalize(&"x".repeat(MAX_TAG)).map(|t| t.len()),
            Some(MAX_TAG)
        );
        assert_eq!(normalize(&"x".repeat(MAX_TAG + 1)), None, "too long");
    }

    #[test]
    fn tags_and_keywords_go_both_ways_as_thunderbird_spells_them() {
        for (tag, keyword) in [
            ("work", "$label2"),
            ("receipts", "receipts"),
            ("công_việc", "c&APQ-ng_vi&Hsc-c"),
        ] {
            assert_eq!(super::keyword(tag).as_deref(), Some(keyword), "{tag}");
            assert_eq!(from_keyword(keyword).as_deref(), Some(tag), "{keyword}");
        }
        assert_eq!(keywords("work"), ["$label2", "work"]);
        assert_eq!(keywords("receipts"), ["receipts"]);
    }

    #[test]
    fn keywords_that_arent_tags_are_not_shown() {
        for keyword in [
            "$Junk",
            "$NotJunk",
            "NonJunk",
            "junk",
            "$Forwarded",
            "$MDNSent",
            "\\Seen",
            "\\seen",
            "has space",
            "two/parts",
            "&Jjo-",
            "",
        ] {
            assert_eq!(from_keyword(keyword), None, "{keyword:?}");
        }
        assert_eq!(from_keyword("$LABEL1").as_deref(), Some("important"));
        // Set with capitals by another program: shown, in lower case.
        assert_eq!(from_keyword("Work").as_deref(), Some("work"));
    }

    #[test]
    fn a_message_shows_its_tags_sorted_once_and_not_too_many() {
        let many: Vec<String> = (0..50).map(|i| format!("t{i:02}")).collect();
        let tags = from_keywords(
            ["work", "$label2", "$Junk", "alpha"]
                .into_iter()
                .chain(many.iter().map(String::as_str)),
        );
        assert_eq!(tags.len(), MAX_TAGS);
        assert_eq!(tags[..2], ["alpha", "t00"]);
        assert!(!tags.contains(&"work".to_string()), "past the cap");
        assert_eq!(from_keywords(["work", "$label2"].into_iter()), ["work"]);
    }

    #[test]
    fn stored_tags_are_checked_again() {
        let stored = to_stored(&["a".into(), "b".into()]);
        assert_eq!(from_stored(&stored), ["a", "b"]);
        assert_eq!(from_stored("b a a Bad $x \u{1b}[31m ok"), ["a", "b", "ok"]);
        assert!(from_stored("").is_empty());
    }

    #[test]
    fn only_atoms_go_in_commands() {
        assert!(is_atom("$label1"));
        assert!(is_atom("c&AfQ-ng"));
        for bad in ["", "a b", "a)", "a*", "a\"", "a\\", "a]", "é", "a\r\n"] {
            assert!(!is_atom(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_search_word_can_ask_for_a_tag() {
        assert_eq!(searched("tag:Work").as_deref(), Some("work"));
        assert_eq!(searched("TAG:work").as_deref(), Some("work"));
        assert_eq!(searched("tag:"), None);
        assert_eq!(searched("tags"), None);
        assert_eq!(searched("work"), None);
        assert_eq!(searched("tàg:x"), None);
    }
}
