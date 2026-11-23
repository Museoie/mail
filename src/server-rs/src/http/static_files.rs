//! The built web client: static files with validators, and the single-page
//! fallback.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::mime_types;
use super::respond::{Body, Response};
use crate::js;

/// A hidden name such as `.env` or `.git`; `..` is the parent check's concern.
fn has_dot_segment(decoded_path: &str) -> bool {
    decoded_path
        .split('/')
        .any(|segment| segment.len() > 1 && segment.starts_with('.') && segment != "..")
}

/// `..` as a whole segment, with either separator.
fn has_parent_segment(decoded_path: &str) -> bool {
    decoded_path.split(['/', '\\']).any(|segment| segment == "..")
}

/// The content type the Bun server gives a file of this name: by the text
/// after the last dot, in any letter case. A name that only starts with a
/// dot has no extension.
pub fn content_type_of(path: &Path) -> &'static str {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    match name.rfind('.') {
        Some(dot) if dot > 0 => mime_types::content_type_for_extension(&name[dot + 1..]),
        _ => mime_types::DEFAULT_TYPE,
    }
}

/// A regular file's size and modification time in milliseconds.
pub async fn stat_file(path: &Path) -> Option<(u64, i64)> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified: DateTime<Utc> = metadata.modified().ok()?.into();
    Some((metadata.len(), modified.timestamp_millis()))
}

/// A file served with validators, so a revalidating browser gets a 304. A
/// `Range` request is answered when the response is written, which adds its
/// own `Accept-Ranges`.
fn file_response(path: PathBuf, size: u64, modified_ms: i64, range_requested: bool) -> Response {
    let modified = DateTime::<Utc>::from_timestamp_millis(modified_ms).unwrap_or_default();
    let mut response = Response::new(200, Body::File { path: path.clone(), size });
    response.append_header("Content-Type", content_type_of(&path));
    response.append_header("Cache-Control", "public, max-age=0");
    response.append_header("Last-Modified", js::utc_string(&modified));
    response.append_header("ETag", format!("W/\"{size:x}-{modified_ms:x}\""));
    if !range_requested {
        response.append_header("Accept-Ranges", "bytes");
    }
    response
}

/// The file under `root` that a GET or HEAD for `url_path` names, or `None`
/// so the caller can fall through. Dotfiles, parent-directory segments and
/// directories are never served.
pub async fn serve_static(root: &Path, url_path: &str, range_requested: bool) -> Option<Response> {
    let decoded = js::decode_uri_component(url_path)?;
    if decoded.contains('\0') || has_parent_segment(&decoded) || has_dot_segment(&decoded) {
        return None;
    }
    let relative = if decoded.ends_with('/') { format!("{decoded}index.html") } else { decoded };
    let path = root.join(relative.trim_start_matches('/'));
    let (size, modified_ms) = stat_file(&path).await?;
    Some(file_response(path, size, modified_ms, range_requested))
}

/// The single-page app's entry document, or `None` when the client is not built.
pub async fn serve_index(root: &Path, range_requested: bool) -> Option<Response> {
    let path = root.join("index.html");
    let (size, modified_ms) = stat_file(&path).await?;
    Some(file_response(path, size, modified_ms, range_requested))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_and_parent_segments() {
        assert!(has_dot_segment("/.env"));
        assert!(has_dot_segment("/a/.git/config"));
        assert!(!has_dot_segment("/a/./b"));
        assert!(!has_dot_segment("/a/../b"));
        assert!(has_parent_segment("/a/../b"));
        assert!(has_parent_segment("\\..\\x"));
        assert!(!has_parent_segment("/a..b/c"));
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type_of(Path::new("index.html")), "text/html;charset=utf-8");
        assert_eq!(content_type_of(Path::new("a/b.JS")), "text/javascript;charset=utf-8");
        assert_eq!(content_type_of(Path::new("noext")), "application/octet-stream");
    }
}
