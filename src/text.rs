//! Text from other people, made safe to show and copy: everything in a
//! message (names, subjects, bodies, file names) comes from its sender.

/// Characters left out of text from mail:
/// - control characters other than line breaks and tabs. The screen drops
///   them anyway, but copied text keeps them, and a hidden `\r` or escape
///   can run a command when pasted into a shell or vim.
/// - bidi overrides and isolates, which can make text read backwards:
///   "invoice<U+202E>fdp.exe" shows as `invoiceexe.pdf` in some terminals,
///   and the bidi marks.
/// - invisible characters that terminals and the layout disagree on: the
///   layout counts them as no columns, while some terminals move the cursor
///   on, which pushes the rest of the row out of place. These are the Hangul
///   fillers of "invisible" names, the soft hyphen, zero-width
///   spaces and the like. Joiners and variation selectors stay, since emoji
///   need them.
/// - the line and paragraph separators (U+2028, U+2029): not control
///   characters, but a terminal that breaks a line on them would push the
///   rest of a message out of the pane the layout drew it in.
/// - the private character kitty uses to place images, so text can't pose
///   as one.
pub fn is_hidden(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(
            c,
            '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{061C}'
                | '\u{00AD}'
                | '\u{034F}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{2800}'
                | '\u{200B}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{2060}'..='\u{2064}'
                | '\u{206A}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0001}'
                | '\u{10EEEE}'
        )
}

/// `text` without the characters [`is_hidden`] leaves out.
pub fn clean(text: &str) -> String {
    text.chars().filter(|&c| !is_hidden(c)).collect()
}

/// `text` cleaned, on one line (every run of whitespace one space), and at
/// most `max` characters, ending with `…` if cut: for names, subjects and
/// file names, which a sender can make as long and as many lines as they
/// like.
pub fn one_line(text: &str, max: usize) -> String {
    // Cut first, so a huge header costs nothing more to clean.
    let text = first_chars(text, max.saturating_mul(8).max(256));
    let mut out = String::new();
    for word in clean(text).split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.chars().count() > max {
        out = first_chars(&out, max.saturating_sub(1)).to_string();
        out.push('…');
    }
    out
}

/// The first `n` characters of `text`.
pub fn first_chars(text: &str, n: usize) -> &str {
    text.char_indices().nth(n).map_or(text, |(i, _)| &text[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_and_bidi_overrides_are_dropped_but_line_breaks_and_tabs_stay() {
        assert_eq!(clean("invoice\u{202E}fdp.exe"), "invoicefdp.exe");
        assert_eq!(clean("a\u{1b}[2Jb\rc\u{7}d\u{9b}e"), "a[2Jbcde");
        assert_eq!(clean("one\ntwo\tthree"), "one\ntwo\tthree");
        assert_eq!(clean("עברית and العربية"), "עברית and العربية");
        // Line and paragraph separators go, so a body can't break its pane.
        assert_eq!(clean("legit\u{2028}spoof\u{2029}more"), "legitspoofmore");
    }

    #[test]
    fn invisible_fillers_go_but_emoji_keep_their_joiners_and_selectors() {
        let filler = format!("x{}SPOOF", "\u{3164}".repeat(40));
        assert_eq!(clean(&filler), "xSPOOF");
        assert_eq!(clean("a\u{FFA0}\u{00AD}\u{200B}\u{FEFF}b"), "ab");
        // A blank Braille cell and Mongolian selectors look like nothing.
        assert_eq!(clean("a\u{2800}\u{2800}b\u{180B}c"), "abc");
        let family = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(clean(family), family);
        assert_eq!(clean("❤\u{FE0F}"), "❤\u{FE0F}");
        let england = "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        assert_eq!(clean(england), england);
    }

    #[test]
    fn one_line_collapses_whitespace_and_caps_the_length() {
        assert_eq!(
            one_line("  Re:\r\n  lunch\t tomorrow? ", 50),
            "Re: lunch tomorrow?"
        );
        assert_eq!(one_line("abcdef", 4), "abc…");
        assert_eq!(one_line("a\u{202E}b", 10), "ab");
        assert_eq!(one_line(&"x ".repeat(10_000), 5), "x x …");
        assert_eq!(one_line(&format!("{}hi", " ".repeat(100)), 5), "hi");
    }

    #[test]
    fn first_chars_stops_on_a_character_boundary() {
        assert_eq!(first_chars("héllo", 2), "hé");
        assert_eq!(first_chars("hi", 5), "hi");
    }
}
