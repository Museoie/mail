//! One-line body previews (mails/snippet.ts).
//!
//! Built from the stored plain-text body: the headers query selects a bounded
//! prefix of the `text` column and this strips quoted history from it.

use crate::mime::charset::jsstr;

pub const SNIPPET_MAX_LENGTH: usize = 160;

fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len() && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// `/^\s*>/`
fn is_quoted_line(line: &str) -> bool {
    jsstr::trim_start(line).starts_with('>')
}

/// `^\s*<opening>\b.+<closing>` with `closing` running to the end of `line`:
/// an attribution line such as "On … wrote:". `.` stops at a line terminator.
fn is_attribution_line(line: &str, opening: &str, closing: &str) -> bool {
    let Some(rest) = jsstr::trim_start(line).strip_prefix(opening) else {
        return false;
    };
    let Some(between) = rest.strip_suffix(closing) else {
        return false;
    };
    between.chars().next().is_some_and(|first| !jsstr::is_word(first)) && !between.contains(jsstr::is_line_terminator)
}

/// `/^\s*On\b.+wrote:\s*$/`
fn is_wrote_line(line: &str) -> bool {
    is_attribution_line(jsstr::trim_end(line), "On", "wrote:")
}

/// `/^\s*Le\b.+a écrit\s*:\s*$/`: the French attribution line ("Le 5 janv.
/// 2021 à 15:00, Jean <jean@x.com> a écrit :").
fn is_wrote_line_fr(line: &str) -> bool {
    let Some(before_colon) = jsstr::trim_end(line).strip_suffix(':') else {
        return false;
    };
    is_attribution_line(jsstr::trim_end(before_colon), "Le", "a écrit")
}

/// `/^(-{2,}\s*(Original Message|Forwarded message)|_{2,}|Begin forwarded message)/i`
fn is_forward_separator(line: &str) -> bool {
    let after_dashes = line.trim_start_matches('-');
    if line.len() - after_dashes.len() >= 2 {
        let title = jsstr::trim_start(after_dashes);
        if starts_with_ignore_ascii_case(title, "Original Message")
            || starts_with_ignore_ascii_case(title, "Forwarded message")
        {
            return true;
        }
    }
    line.starts_with("__") || starts_with_ignore_ascii_case(line, "Begin forwarded message")
}

/// The preview of a plain-text body: its unquoted text on one line, cut at a
/// word boundary. `None` when there is no unquoted content to show.
pub fn build_snippet(text: Option<&str>) -> Option<String> {
    let text = text.filter(|text| !text.is_empty())?;
    let mut words: Vec<&str> = Vec::new();
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        // A line break is `\r?\n`, so a `\r` goes with the break after it.
        let line = match lines.peek() {
            Some(_) => line.strip_suffix('\r').unwrap_or(line),
            None => line,
        };
        if is_forward_separator(line) {
            break;
        }
        if is_quoted_line(line) || is_wrote_line(line) || is_wrote_line_fr(line) {
            continue;
        }
        words.extend(line.split(jsstr::is_space).filter(|word| !word.is_empty()));
    }
    let collapsed = words.join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if jsstr::len(&collapsed) <= SNIPPET_MAX_LENGTH {
        return Some(collapsed);
    }
    let mut cut = jsstr::truncate(&collapsed, SNIPPET_MAX_LENGTH);
    if let Some(last_space) = cut.rfind(' ').filter(|&at| at > 0) {
        cut.truncate(last_space);
    }
    cut.push('…');
    Some(cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(text: &str) -> Option<String> {
        build_snippet(Some(text))
    }

    #[test]
    fn passes_short_plain_text_through_unchanged() {
        assert_eq!(
            snippet("Hello, this is the body.").as_deref(),
            Some("Hello, this is the body.")
        );
    }

    #[test]
    fn collapses_newlines_and_extra_whitespace_into_one_line() {
        assert_eq!(
            snippet("First line\nsecond   line\n\n  third line  ").as_deref(),
            Some("First line second line third line")
        );
    }

    #[test]
    fn strips_quoted_lines_including_nested_chains() {
        assert_eq!(
            snippet("My reply is here.\n> Quoted original\n>> Nested quote\n> More quote").as_deref(),
            Some("My reply is here.")
        );
    }

    #[test]
    fn drops_the_on_wrote_attribution_line() {
        assert_eq!(
            snippet("See below.\nOn Tue, Jan 5, 2021 at 3:00 PM, John <john@x.com> wrote:\n> old text").as_deref(),
            Some("See below.")
        );
    }

    #[test]
    fn drops_a_french_a_ecrit_attribution_line() {
        assert_eq!(
            snippet("See below.\nLe 5 janv. 2021 à 15:00, Jean <jean@x.com> a écrit :\n> old text").as_deref(),
            Some("See below.")
        );
    }

    #[test]
    fn stops_at_an_original_message_separator() {
        assert_eq!(
            snippet("New content here.\n-----Original Message-----\nFrom: old@x.com\nOld body").as_deref(),
            Some("New content here.")
        );
    }

    #[test]
    fn stops_at_a_forwarded_message_separator() {
        assert_eq!(
            snippet("Forwarding this along.\nBegin forwarded message:\nFrom: old@x.com").as_deref(),
            Some("Forwarding this along.")
        );
    }

    #[test]
    fn stops_at_a_gmail_forwarded_message_separator() {
        assert_eq!(
            snippet("See below.\n---------- Forwarded message ---------\nFrom: old@x.com\nOld body").as_deref(),
            Some("See below.")
        );
    }

    #[test]
    fn returns_none_for_a_forward_with_no_added_text() {
        assert_eq!(
            snippet("---------- Forwarded message ---------\nFrom: old@x.com\nOld body"),
            None
        );
    }

    #[test]
    fn truncates_long_text_at_a_word_boundary_with_an_ellipsis() {
        let long: Vec<String> = (0..30).map(|i| format!("word{i}")).collect();
        let long = long.join(" ");
        let snippet = snippet(&long).unwrap();
        assert!(jsstr::len(&snippet) <= SNIPPET_MAX_LENGTH + 1);
        assert!(snippet.ends_with('…'));
        // The cut lands on a word boundary: no partial word before the ellipsis.
        let kept = snippet.strip_suffix('…').unwrap();
        assert!(!kept.ends_with(' '));
        assert!(long.starts_with(kept));
    }

    #[test]
    fn keeps_text_that_fits_the_limit_exactly() {
        let exact = "x".repeat(SNIPPET_MAX_LENGTH);
        assert_eq!(snippet(&exact), Some(exact));
    }

    #[test]
    fn returns_none_for_empty_blank_or_all_quoted_text() {
        assert_eq!(snippet(""), None);
        assert_eq!(snippet("   \n  "), None);
        assert_eq!(snippet("> only quotes\n> here"), None);
        assert_eq!(build_snippet(None), None);
    }
}
