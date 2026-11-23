//! An APPEND message literal on disk (imap/literal-spool.ts).

use std::path::{Component, Path, PathBuf};

use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;

use super::types::{Literal, MessageLiteral};
use crate::smtp::transaction_dir;

const MESSAGE_FILE: &str = "message";

/// Per-transaction attachment working directories, which the SMTP server
/// owns (`smtp.ts` in the Bun server). Unit tests run from the crate
/// directory; their spool files go under the build output rather than into
/// the source tree.
pub fn incoming_attachment_folder() -> PathBuf {
    if cfg!(test) {
        return Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-attachments").join(".incoming");
    }
    PathBuf::from(transaction_dir::INCOMING_ATTACHMENT_FOLDER)
}

/// A fresh directory name for one transaction's working files.
pub fn new_transaction_dir() -> PathBuf {
    if cfg!(test) {
        return incoming_attachment_folder().join(uuid::Uuid::new_v4().to_string());
    }
    transaction_dir::new_transaction_dir()
}

/// Removes a transaction's working files. A failure is logged, not returned.
pub use crate::smtp::transaction_dir::remove_transaction_dir;

/// `path.resolve`: absolute, with `.` and `..` folded away, symlinks left alone.
fn resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// Removes a spooled message literal and the directory it was written into.
/// A literal whose directory is not a transaction directory is left alone.
pub async fn remove_message_literal(literal: &MessageLiteral) {
    let file = resolve(Path::new(&literal.path));
    let Some(dir) = file.parent() else {
        return;
    };
    if dir.parent() != Some(resolve(&incoming_attachment_folder()).as_path()) {
        return;
    }
    remove_transaction_dir(dir).await;
}

/// Removes every spooled message literal among `literals`.
pub async fn remove_message_literals(literals: &[Literal]) {
    for literal in literals {
        if let Literal::Message(message) = literal {
            remove_message_literal(message).await;
        }
    }
}

/// An APPEND message literal written to its own transaction directory as its
/// octets arrive, so a session holds one segment of a message rather than
/// all of it.
pub struct LiteralSpool {
    dir: PathBuf,
    path: PathBuf,
    file: Option<File>,
    written: u64,
}

impl LiteralSpool {
    /// A spool that has touched nothing yet: the directory and the file are
    /// made by the first write, or by `finish` for an empty message.
    pub fn new() -> LiteralSpool {
        let dir = new_transaction_dir();
        let path = dir.join(MESSAGE_FILE);
        LiteralSpool { dir, path, file: None, written: 0 }
    }

    async fn open(&mut self) -> std::io::Result<&mut File> {
        if self.file.is_none() {
            tokio::fs::create_dir_all(&self.dir).await?;
            let file = OpenOptions::new().write(true).create_new(true).open(&self.path).await?;
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("the file was just opened"))
    }

    /// Appends `chunk`.
    pub async fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.open().await?.write_all(chunk).await?;
        self.written += chunk.len() as u64;
        Ok(())
    }

    /// Closes the file and answers the literal it holds.
    pub async fn finish(mut self) -> std::io::Result<MessageLiteral> {
        let flushed = match self.open().await {
            Ok(file) => file.flush().await,
            Err(error) => Err(error),
        };
        self.file = None;
        if let Err(error) = flushed {
            remove_transaction_dir(&self.dir).await;
            return Err(error);
        }
        Ok(MessageLiteral { path: self.path.to_string_lossy().into_owned(), size: self.written })
    }

    /// Drops the file, whatever state the spool is in.
    pub async fn discard(mut self) {
        self.file = None;
        remove_transaction_dir(&self.dir).await;
    }
}

impl Default for LiteralSpool {
    fn default() -> LiteralSpool {
        LiteralSpool::new()
    }
}

/// Tests that look at which spool directories exist take turns.
#[cfg(test)]
pub(crate) async fn spool_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// The spool directories that exist now.
#[cfg(test)]
pub(crate) fn spool_dirs() -> Vec<String> {
    std::fs::read_dir(incoming_attachment_folder())
        .map(|entries| entries.filter_map(|entry| entry.ok()).map(|entry| entry.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_spooled_message_is_the_octets_written_in_order() {
        let _spool = spool_test_lock().await;
        let mut spool = LiteralSpool::new();
        spool.write(b"Subject: x\r\n").await.unwrap();
        spool.write(&[0xff, 0xfe, 0x80]).await.unwrap();
        spool.write(b"").await.unwrap();
        let literal = spool.finish().await.unwrap();
        assert_eq!(literal.size, 15);
        let stored = tokio::fs::read(&literal.path).await.unwrap();
        assert_eq!(stored, [b"Subject: x\r\n".as_slice(), &[0xff, 0xfe, 0x80]].concat());

        remove_message_literal(&literal).await;
        assert!(!Path::new(&literal.path).exists());
        assert!(!Path::new(&literal.path).parent().unwrap().exists());
    }

    #[tokio::test]
    async fn an_empty_message_is_an_empty_file() {
        let _spool = spool_test_lock().await;
        let literal = LiteralSpool::new().finish().await.unwrap();
        assert_eq!(literal.size, 0);
        assert_eq!(tokio::fs::read(&literal.path).await.unwrap(), b"");
        remove_message_literals(&[Literal::Text("INBOX".into()), Literal::Message(literal.clone())]).await;
        assert!(!Path::new(&literal.path).exists());
    }

    #[tokio::test]
    async fn discard_removes_a_partial_file_and_is_safe_before_any_write() {
        let _spool = spool_test_lock().await;
        let mut spool = LiteralSpool::new();
        spool.write(b"partial").await.unwrap();
        let dir = spool.dir.clone();
        assert!(dir.exists());
        spool.discard().await;
        assert!(!dir.exists());

        LiteralSpool::new().discard().await;
    }

    #[tokio::test]
    async fn leaves_a_directory_that_is_not_a_transaction_directory_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("message");
        std::fs::write(&file, "hello").unwrap();
        remove_message_literal(&MessageLiteral { path: file.to_string_lossy().into_owned(), size: 5 }).await;
        assert!(file.exists());
    }

    #[test]
    fn resolve_folds_dots_like_path_resolve() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(resolve(Path::new("./attachments/.incoming")), cwd.join("attachments/.incoming"));
        assert_eq!(resolve(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(resolve(Path::new(transaction_dir::INCOMING_ATTACHMENT_FOLDER)), cwd.join("attachments/.incoming"));
    }
}
