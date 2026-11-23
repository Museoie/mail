//! HTML character references (mails/entities.ts).
//!
//! Decoding for text content, following the HTML parsing rules: named
//! references with a semicolon, the legacy names that also decode without one
//! (`&amp`, `&nbsp`), and decimal or hexadecimal references. Named references
//! cover the HTML 4 set; any other name is left as written.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

const NAMED_TABLE: &str = "aacute:225 Aacute:193 acirc:226 Acirc:194 acute:180 aelig:230 AElig:198 agrave:224 \
    Agrave:192 alefsym:8501 alpha:945 Alpha:913 amp:38 AMP:38 and:8743 ang:8736 apos:39 \
    aring:229 Aring:197 asymp:8776 atilde:227 Atilde:195 auml:228 Auml:196 bdquo:8222 \
    beta:946 Beta:914 brvbar:166 bull:8226 cap:8745 ccedil:231 Ccedil:199 cedil:184 \
    cent:162 chi:967 Chi:935 circ:710 clubs:9827 cong:8773 copy:169 COPY:169 crarr:8629 \
    cup:8746 curren:164 dagger:8224 Dagger:8225 darr:8595 dArr:8659 deg:176 delta:948 \
    Delta:916 diams:9830 divide:247 eacute:233 Eacute:201 ecirc:234 Ecirc:202 egrave:232 \
    Egrave:200 empty:8709 emsp:8195 ensp:8194 epsilon:949 Epsilon:917 equiv:8801 eta:951 \
    Eta:919 eth:240 ETH:208 euml:235 Euml:203 euro:8364 exist:8707 fnof:402 forall:8704 \
    frac12:189 frac14:188 frac34:190 frasl:8260 gamma:947 Gamma:915 ge:8805 gt:62 GT:62 \
    harr:8596 hArr:8660 hearts:9829 hellip:8230 iacute:237 Iacute:205 icirc:238 \
    Icirc:206 iexcl:161 igrave:236 Igrave:204 image:8465 infin:8734 int:8747 iota:953 \
    Iota:921 iquest:191 isin:8712 iuml:239 Iuml:207 kappa:954 Kappa:922 lambda:955 \
    Lambda:923 lang:10216 laquo:171 larr:8592 lArr:8656 lceil:8968 ldquo:8220 le:8804 \
    lfloor:8970 lowast:8727 loz:9674 lrm:8206 lsaquo:8249 lsquo:8216 lt:60 LT:60 \
    macr:175 mdash:8212 micro:181 middot:183 minus:8722 mu:956 Mu:924 nabla:8711 \
    nbsp:160 ndash:8211 ne:8800 ni:8715 not:172 notin:8713 nsub:8836 ntilde:241 \
    Ntilde:209 nu:957 Nu:925 oacute:243 Oacute:211 ocirc:244 Ocirc:212 oelig:339 \
    OElig:338 ograve:242 Ograve:210 oline:8254 omega:969 Omega:937 omicron:959 \
    Omicron:927 oplus:8853 or:8744 ordf:170 ordm:186 oslash:248 Oslash:216 otilde:245 \
    Otilde:213 otimes:8855 ouml:246 Ouml:214 para:182 part:8706 permil:8240 perp:8869 \
    phi:966 Phi:934 pi:960 Pi:928 piv:982 plusmn:177 pound:163 prime:8242 Prime:8243 \
    prod:8719 prop:8733 psi:968 Psi:936 quot:34 QUOT:34 radic:8730 rang:10217 raquo:187 \
    rarr:8594 rArr:8658 rceil:8969 rdquo:8221 real:8476 reg:174 REG:174 rfloor:8971 \
    rho:961 Rho:929 rlm:8207 rsaquo:8250 rsquo:8217 sbquo:8218 scaron:353 Scaron:352 \
    sdot:8901 sect:167 shy:173 sigma:963 Sigma:931 sigmaf:962 sim:8764 spades:9824 \
    sub:8834 sube:8838 sum:8721 sup:8835 sup1:185 sup2:178 sup3:179 supe:8839 szlig:223 \
    tau:964 Tau:932 there4:8756 theta:952 Theta:920 thetasym:977 thinsp:8201 thorn:254 \
    THORN:222 tilde:732 times:215 trade:8482 uacute:250 Uacute:218 uarr:8593 uArr:8657 \
    ucirc:251 Ucirc:219 ugrave:249 Ugrave:217 uml:168 upsih:978 upsilon:965 Upsilon:933 \
    uuml:252 Uuml:220 weierp:8472 xi:958 Xi:926 yacute:253 Yacute:221 yen:165 yuml:255 \
    Yuml:376 zeta:950 Zeta:918 zwj:8205 zwnj:8204";

static NAMED: LazyLock<HashMap<&'static str, char>> = LazyLock::new(|| {
    NAMED_TABLE
        .split(' ')
        .filter_map(|pair| {
            let (name, code) = pair.split_once(':')?;
            Some((name, char::from_u32(code.parse().ok()?)?))
        })
        .collect()
});

/// Names that also decode without a semicolon.
const LEGACY_TABLE: &str = "Aacute Agrave Atilde Ccedil Eacute Egrave Iacute Igrave Ntilde Oacute Ograve Oslash \
    Otilde Uacute Ugrave Yacute aacute agrave atilde brvbar ccedil curren divide eacute \
    egrave frac12 frac14 frac34 iacute igrave iquest middot ntilde oacute ograve oslash \
    otilde plusmn uacute ugrave yacute AElig Acirc Aring Ecirc Icirc Ocirc THORN Ucirc acirc \
    acute aelig aring cedil ecirc icirc iexcl laquo micro ocirc pound raquo szlig thorn \
    times ucirc Auml COPY Euml Iuml Ouml QUOT Uuml auml cent copy euml iuml macr nbsp ordf \
    ordm ouml para quot sect sup1 sup2 sup3 uuml yuml AMP ETH REG amp deg eth not reg shy \
    uml yen GT LT gt lt";

static LEGACY: LazyLock<HashSet<&'static str>> = LazyLock::new(|| LEGACY_TABLE.split(' ').collect());

/// The longest legacy name.
const LEGACY_MAX: usize = 6;

/// Numeric references the HTML parser remaps (NUL and the Windows-1252 range).
fn numeric_remap(code_point: u32) -> u32 {
    match code_point {
        0 => 65533,
        128 => 8364,
        130 => 8218,
        131 => 402,
        132 => 8222,
        133 => 8230,
        134 => 8224,
        135 => 8225,
        136 => 710,
        137 => 8240,
        138 => 352,
        139 => 8249,
        140 => 338,
        142 => 381,
        145 => 8216,
        146 => 8217,
        147 => 8220,
        148 => 8221,
        149 => 8226,
        150 => 8211,
        151 => 8212,
        152 => 732,
        153 => 8482,
        154 => 353,
        155 => 8250,
        156 => 339,
        158 => 382,
        159 => 376,
        other => other,
    }
}

/// The character a numeric reference's digits name; U+FFFD for a surrogate
/// or anything past the last code point.
fn from_digits(digits: &str, radix: u32) -> char {
    let mut code_point: u32 = 0;
    for digit in digits.chars().filter_map(|c| c.to_digit(radix)) {
        code_point = code_point.saturating_mul(radix).saturating_add(digit);
    }
    match code_point {
        0xd800..=0xdfff => '\u{fffd}',
        _ => char::from_u32(numeric_remap(code_point)).unwrap_or('\u{fffd}'),
    }
}

/// A character reference candidate: `&#x…`, `&#…` or `&name`, each with or
/// without its semicolon.
enum Reference<'a> {
    Hex(&'a str),
    Decimal(&'a str),
    Named(&'a str),
}

/// The reference starting at the `&` at `at`, with where it ends and whether
/// a semicolon closes it.
fn reference_at(text: &str, at: usize) -> Option<(Reference<'_>, usize, bool)> {
    let bytes = text.as_bytes();
    let run = |from: usize, accept: fn(&u8) -> bool| from + bytes[from..].iter().take_while(|b| accept(b)).count();
    let (reference, end) = if bytes.get(at + 1) == Some(&b'#') {
        if matches!(bytes.get(at + 2), Some(b'x' | b'X')) && bytes.get(at + 3).is_some_and(u8::is_ascii_hexdigit) {
            let end = run(at + 3, u8::is_ascii_hexdigit);
            (Reference::Hex(&text[at + 3..end]), end)
        } else {
            let end = run(at + 2, u8::is_ascii_digit);
            if end == at + 2 {
                return None;
            }
            (Reference::Decimal(&text[at + 2..end]), end)
        }
    } else {
        let end = run(at + 1, u8::is_ascii_alphanumeric);
        if end == at + 1 {
            return None;
        }
        (Reference::Named(&text[at + 1..end]), end)
    };
    let closed = bytes.get(end) == Some(&b';');
    Some((reference, end + usize::from(closed), closed))
}

/// The longest legacy name starting `name`, unless it is all of `name` and a
/// semicolon follows.
fn legacy_prefix(name: &str, semicolon: bool) -> Option<&str> {
    (2..=LEGACY_MAX.min(name.len()))
        .rev()
        .map(|length| &name[..length])
        .find(|candidate| LEGACY.contains(candidate) && (candidate.len() < name.len() || !semicolon))
}

fn decode_with<F>(text: &str, mut decode: F) -> String
where
    F: FnMut(Reference, &str, bool, &mut String),
{
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut at = 0;
    while let Some(found) = bytes[at..].iter().position(|&b| b == b'&') {
        let start = at + found;
        at = start + 1;
        let Some((reference, end, closed)) = reference_at(text, start) else {
            continue;
        };
        out.push_str(&text[last..start]);
        decode(reference, &text[start..end], closed, &mut out);
        last = end;
        at = end;
    }
    out.push_str(&text[last..]);
    out
}

/// Decodes an attribute value. Inside an attribute only references closed by
/// a semicolon decode, so a query string's `&copy=2` stays as written.
pub fn decode_attribute_entities(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    decode_with(value, |reference, written, closed, out| match reference {
        _ if !closed => out.push_str(written),
        Reference::Hex(digits) => out.push(from_digits(digits, 16)),
        Reference::Decimal(digits) => out.push(from_digits(digits, 10)),
        Reference::Named(name) => match NAMED.get(name) {
            Some(&named) => out.push(named),
            None => out.push_str(written),
        },
    })
}

/// Decodes the character references of text content.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    decode_with(text, |reference, written, closed, out| match reference {
        Reference::Hex(digits) => out.push(from_digits(digits, 16)),
        Reference::Decimal(digits) => out.push(from_digits(digits, 10)),
        Reference::Named(name) => {
            if let Some(&named) = NAMED.get(name).filter(|_| closed) {
                out.push(named);
            } else if let Some(legacy) = legacy_prefix(name, closed) {
                out.push(NAMED[legacy]);
                out.push_str(&written[1 + legacy.len()..]);
            } else {
                out.push_str(written);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_named_references() {
        assert_eq!(
            decode_entities("&amp; &lt; &gt; &quot; &apos; &nbsp; &copy; &hellip; &euro;"),
            "& < > \" ' \u{a0} © … €"
        );
    }

    #[test]
    fn decodes_legacy_names_without_a_semicolon_keeping_the_next_character() {
        assert_eq!(
            decode_entities("&amp &ampx; a=1&copy=2 &lt3 &frac12x"),
            "& &x; a=1©=2 <3 ½x"
        );
    }

    #[test]
    fn decodes_decimal_and_hexadecimal_references() {
        assert_eq!(decode_entities("&#65;&#x42;&#X43;&#68 &#x1F600;"), "ABCD 😀");
    }

    #[test]
    fn remaps_references_the_html_parser_treats_specially() {
        assert_eq!(
            decode_entities("&#0; &#150; &#xD800; &#99999999;"),
            "\u{fffd} – \u{fffd} \u{fffd}"
        );
    }

    #[test]
    fn leaves_anything_else_as_written() {
        assert_eq!(
            decode_entities("&; & &#; &#x; AT&T &unknown; &check;"),
            "&; & &#; &#x; AT&T &unknown; &check;"
        );
    }

    #[test]
    fn attribute_values_decode_only_references_closed_by_a_semicolon() {
        assert_eq!(decode_attribute_entities("?a=1&amp;b=2&#38;c&#x26;d"), "?a=1&b=2&c&d");
        assert_eq!(
            decode_attribute_entities("?a=1&copy=2&amp=3&#38x"),
            "?a=1&copy=2&amp=3&#38x"
        );
        assert_eq!(decode_attribute_entities("&notaname;"), "&notaname;");
    }

    #[test]
    fn every_legacy_name_is_a_named_reference_no_longer_than_the_limit() {
        for name in LEGACY.iter() {
            assert!(NAMED.contains_key(name), "{name}");
            assert!(name.len() <= LEGACY_MAX, "{name}");
        }
        assert!(LEGACY.iter().any(|name| name.len() == LEGACY_MAX));
        assert_eq!(NAMED.len(), NAMED_TABLE.split(' ').count());
    }
}
