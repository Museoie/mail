//! The working directories a DATA transaction (or an IMAP APPEND) parses its
//! attachments into, beside the attachments they become (`smtp.ts`).

use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::logger;

/// Where the per-transaction directories live:
/// `path.join(ATTACHMENT_FOLDER, ".incoming")`, which normalizes
/// `./attachments` away to `attachments/.incoming`.
pub const INCOMING_ATTACHMENT_FOLDER: &str = "attachments/.incoming";

/// A directory of its own for one transaction, not created yet: the parser
/// creates it when the first attachment arrives.
pub fn new_transaction_dir() -> PathBuf {
    Path::new(INCOMING_ATTACHMENT_FOLDER).join(uuid::Uuid::new_v4().to_string())
}

/// Removes a transaction's directory and everything in it. One that is not
/// there is fine; any other failure is logged and otherwise ignored.
pub async fn remove_transaction_dir(dir: &Path) {
    let removed = match tokio::fs::remove_dir_all(dir).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        // `rm -rf` of a plain file removes the file.
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => tokio::fs::remove_file(dir).await,
        other => other,
    };
    if let Err(error) = removed {
        logger::warn_with(
            "Could not remove a transaction's working files",
            json!({ "dir": dir.to_string_lossy() }),
            &error,
        );
    }
}

/// Removes the transaction directories a previous process left behind,
/// crashed mid-DATA or mid-APPEND. Runs before the SMTP and IMAP listeners
/// open, so none can belong to a live transaction of this one.
pub async fn sweep_leftover_transaction_dirs() {
    remove_transaction_dir(Path::new(INCOMING_ATTACHMENT_FOLDER)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transaction_dir_is_a_fresh_uuid_under_the_incoming_folder() {
        let first = new_transaction_dir();
        let second = new_transaction_dir();
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(Path::new("attachments/.incoming")));
        let name = first.file_name().unwrap().to_str().unwrap();
        assert!(crate::util::is_uuid(name), "{name}");
    }

    #[tokio::test]
    async fn removing_takes_the_whole_tree_and_tolerates_absence() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("tx");
        tokio::fs::create_dir_all(dir.join("nested")).await.unwrap();
        tokio::fs::write(dir.join("nested/file"), b"x").await.unwrap();
        remove_transaction_dir(&dir).await;
        assert!(!dir.exists());
        remove_transaction_dir(&dir).await;
        assert!(!dir.exists());
    }
}
