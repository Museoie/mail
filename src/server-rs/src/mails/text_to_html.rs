//! Plain text to HTML (mails/text-to-html.ts).
//!
//! Plain-text bodies are stored in the `html` column as-is, and the viewer
//! renders that column as HTML. Rendered raw, a plain-text mail loses every
//! line break, decodes entity-like runs and treats a literal `<` as markup.
//! These helpers turn such a body into HTML that shows the text as written.

use crate::mime::charset::jsstr;

fn escape_html(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// A character a URL runs through: anything but whitespace, `<`, `>` and `"`.
fn is_url_char(c: char) -> bool {
    !jsstr::is_space(c) && !matches!(c, '<' | '>' | '"')
}

/// The URL starting at `at`, as
/// `/\bhttps?:\/\/[^\s<>"]+[^\s<>".,;:!?)\]'}]/` matches one: it stops
/// before trailing punctuation that usually ends a sentence rather than the
/// URL ("see https://x.com/a." links "https://x.com/a").
fn url_at(line: &str, at: usize) -> Option<&str> {
    if line[..at].chars().next_back().is_some_and(jsstr::is_word) {
        return None;
    }
    let rest = &line[at..];
    let scheme = ["https://", "http://"]
        .into_iter()
        .find(|scheme| rest.starts_with(scheme))?;
    let run = &rest[scheme.len()..];
    let run = &run[..run.find(|c| !is_url_char(c)).unwrap_or(run.len())];
    // The last character that can end a URL, with at least one before it.
    let (last, ending) = run
        .char_indices()
        .skip(1)
        .filter(|(_, c)| !".,;:!?)]'}".contains(*c))
        .last()?;
    Some(&rest[..scheme.len() + last + ending.len_utf8()])
}

/// Escapes plain text and turns bare http(s) URLs into links.
fn linkify(line: &str, out: &mut String) {
    let mut last = 0;
    let mut at = 0;
    while let Some(found) = line[at..].find("http") {
        let start = at + found;
        at = start + 1;
        let Some(url) = url_at(line, start) else {
            continue;
        };
        escape_html(&line[last..start], out);
        out.push_str("<a href=\"");
        escape_html(url, out);
        out.push_str("\">");
        escape_html(url, out);
        out.push_str("</a>");
        last = start + url.len();
        at = last;
    }
    escape_html(&line[last..], out);
}

/// The `>` quote markers opening `line`, each with one optional whitespace
/// character after it: how many there are and where the text starts.
fn quote_prefix(line: &str) -> (usize, usize) {
    let mut level = 0;
    let mut end = 0;
    while let Some(rest) = line[end..].strip_prefix('>') {
        level += 1;
        end += 1 + rest
            .chars()
            .next()
            .filter(|c| jsstr::is_space(*c))
            .map_or(0, char::len_utf8);
    }
    (level, end)
}

/// Converts a plain-text body to HTML: escaped, URLs linked, line breaks and
/// runs of spaces kept (`white-space: pre-wrap`), and `>`-quoted lines
/// wrapped in a `<blockquote>` per quote level so the viewer folds them like
/// an HTML reply.
pub fn plain_text_to_html(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut depth = 0;
    let mut body = String::with_capacity(text.len() + 64);
    for raw in text.split('\n') {
        let (level, start) = quote_prefix(raw);
        // A blockquote is a block of its own, so a line break against its
        // edge would show as an extra blank line under pre-wrap.
        if depth != level && body.ends_with('\n') {
            body.pop();
        }
        while depth < level {
            body.push_str("<blockquote>");
            depth += 1;
        }
        while depth > level {
            body.push_str("</blockquote>");
            depth -= 1;
        }
        linkify(&raw[start..], &mut body);
        body.push('\n');
    }
    body.pop();
    while depth > 0 {
        body.push_str("</blockquote>");
        depth -= 1;
    }
    format!("<div style=\"white-space: pre-wrap\">{body}</div>")
}

/// The HTML to show for a stored mail. A body whose `html` column is empty or
/// holds exactly the plain text was never HTML, so it is converted; any other
/// value is the mail's own HTML and is returned untouched.
pub fn body_html(html: Option<&str>, text: Option<&str>) -> String {
    let html = html.unwrap_or("");
    let text = text.unwrap_or("");
    if !text.is_empty() && (html.is_empty() || html == text) {
        return plain_text_to_html(text);
    }
    html.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_line_breaks_inside_a_pre_wrap_container() {
        assert_eq!(
            plain_text_to_html("one\ntwo\r\nthree"),
            "<div style=\"white-space: pre-wrap\">one\ntwo\nthree</div>"
        );
    }

    #[test]
    fn escapes_markup_and_entity_like_runs_so_they_show_literally() {
        assert!(plain_text_to_html("a < b && c > d &para;").contains("a &lt; b &amp;&amp; c &gt; d &amp;para;"));
    }

    #[test]
    fn links_bare_urls_keeping_the_query_string_intact() {
        assert!(plain_text_to_html("see https://example.com/x?with=query&params=1 now").contains(
            "<a href=\"https://example.com/x?with=query&amp;params=1\">https://example.com/x?with=query&amp;params=1</a> now"
        ));
    }

    #[test]
    fn leaves_sentence_punctuation_out_of_a_link() {
        assert!(plain_text_to_html("go to https://example.com/a.")
            .contains("<a href=\"https://example.com/a\">https://example.com/a</a>."));
    }

    #[test]
    fn does_not_link_non_http_schemes() {
        assert!(!plain_text_to_html("javascript:alert(1)").contains("<a"));
    }

    #[test]
    fn wraps_quoted_lines_in_a_blockquote_per_level() {
        assert_eq!(
            plain_text_to_html("hi\n> one\n>> two\n> three\nbye"),
            "<div style=\"white-space: pre-wrap\">hi<blockquote>one<blockquote>two</blockquote>three</blockquote>bye</div>"
        );
    }

    #[test]
    fn closes_blockquotes_still_open_at_the_end() {
        assert_eq!(
            plain_text_to_html("> a"),
            "<div style=\"white-space: pre-wrap\"><blockquote>a</blockquote></div>"
        );
    }

    #[test]
    fn converts_a_body_whose_html_column_holds_the_plain_text() {
        assert_eq!(body_html(Some("a\nb"), Some("a\nb")), plain_text_to_html("a\nb"));
    }

    #[test]
    fn converts_a_body_with_no_html() {
        assert_eq!(body_html(Some(""), Some("text")), plain_text_to_html("text"));
        assert_eq!(body_html(None, Some("text")), plain_text_to_html("text"));
    }

    #[test]
    fn returns_real_html_untouched() {
        assert_eq!(body_html(Some("<p>a</p>"), Some("a")), "<p>a</p>");
    }

    #[test]
    fn returns_an_empty_string_when_both_are_empty() {
        assert_eq!(body_html(None, None), "");
        assert_eq!(body_html(Some(""), Some("")), "");
    }
}
