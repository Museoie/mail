//! Charset labels and decoding (mime/charset.ts).

use encoding_rs::{Encoding, UTF_16BE, UTF_16LE};

/// JavaScript string semantics the mail text code depends on: what `\s`,
/// `trim()` and a UTF-16 `slice` mean. They differ from Rust's in the corners
/// (`trim` keeps U+0085 and drops U+FEFF), and a difference there changes
/// stored text.
pub(crate) mod jsstr {
    /// A character `\s` matches, which is also what `trim()` strips.
    pub fn is_space(c: char) -> bool {
        matches!(
            c,
            '\u{9}'..='\u{d}'
                | ' '
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    }

    /// A character `\w` matches.
    pub fn is_word(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_'
    }

    /// A character `.` does not match.
    pub fn is_line_terminator(c: char) -> bool {
        matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
    }

    /// `string.trim()`.
    pub fn trim(s: &str) -> &str {
        s.trim_matches(is_space)
    }

    /// `string.trimStart()`.
    pub fn trim_start(s: &str) -> &str {
        s.trim_start_matches(is_space)
    }

    /// `string.trimEnd()`.
    pub fn trim_end(s: &str) -> &str {
        s.trim_end_matches(is_space)
    }

    /// `string.length`.
    pub fn len(s: &str) -> usize {
        s.chars().map(char::len_utf16).sum()
    }

    /// `string.slice(0, end)` with `end` in UTF-16 code units. A cut through
    /// a surrogate pair leaves a lone surrogate in JavaScript, which is
    /// U+FFFD as soon as the string is stored or sent, so it is U+FFFD here.
    pub fn truncate(s: &str, end: usize) -> String {
        let mut units = 0;
        for (at, c) in s.char_indices() {
            let width = c.len_utf16();
            if units + width > end {
                let mut out = s[..at].to_string();
                if units < end {
                    out.push('\u{fffd}');
                }
                return out;
            }
            units += width;
        }
        s.to_string()
    }
}

/// Labels mail software writes that the WHATWG Encoding Standard does not
/// list, mapped onto the encoding they mean.
fn label_alias(label: &str) -> Option<&'static str> {
    Some(match label {
        "cp949" | "uhc" | "windows-949" | "ks_c_5601-1989" | "x-windows-949" => "euc-kr",
        "shift-jis" => "shift_jis",
        "euc-cn" | "x-gbk" => "gbk",
        "big5-hkscs" => "big5",
        _ => return None,
    })
}

/// `new TextDecoder(label)`: the encoding a WHATWG label names, or `None`
/// where the constructor throws (an unknown label, or `replacement`).
pub(crate) fn text_decoder(label: &str) -> Option<&'static Encoding> {
    Encoding::for_label_no_replacement(label.as_bytes())
}

/// `decoder.decode(bytes)`: malformed input becomes U+FFFD and the
/// encoding's own byte order mark is dropped.
pub(crate) fn text_decode(encoding: &'static Encoding, bytes: &[u8]) -> String {
    encoding.decode_with_bom_removal(bytes).0.into_owned()
}

/// `Buffer.from(bytes).toString("utf8")`: like a UTF-8 `TextDecoder`, but a
/// byte order mark stays in the text.
pub(crate) fn utf8_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `prefix`, an optional `-` or `_`, then digits: the digits and what
/// follows them.
fn after_prefix_digits<'a>(label: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let rest = label.strip_prefix(prefix)?;
    let rest = rest.strip_prefix(['-', '_']).unwrap_or(rest);
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then(|| rest.split_at(digits))
}

/// Spelling variants (`utf_8`, `win1252`, `latin-2`, `iso8859_15`, `l2`)
/// folded onto their canonical label.
fn normalized_label(label: &str) -> String {
    let mut label = label.to_string();
    if let Some((digits, rest)) = after_prefix_digits(&label, "utf") {
        label = format!("utf-{digits}{rest}");
    }
    let windows = after_prefix_digits(&label, "windows").or_else(|| after_prefix_digits(&label, "win"));
    if let Some((digits, rest)) = windows {
        label = format!("windows-{digits}{rest}");
    }
    let iso = after_prefix_digits(&label, "latin").or_else(|| {
        let rest = label.strip_prefix("iso")?;
        let rest = rest.strip_prefix(['-', '_']).unwrap_or(rest);
        after_prefix_digits(rest, "8859")
    });
    if let Some((digits, rest)) = iso {
        label = format!("iso-8859-{digits}{rest}");
    }
    if let Some((digits, "")) = after_prefix_digits(&label, "l") {
        label = format!("iso-8859-{digits}");
    }
    label
}

/// The decoder for a MIME charset label, or `None` when no encoding answers
/// to it.
fn decoder_for(charset: &str) -> Option<&'static Encoding> {
    let label = jsstr::trim(charset).to_lowercase();
    text_decoder(label_alias(&label).unwrap_or(&label)).or_else(|| text_decoder(&normalized_label(&label)))
}

/// Most code units looked at to tell the byte order of unmarked UTF-16.
const UTF16_SNIFF_UNITS: usize = 100;

/// Unmarked `utf-16` takes its byte order from a BOM, else from which half
/// of each leading code unit is zero (ASCII-range text), else little-endian.
fn decode_utf16(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xfe, 0xff]) {
        return text_decode(UTF_16BE, bytes);
    }
    if bytes.starts_with(&[0xff, 0xfe]) {
        return text_decode(UTF_16LE, bytes);
    }
    let mut big_endian_votes = 0;
    let mut little_endian_votes = 0;
    let end = bytes.len().saturating_sub(1).min(UTF16_SNIFF_UNITS * 2);
    for unit in bytes[..end.div_ceil(2) * 2].chunks_exact(2) {
        if unit[0] == 0 && unit[1] != 0 {
            big_endian_votes += 1;
        } else if unit[0] != 0 && unit[1] == 0 {
            little_endian_votes += 1;
        }
    }
    let encoding = if big_endian_votes > little_endian_votes {
        UTF_16BE
    } else {
        UTF_16LE
    };
    text_decode(encoding, bytes)
}

/// `/^(?:utf-?8|7bit)$/i`
fn is_utf8_label(label: &str) -> bool {
    ["utf8", "utf-8", "7bit"]
        .iter()
        .any(|known| label.eq_ignore_ascii_case(known))
}

/// `/^utf[-_]?16$/i`
fn is_utf16_label(label: &str) -> bool {
    ["utf16", "utf-16", "utf_16"]
        .iter()
        .any(|known| label.eq_ignore_ascii_case(known))
}

/// Decodes header bytes (an RFC 2047 encoded word) in `charset`. Unknown
/// labels fall back to UTF-8; `us-ascii` follows the Encoding Standard and
/// reads as windows-1252.
pub fn decode_header_bytes(bytes: &[u8], charset: &str) -> String {
    let label = jsstr::trim(charset);
    if is_utf8_label(label) {
        return utf8_lossy(bytes);
    }
    if is_utf16_label(label) {
        return decode_utf16(bytes);
    }
    match decoder_for(charset) {
        Some(encoding) => text_decode(encoding, bytes),
        None => utf8_lossy(bytes),
    }
}

/// Decodes a text body part declared as `charset`. `us-ascii` is read as
/// UTF-8 here, unlike in headers: a body labelled ASCII that carries 8-bit
/// octets is overwhelmingly UTF-8 from a client that did not relabel it.
pub fn decode_body_bytes(bytes: &[u8], charset: Option<&str>) -> String {
    let charset = charset.unwrap_or("utf-8");
    let label: String = charset
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect();
    match label.as_str() {
        "ascii" | "usascii" | "utf8" => return utf8_lossy(bytes),
        "utf16" => return decode_utf16(bytes),
        _ => {}
    }
    match decoder_for(charset) {
        Some(encoding) => text_decode(encoding, bytes),
        None => utf8_lossy(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIG_ENDIAN_BOM: [u8; 6] = [0xfe, 0xff, 0x00, 0x68, 0x00, 0x69];
    const LITTLE_ENDIAN_BOM: [u8; 6] = [0xff, 0xfe, 0x68, 0x00, 0x69, 0x00];
    const UNMARKED: [u8; 4] = [0x00, 0x68, 0x00, 0x69];

    #[test]
    fn takes_the_byte_order_from_a_bom_and_drops_the_bom() {
        assert_eq!(decode_body_bytes(&BIG_ENDIAN_BOM, Some("utf-16")), "hi");
        assert_eq!(decode_body_bytes(&LITTLE_ENDIAN_BOM, Some("UTF-16")), "hi");
        assert_eq!(decode_header_bytes(&BIG_ENDIAN_BOM, "utf-16"), "hi");
        assert_eq!(decode_header_bytes(&LITTLE_ENDIAN_BOM, "utf16"), "hi");
    }

    #[test]
    fn infers_the_byte_order_of_unmarked_text_from_its_zero_bytes() {
        let unmarked_little_endian = [0x68, 0x00, 0x69, 0x00];
        assert_eq!(decode_body_bytes(&UNMARKED, Some("utf-16")), "hi");
        assert_eq!(decode_body_bytes(&UNMARKED, Some("utf_16")), "hi");
        assert_eq!(decode_header_bytes(&UNMARKED, "UTF-16"), "hi");
        assert_eq!(decode_body_bytes(&unmarked_little_endian, Some("utf-16")), "hi");
        assert_eq!(decode_header_bytes(&unmarked_little_endian, "UTF-16"), "hi");
    }

    #[test]
    fn reads_unmarked_text_with_no_ascii_range_units_as_little_endian() {
        assert_eq!(decode_body_bytes(&[0x5c, 0xd5, 0x6d, 0xad], Some("utf-16")), "한국");
    }

    #[test]
    fn keeps_an_explicitly_labelled_byte_order() {
        assert_eq!(decode_body_bytes(&[0x68, 0x00, 0x69, 0x00], Some("utf-16le")), "hi");
        assert_eq!(decode_body_bytes(&UNMARKED, Some("utf-16be")), "hi");
    }

    #[test]
    fn folds_spelling_variants_onto_their_label() {
        assert_eq!(normalized_label("utf_8"), "utf-8");
        assert_eq!(normalized_label("win1252"), "windows-1252");
        assert_eq!(normalized_label("windows_1251"), "windows-1251");
        assert_eq!(normalized_label("latin-2"), "iso-8859-2");
        assert_eq!(normalized_label("iso8859_15"), "iso-8859-15");
        assert_eq!(normalized_label("iso_885915"), "iso-8859-15");
        assert_eq!(normalized_label("l-9"), "iso-8859-9");
        assert_eq!(normalized_label("l9x"), "l9x");
        assert_eq!(normalized_label("koi8-r"), "koi8-r");
    }

    #[test]
    fn javascript_trim_and_slice() {
        assert_eq!(jsstr::trim("\u{feff} a\u{a0}"), "a");
        assert_eq!(jsstr::trim("\u{85}a\u{85}"), "\u{85}a\u{85}");
        assert_eq!(jsstr::truncate("a😀b", 1), "a");
        assert_eq!(jsstr::truncate("a😀b", 2), "a\u{fffd}");
        assert_eq!(jsstr::truncate("a😀b", 3), "a😀");
        assert_eq!(jsstr::truncate("a😀b", 9), "a😀b");
    }
}
