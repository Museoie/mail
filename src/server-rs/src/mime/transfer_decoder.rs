//! Base64 and quoted-printable decoding (mime/transfer-decoder.ts).

/// Incremental Content-Transfer-Encoding decoder. `write` appends whatever is
/// already decodable to `out` and keeps only the few bytes that are not, so a
/// part of any size passes through in constant memory.
#[derive(Debug)]
pub enum TransferDecoder {
    Base64(Base64Decoder),
    QuotedPrintable(QuotedPrintableDecoder),
    /// 7bit, 8bit, binary and anything unrecognized pass through.
    Identity,
}

impl TransferDecoder {
    pub fn write(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        match self {
            TransferDecoder::Base64(decoder) => decoder.write(chunk, out),
            TransferDecoder::QuotedPrintable(decoder) => decoder.write(chunk, out),
            TransferDecoder::Identity => out.extend_from_slice(chunk),
        }
    }

    /// Appends what was held back at the end of the input.
    pub fn end(&mut self, out: &mut Vec<u8>) {
        match self {
            TransferDecoder::Base64(decoder) => decoder.flush_partial(out),
            TransferDecoder::QuotedPrintable(decoder) => decoder.end(out),
            TransferDecoder::Identity => {}
        }
    }
}

fn base64_value(byte: u8) -> Option<u32> {
    Some(match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    } as u32)
}

/// Base64 (RFC 2045 §6.8). Characters outside the alphabet — line breaks,
/// stray whitespace — are skipped, and decoding stops being strict at `=`:
/// padding only ends the current quantum, so concatenated encodings decode
/// back to back.
#[derive(Debug, Default)]
pub struct Base64Decoder {
    quantum: u32,
    filled: u8,
}

impl Base64Decoder {
    fn write(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        out.reserve(chunk.len() * 3 / 4 + 3);
        for &byte in chunk {
            if byte == b'=' {
                self.flush_partial(out);
                continue;
            }
            let Some(value) = base64_value(byte) else {
                continue;
            };
            self.quantum = self.quantum << 6 | value;
            self.filled += 1;
            if self.filled == 4 {
                out.extend_from_slice(&self.quantum.to_be_bytes()[1..]);
                self.quantum = 0;
                self.filled = 0;
            }
        }
    }

    /// Emits the whole octets of an incomplete quantum (2 or 3 characters).
    fn flush_partial(&mut self, out: &mut Vec<u8>) {
        match self.filled {
            2 => out.push((self.quantum >> 4) as u8),
            3 => out.extend_from_slice(&[(self.quantum >> 10) as u8, (self.quantum >> 2) as u8]),
            _ => {}
        }
        self.quantum = 0;
        self.filled = 0;
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

fn decode_escapes(bytes: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push(high << 4 | low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
}

/// Longest unterminated line held back before its decodable prefix is released.
const MAX_QP_CARRY: usize = 64 * 1024;

/// Quoted-printable (RFC 2045 §6.7): trailing whitespace before a line break
/// is dropped, `=` at the end of a line joins it to the next, and `=XX` is
/// one octet. Octets that arrive unencoded pass through as they are.
#[derive(Debug, Default)]
pub struct QuotedPrintableDecoder {
    carry: Vec<u8>,
}

impl QuotedPrintableDecoder {
    fn write(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        let joined;
        let data = if self.carry.is_empty() {
            chunk
        } else {
            self.carry.extend_from_slice(chunk);
            joined = std::mem::take(&mut self.carry);
            &joined[..]
        };
        let mut start = 0;
        while let Some(found) = data[start..].iter().position(|&b| b == b'\n') {
            decode_line(&data[start..start + found], true, out);
            start += found + 1;
        }
        let mut rest = &data[start..];
        if rest.len() > MAX_QP_CARRY {
            let mut cut = rest.len();
            while cut > 0 && matches!(rest[cut - 1], b' ' | b'\t' | b'\r') {
                cut -= 1;
            }
            // An escape cut in two stays whole in the carry.
            if cut >= 1 && rest[cut - 1] == b'=' {
                cut -= 1;
            } else if cut >= 2 && rest[cut - 2] == b'=' {
                cut -= 2;
            }
            if cut > 0 {
                decode_escapes(&rest[..cut], out);
                rest = &rest[cut..];
            }
        }
        self.carry = rest.to_vec();
    }

    fn end(&mut self, out: &mut Vec<u8>) {
        let rest = std::mem::take(&mut self.carry);
        if !rest.is_empty() {
            decode_line(&rest, false, out);
        }
    }
}

fn decode_line(line: &[u8], terminated: bool, out: &mut Vec<u8>) {
    let mut end = line.len();
    let has_cr = terminated && line.last() == Some(&b'\r');
    if has_cr {
        end -= 1;
    }
    while end > 0 && matches!(line[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    decode_escapes(&line[..end], out);
    if end > 0 && line[end - 1] == b'=' {
        // A soft line break: the `=` goes and the line runs on.
        out.pop();
        return;
    }
    if terminated {
        out.extend_from_slice(if has_cr { b"\r\n" } else { b"\n" });
    }
}

/// The decoder for a Content-Transfer-Encoding value; anything unrecognized
/// passes through.
pub fn create_transfer_decoder(encoding: &str) -> TransferDecoder {
    match encoding {
        "base64" => TransferDecoder::Base64(Base64Decoder::default()),
        "quoted-printable" => TransferDecoder::QuotedPrintable(QuotedPrintableDecoder::default()),
        _ => TransferDecoder::Identity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `input` in `size`-byte pieces and joins what comes out.
    fn decode_in_pieces(encoding: &str, input: &[u8], size: usize) -> Vec<u8> {
        let mut decoder = create_transfer_decoder(encoding);
        let mut out = Vec::new();
        for piece in input.chunks(size) {
            decoder.write(piece, &mut out);
        }
        decoder.end(&mut out);
        out
    }

    fn base64_encode(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn base64_decodes_the_same_bytes_whatever_the_chunking() {
        let payload: Vec<u8> = (0..1000).map(|i| ((i * 37) % 256) as u8).collect();
        let encoded: String = base64_encode(&payload)
            .as_bytes()
            .chunks(76)
            .map(|line| format!("{}\r\n", String::from_utf8_lossy(line)))
            .collect();
        for size in [1, 2, 3, 5, 77, 4096] {
            assert_eq!(decode_in_pieces("base64", encoded.as_bytes(), size), payload);
        }
    }

    #[test]
    fn base64_skips_characters_outside_the_alphabet() {
        assert_eq!(decode_in_pieces("base64", b"QU\r\nJD !REVG", 3), b"ABCDEF");
    }

    #[test]
    fn base64_decodes_padded_quanta_and_concatenated_encodings() {
        assert_eq!(decode_in_pieces("base64", b"QQ==", 1), b"A");
        assert_eq!(decode_in_pieces("base64", b"QUI=QUI=", 2), b"ABAB");
        assert_eq!(decode_in_pieces("base64", b"QUI", 1), b"AB");
    }

    #[test]
    fn quoted_printable_decodes_escapes_and_joins_soft_line_breaks() {
        assert_eq!(
            decode_in_pieces("quoted-printable", b"caf=C3=A9 au=\r\n lait\r\n", 1),
            "café au lait\r\n".as_bytes()
        );
    }

    #[test]
    fn quoted_printable_drops_whitespace_before_a_line_break_but_keeps_it_inside_a_line() {
        assert_eq!(
            decode_in_pieces("quoted-printable", b"a  b   \r\nc \t\nd  ", 4),
            b"a  b\r\nc\nd"
        );
    }

    #[test]
    fn quoted_printable_leaves_an_incomplete_or_invalid_escape_as_written() {
        assert_eq!(decode_in_pieces("quoted-printable", b"=ZZ x=4", 2), b"=ZZ x=4");
        assert_eq!(decode_in_pieces("quoted-printable", b"trailing=", 3), b"trailing");
    }

    #[test]
    fn quoted_printable_passes_unencoded_8_bit_octets_through_unchanged() {
        let raw = [b"x ".as_slice(), "한".as_bytes(), b"=41\r\n"].concat();
        assert_eq!(decode_in_pieces("quoted-printable", &raw, 1), "x 한A\r\n".as_bytes());
    }

    #[test]
    fn quoted_printable_decodes_a_line_longer_than_its_carry_limit_without_losing_an_escape_at_the_cut() {
        let line = format!("{}=42", "=41".repeat(30000));
        let decoded = decode_in_pieces("quoted-printable", line.as_bytes(), 50000);
        assert_eq!(decoded, format!("{}B", "A".repeat(30000)).as_bytes());
    }

    #[test]
    fn identity_passes_7bit_8bit_and_binary_through() {
        assert_eq!(decode_in_pieces("8bit", b"abc\r\n", 2), b"abc\r\n");
    }
}
