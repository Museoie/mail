//! Whether this process can serve TLS: the cases of
//! `src/server/lib/tls.test.ts`, and the acceptor the mail listeners build
//! from the same settings.
//!
//! They set `SSL_CERTIFICATE` / `SSL_CERTIFICATE_KEY`, so they take turns.

mod smtp_support;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use mail_server::tls::{acceptor, get_tls_credentials, is_tls_available, TlsCredentials};

static TURN: Mutex<()> = Mutex::new(());

/// A directory holding a stand-in certificate and key, and the environment
/// pointed wherever a test wants. What was set before comes back when it goes.
struct Fixture {
    directory: tempfile::TempDir,
    cert: String,
    key: String,
    original: [Option<String>; 2],
    _turn: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Fixture {
        let turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let directory = tempfile::tempdir().unwrap();
        let (cert, key) = (directory.path().join("cert.pem"), directory.path().join("key.pem"));
        std::fs::write(&cert, "cert").unwrap();
        std::fs::write(&key, "key").unwrap();
        Fixture {
            cert: text(&cert),
            key: text(&key),
            directory,
            original: [std::env::var("SSL_CERTIFICATE").ok(), std::env::var("SSL_CERTIFICATE_KEY").ok()],
            _turn: turn,
        }
    }

    /// A path inside the fixture directory that does not exist.
    fn absent(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    /// Points the environment at the given paths; `None` unsets that variable.
    fn point(&self, cert: Option<&str>, key: Option<&str>) {
        for (name, value) in [("SSL_CERTIFICATE", cert), ("SSL_CERTIFICATE_KEY", key)] {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.point(self.original[0].as_deref(), self.original[1].as_deref());
    }
}

fn text(path: &Path) -> String {
    path.to_str().expect("a UTF-8 path").to_string()
}

#[test]
fn reports_unconfigured_when_neither_variable_is_set() {
    let ssl = Fixture::new();
    ssl.point(None, None);
    assert_eq!(get_tls_credentials(), TlsCredentials::Unconfigured);
    assert!(!is_tls_available());
}

#[test]
fn reports_unconfigured_when_only_the_certificate_is_set() {
    let ssl = Fixture::new();
    ssl.point(Some(&ssl.cert), None);
    assert_eq!(get_tls_credentials(), TlsCredentials::Unconfigured);
    assert!(!is_tls_available());
}

#[test]
fn reports_unconfigured_for_an_empty_string_path() {
    let ssl = Fixture::new();
    ssl.point(Some(&ssl.cert), Some(""));
    assert_eq!(get_tls_credentials(), TlsCredentials::Unconfigured);
    assert!(!is_tls_available());
}

#[test]
fn reports_unreadable_with_both_paths_when_the_certificate_is_absent() {
    let ssl = Fixture::new();
    let absent = text(&ssl.absent("absent-cert.pem"));
    ssl.point(Some(&absent), Some(&ssl.key));
    assert_eq!(get_tls_credentials(), TlsCredentials::Unreadable { cert: absent, key: ssl.key.clone() });
    assert!(!is_tls_available());
}

#[test]
fn reports_unreadable_when_the_key_is_absent() {
    let ssl = Fixture::new();
    let absent = text(&ssl.absent("absent-key.pem"));
    ssl.point(Some(&ssl.cert), Some(&absent));
    assert_eq!(get_tls_credentials(), TlsCredentials::Unreadable { cert: ssl.cert.clone(), key: absent });
    assert!(!is_tls_available());
}

#[test]
fn reports_available_with_both_paths_when_both_files_exist() {
    let ssl = Fixture::new();
    ssl.point(Some(&ssl.cert), Some(&ssl.key));
    assert_eq!(get_tls_credentials(), TlsCredentials::Available { cert: ssl.cert.clone(), key: ssl.key.clone() });
    assert!(is_tls_available());
}

// root bypasses the permission bits entirely, so there is nothing to assert
// when the suite runs as root.
#[test]
fn reports_unreadable_for_a_file_that_exists_but_the_process_cannot_read() {
    use std::os::unix::fs::PermissionsExt;

    let ssl = Fixture::new();
    // The shape a Let's Encrypt `privkey.pem` at 0640 root:root takes for a
    // non-root app user: it exists, and reading it fails.
    let locked = ssl.absent("locked-key.pem");
    std::fs::write(&locked, "key").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&locked).is_ok() {
        eprintln!("skipped: this user reads any file");
        return;
    }
    let locked = text(&locked);
    ssl.point(Some(&ssl.cert), Some(&locked));
    assert_eq!(get_tls_credentials(), TlsCredentials::Unreadable { cert: ssl.cert.clone(), key: locked });
    assert!(!is_tls_available());
}

#[test]
fn re_reads_the_filesystem_on_every_call_so_a_renewed_certificate_is_picked_up() {
    let ssl = Fixture::new();
    let later = ssl.absent("later-cert.pem");
    ssl.point(Some(&text(&later)), Some(&ssl.key));
    assert!(!is_tls_available());
    std::fs::write(&later, "cert").unwrap();
    assert!(is_tls_available());
    std::fs::remove_file(&later).unwrap();
}

#[test]
fn builds_an_acceptor_from_the_certificate_on_disk_at_the_time_it_is_asked() {
    let ssl = Fixture::new();
    // Files that are there but hold no certificate: available, and unusable.
    ssl.point(Some(&ssl.cert), Some(&ssl.key));
    assert!(acceptor().is_err());

    let real = smtp_support::generate_certificate();
    ssl.point(Some(&text(&real.cert)), Some(&text(&real.key)));
    assert!(acceptor().is_ok());

    ssl.point(None, None);
    assert!(acceptor().is_err());
}
