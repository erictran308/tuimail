//! IMAP's modified UTF-7 (RFC 3501 §5.1.3), the way folder names are
//! spelled on the wire: "Entw&APw-rfe" is "Entwürfe".

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

/// The folder name as text; `None` if it isn't valid modified UTF-7 (then
/// it's shown as the server sent it).
pub fn decode(name: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = name;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let end = rest.find('-')?;
        let encoded = &rest[..end];
        rest = &rest[end + 1..];
        if encoded.is_empty() {
            out.push('&');
            continue;
        }
        let mut bits = 0u32;
        let mut count = 0;
        let mut units = Vec::new();
        for b in encoded.bytes() {
            let value = ALPHABET.iter().position(|&a| a == b)? as u32;
            bits = (bits << 6) | value;
            count += 6;
            if count >= 16 {
                count -= 16;
                units.push((bits >> count) as u16);
                bits &= (1 << count) - 1;
            }
        }
        // Leftover bits must be zero padding, less than a 16-bit unit.
        if count >= 6 || bits != 0 {
            return None;
        }
        out.push_str(&String::from_utf16(&units).ok()?);
    }
    out.push_str(rest);
    Some(out)
}

/// `name` in modified UTF-7, for a tag's keyword.
pub fn encode(name: &str) -> String {
    let mut out = String::new();
    let mut pending: Vec<u16> = Vec::new();
    let flush = |pending: &mut Vec<u16>, out: &mut String| {
        if pending.is_empty() {
            return;
        }
        out.push('&');
        let bytes: Vec<u8> = pending.iter().flat_map(|u| u.to_be_bytes()).collect();
        let mut bits = 0u32;
        let mut count = 0;
        for b in bytes {
            bits = (bits << 8) | u32::from(b);
            count += 8;
            while count >= 6 {
                count -= 6;
                out.push(ALPHABET[((bits >> count) & 63) as usize] as char);
            }
        }
        if count > 0 {
            out.push(ALPHABET[((bits << (6 - count)) & 63) as usize] as char);
        }
        out.push('-');
        pending.clear();
    };
    for c in name.chars() {
        if (' '..='~').contains(&c) {
            flush(&mut pending, &mut out);
            if c == '&' {
                out.push_str("&-");
            } else {
                out.push(c);
            }
        } else {
            let mut buf = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut pending, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_decode_and_encode_both_ways() {
        for (wire, text) in [
            ("INBOX", "INBOX"),
            ("Entw&APw-rfe", "Entwürfe"),
            ("&-", "&"),
            ("Tom &- Jerry", "Tom & Jerry"),
            ("&ZeVnLIqe-", "日本語"),
            (
                "[Gmail]/&BB4EQgQ,BEAEMAQyBDsENQQ9BD0ESwQ1-",
                "[Gmail]/Отправленные",
            ),
            ("&2D3eAA-", "😀"),
        ] {
            assert_eq!(decode(wire).as_deref(), Some(text), "{wire}");
            assert_eq!(encode(text), wire, "{text}");
        }
    }

    #[test]
    fn broken_names_are_not_decoded() {
        assert_eq!(decode("&unclosed"), None);
        assert_eq!(decode("&!!-"), None);
        assert_eq!(decode("&AB-"), None, "bits left over");
        assert_eq!(decode("&2D0-"), None, "half a surrogate pair");
    }
}
