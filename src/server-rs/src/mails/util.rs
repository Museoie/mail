//! Attachment storage and shared mail helpers (mails/util.ts).

use std::path::Path;

use serde_json::json;

use super::plain_text::{html_to_plain_text, PlainTextOptions};
use crate::logger;
use crate::mime::charset::jsstr;

pub const TO_ADDRESS_FIELD: &str = "mail.envelopeTo.address";
pub const FROM_ADDRESS_FIELD: &str = "mail.from.value.address";

/// The path of the object a dotted field lives in. A field with no dot loses
/// its last character, as `field.slice(0, field.lastIndexOf("."))` does.
pub fn nested_path(field: &str) -> String {
    match field.rfind('.') {
        Some(dot) => field[..dot].to_string(),
        None => jsstr::truncate(field, jsstr::len(field).saturating_sub(1)),
    }
}

/// Where attachment files are stored, relative to the working directory.
pub const ATTACHMENT_FOLDER: &str = "./attachments";

/// A new attachment id: a random UUID no stored attachment has.
pub fn get_attachment_id() -> String {
    loop {
        let id = uuid::Uuid::new_v4().to_string();
        let file_path = get_attachment_file_path(Some(&id));
        if !Path::new(&file_path).exists() {
            return id;
        }
        logger::warn(format!("Duplicate uuid is found: {file_path}"), json!({}));
        logger::info("Proceeding to regenerate uuid", json!({}));
    }
}

/// The path of the attachment `id`, or of a new one when there is no id.
pub fn get_attachment_file_path(id: Option<&str>) -> String {
    match id.filter(|id| !id.is_empty()) {
        Some(id) => format!("{ATTACHMENT_FOLDER}/{id}"),
        None => format!("{ATTACHMENT_FOLDER}/{}", get_attachment_id()),
    }
}

/// The stored attachment's content, or `None` when it is missing.
pub fn get_attachment(id: &str) -> std::io::Result<Option<Vec<u8>>> {
    let file_path = get_attachment_file_path(Some(id));
    if !Path::new(&file_path).exists() {
        return Ok(None);
    }
    std::fs::read(file_path).map(Some)
}

/// A stored attachment opened for reading, with nothing read yet.
#[derive(Debug)]
pub struct AttachmentFile {
    pub path: String,
    pub size: u64,
    pub file: tokio::fs::File,
}

/// The stored attachment as a file read lazily when sent, or `None` when it
/// is missing or not a regular file.
pub async fn get_attachment_file(id: &str) -> Option<AttachmentFile> {
    let path = get_attachment_file_path(Some(id));
    let file = tokio::fs::File::open(&path).await.ok()?;
    let metadata = file.metadata().await.ok()?;
    metadata.is_file().then(|| AttachmentFile {
        path,
        size: metadata.len(),
        file,
    })
}

/// Where the URL starting at `start` ends, as
/// `/(?:https?|ftp):\/\/[^\s/$.?#].[^\s]*\b/` matches one: a scheme, one
/// character that can start a host, any one character, then the run of
/// non-whitespace given back until it ends at a word boundary.
fn url_end(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    let scheme = ["https://", "http://", "ftp://"]
        .into_iter()
        .find(|scheme| rest.starts_with(scheme))?;
    let mut chars = rest[scheme.len()..].chars();
    let first = chars.next().filter(|c| !jsstr::is_space(*c) && !"/$.?#".contains(*c))?;
    let second = chars.next().filter(|c| !jsstr::is_line_terminator(*c))?;

    let mut previous = second;
    let mut position = scheme.len() + first.len_utf8() + second.len_utf8();
    let mut end = None;
    loop {
        let next = rest[position..].chars().next().filter(|c| !jsstr::is_space(*c));
        if jsstr::is_word(previous) != next.is_some_and(jsstr::is_word) {
            end = Some(start + position);
        }
        let Some(next) = next else {
            return end;
        };
        previous = next;
        position += next.len_utf8();
    }
}

/// The text of an HTML body as it is stored for search and previews: URLs
/// replaced by `[url]`, whitespace runs collapsed, blank lines dropped.
///
/// Where the TypeScript `getText` throws (see [`html_to_plain_text`]), this
/// answers the text rendered past the failure.
pub fn get_text(html: &str) -> String {
    let text = match html_to_plain_text(html, PlainTextOptions::default()) {
        Ok(text) => text,
        Err(failure) => failure.rendered,
    };

    let mut url_eliminated = String::with_capacity(text.len());
    let mut last = 0;
    let mut at = 0;
    while let Some(found) = text[at..].find(['h', 'f']) {
        let start = at + found;
        at = start + 1;
        if let Some(end) = url_end(&text, start) {
            url_eliminated.push_str(&text[last..start]);
            url_eliminated.push_str("[url]");
            last = end;
            at = end;
        }
    }
    url_eliminated.push_str(&text[last..]);

    // `/((?![\n])\s)+/g`: whitespace other than a line break collapses to one space.
    let mut spaces: Vec<char> = Vec::with_capacity(url_eliminated.len());
    for c in url_eliminated.chars() {
        if c != '\n' && jsstr::is_space(c) {
            if spaces.last() != Some(&' ') {
                spaces.push(' ');
            }
        } else {
            spaces.push(c);
        }
    }

    // `/(\n\s|\s\n|\n)+/g`: line breaks, each with one whitespace character
    // after or before it, collapse to one line break. A space left over
    // after such a run stays.
    let is_space = |at: usize| spaces.get(at).copied().is_some_and(jsstr::is_space);
    let is_break = |at: usize| spaces.get(at) == Some(&'\n');
    let mut spaces_compressed = String::with_capacity(spaces.len());
    let mut at = 0;
    while at < spaces.len() {
        let run_start = at;
        loop {
            if (is_break(at) && is_space(at + 1)) || (is_space(at) && is_break(at + 1)) {
                at += 2;
            } else if is_break(at) {
                at += 1;
            } else {
                break;
            }
        }
        if at > run_start {
            spaces_compressed.push('\n');
        } else {
            spaces_compressed.push(spaces[at]);
            at += 1;
        }
    }
    spaces_compressed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_constants() {
        assert_eq!(TO_ADDRESS_FIELD, "mail.envelopeTo.address");
        assert_eq!(FROM_ADDRESS_FIELD, "mail.from.value.address");
        assert_eq!(ATTACHMENT_FOLDER, "./attachments");
    }

    #[test]
    fn nested_path_returns_the_path_without_its_last_segment() {
        assert_eq!(nested_path("mail.envelopeTo.address"), "mail.envelopeTo");
        assert_eq!(nested_path("mail.from.value.address"), "mail.from.value");
        assert_eq!(nested_path("parent.child"), "parent");
    }

    #[test]
    fn nested_path_of_a_single_segment_drops_its_last_character() {
        assert_eq!(nested_path("field"), "fiel");
        assert_eq!(nested_path(""), "");
    }

    #[test]
    fn attachment_file_path_holds_the_given_id() {
        assert_eq!(get_attachment_file_path(Some("abc-123")), "./attachments/abc-123");
    }

    #[test]
    fn attachment_ids_are_fresh_uuids() {
        let id = get_attachment_id();
        assert!(crate::util::is_uuid(&id));
        assert_ne!(id, get_attachment_id());
        assert!(get_attachment_file_path(None).starts_with("./attachments/"));
    }

    #[tokio::test]
    async fn attachment_file_is_the_stored_file_unread_when_it_exists() {
        let id = "util-test-attachment-file";
        std::fs::create_dir_all(ATTACHMENT_FOLDER).unwrap();
        std::fs::write(get_attachment_file_path(Some(id)), "stored bytes").unwrap();

        let mut attachment = get_attachment_file(id).await.unwrap();
        assert_eq!(attachment.size, 12);
        let mut content = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut attachment.file, &mut content)
            .await
            .unwrap();
        assert_eq!(content, "stored bytes");
        assert_eq!(get_attachment(id).unwrap().as_deref(), Some(b"stored bytes".as_slice()));

        std::fs::remove_file(get_attachment_file_path(Some(id))).unwrap();
    }

    #[tokio::test]
    async fn attachment_file_is_none_when_the_file_is_missing() {
        assert!(get_attachment_file("util-test-attachment-never-written")
            .await
            .is_none());
        assert_eq!(get_attachment("util-test-attachment-never-written").unwrap(), None);
    }

    #[test]
    fn get_text_converts_simple_html_to_text() {
        assert_eq!(get_text("<p>Hello, world!</p>"), "Hello, world!");
    }

    #[test]
    fn get_text_handles_multiple_paragraphs() {
        let text = get_text("<p>First paragraph.</p><p>Second paragraph.</p>");
        assert!(text.contains("First paragraph."));
        assert!(text.contains("Second paragraph."));
    }

    #[test]
    fn get_text_skips_images() {
        let text = get_text("<p>Text before<img src=\"image.jpg\" alt=\"test\">Text after</p>");
        assert!(!text.contains("image.jpg"));
        assert!(!text.contains("[test]"));
        assert!(text.contains("Text before"));
        assert!(text.contains("Text after"));
    }

    #[test]
    fn get_text_handles_links_without_displaying_href() {
        let text = get_text("<p>Visit <a href=\"https://example.com\">our site</a></p>");
        assert!(text.contains("our site"));
        assert!(!text.contains("example.com"));
    }

    #[test]
    fn get_text_replaces_urls_with_a_placeholder() {
        let text = get_text("<p>Check out https://example.com/page for more info</p>");
        assert!(text.contains("[url]"));
        assert!(!text.contains("https://example.com"));
    }

    #[test]
    fn get_text_compresses_multiple_spaces() {
        assert_eq!(get_text("<p>Too    many     spaces</p>"), "Too many spaces");
    }

    #[test]
    fn get_text_compresses_multiple_newlines() {
        let text = get_text("<p>Line 1</p><br><br><br><p>Line 2</p>");
        assert!(!text.contains("\n\n\n"));
    }

    #[test]
    fn get_text_handles_a_complex_html_email() {
        let html = "
      <html>
        <body>
          <h1>Newsletter</h1>
          <p>Dear subscriber,</p>
          <p>Check out our <a href=\"https://example.com/products\">new products</a>!</p>
          <img src=\"banner.jpg\" alt=\"Banner\">
          <p>Best regards,<br>The Team</p>
        </body>
      </html>
    ";
        let text = get_text(html);
        assert!(text.contains("NEWSLETTER"));
        assert!(text.contains("Dear subscriber"));
        assert!(text.contains("new products"));
        assert!(text.contains("Best regards"));
        assert!(!text.contains("banner.jpg"));
        assert!(!text.contains("https://"));
    }
}
