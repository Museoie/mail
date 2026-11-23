//! Shared pieces of the IMAP unit tests: the lock around process-wide state,
//! a throwaway TLS certificate, the journal the log helpers feed, and a fake
//! credential backend.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};
use serde_json::Value;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::auth::AuthBackend;
use crate::models::User;
use crate::postgres::repositories::domains::{DomainAccess, DomainRole};
use crate::sso::app_password::AppPasswordCheck;
use crate::sso::Sso;
use crate::tls::TlsAcceptor;

static PROCESS_WIDE: RwLock<()> = RwLock::const_new(());

/// Taken by every test that opens a session. Sessions register themselves
/// in the process-wide registries and IDLE manager, so they run alongside
/// each other but never alongside a sweep.
pub(crate) async fn shared() -> RwLockReadGuard<'static, ()> {
    PROCESS_WIDE.read().await
}

/// Taken by a test that sweeps every session (the shutdown sweep, the IDLE
/// manager's shutdown) or counts the registries.
pub(crate) async fn exclusive() -> RwLockWriteGuard<'static, ()> {
    PROCESS_WIDE.write().await
}

/// A self-signed certificate for `localhost`, made once per build directory
/// with the `openssl` command. `None` when the command is not there.
pub(crate) fn certificate() -> Option<&'static (PathBuf, PathBuf)> {
    static CERTIFICATE: OnceLock<Option<(PathBuf, PathBuf)>> = OnceLock::new();
    CERTIFICATE
        .get_or_init(|| {
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tls");
            let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
            if cert.exists() && key.exists() {
                return Some((cert, key));
            }
            // Made beside the final place and moved in, so a test process
            // running at the same time never reads half a file.
            let staging = dir.join(format!("staging-{}", std::process::id()));
            std::fs::create_dir_all(&staging).ok()?;
            let made = Command::new("openssl")
                .args([
                    "req",
                    "-x509",
                    "-newkey",
                    "ec",
                    "-pkeyopt",
                    "ec_paramgen_curve:prime256v1",
                    "-nodes",
                    "-keyout",
                    "key.pem",
                    "-out",
                    "cert.pem",
                    "-days",
                    "2",
                    "-subj",
                    "/CN=localhost",
                ])
                .current_dir(&staging)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .ok()?
                .success();
            if !made {
                return None;
            }
            std::fs::rename(staging.join("key.pem"), &key).ok()?;
            std::fs::rename(staging.join("cert.pem"), &cert).ok()?;
            let _ = std::fs::remove_dir_all(&staging);
            Some((cert, key))
        })
        .as_ref()
}

/// A TLS acceptor for the test certificate, built without the process
/// environment the server reads its own certificate from.
pub(crate) fn acceptor() -> Option<TlsAcceptor> {
    let (cert, key) = certificate()?;
    crate::tls::acceptor_for(cert.to_str()?, key.to_str()?).ok()
}

/// The server name the test certificate is for.
pub(crate) fn server_name() -> rustls::pki_types::ServerName<'static> {
    rustls::pki_types::ServerName::try_from("localhost").expect("a valid name")
}

static JOURNAL: Mutex<Vec<(String, String, Value)>> = Mutex::new(Vec::new());

/// Called by the IMAP log helpers.
pub(crate) fn journal(level: &str, message: &str, context: &Value) {
    JOURNAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((
        level.to_string(),
        message.to_string(),
        context.clone(),
    ));
}

/// Every journal entry so far, as `(level, message, context)`. Tests run side
/// by side, so each one looks for markers only it uses.
pub(crate) fn journal_entries() -> Vec<(String, String, Value)> {
    JOURNAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// A call into [`FakeAuthBackend`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FakeAuthCall {
    IsLimited(String, Option<String>),
    Record(String, Option<String>),
    Reset(String, Option<String>),
    GetUser(String),
    GetUserById(String),
    /// The role read for a scoped login: the person's id and the domain.
    DomainAccess(String, String),
    /// The password compared, and the digest it was compared against.
    Verify(String, Option<String>),
    AppPassword(String, String, String),
}

/// A credential backend that answers from its own accounts and records
/// every call made into it. The password comparison is the real one.
#[derive(Default)]
pub(crate) struct FakeAuthBackend {
    pub(crate) users: HashMap<String, User>,
    pub(crate) lookup_fails: bool,
    pub(crate) rate_limited: bool,
    /// What recording a failure answers: true once past the threshold.
    pub(crate) record_limits: bool,
    pub(crate) sso: Option<Arc<Sso>>,
    /// What the provider answers; `Rejected` when unset.
    pub(crate) app_password: Option<AppPasswordCheck>,
    /// The roles people hold on domains, by the person's user id.
    pub(crate) domains: Vec<(String, DomainAccess)>,
    pub(crate) calls: Mutex<Vec<FakeAuthCall>>,
}

impl FakeAuthBackend {
    pub(crate) fn with_users(users: Vec<User>) -> FakeAuthBackend {
        FakeAuthBackend {
            users: users.into_iter().map(|user| (user.username.clone().unwrap(), user)).collect(),
            ..FakeAuthBackend::default()
        }
    }

    pub(crate) fn calls(&self) -> Vec<FakeAuthCall> {
        self.calls.lock().unwrap().clone()
    }

    pub(crate) fn called(&self, matches: impl Fn(&FakeAuthCall) -> bool) -> bool {
        self.calls().iter().any(matches)
    }

    pub(crate) fn compared(&self) -> Vec<(String, Option<String>)> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                FakeAuthCall::Verify(password, digest) => Some((password, digest)),
                _ => None,
            })
            .collect()
    }

    fn record(&self, call: FakeAuthCall) {
        self.calls.lock().unwrap().push(call);
    }
}

impl AuthBackend for FakeAuthBackend {
    fn sso(&self) -> Option<Arc<Sso>> {
        self.sso.clone()
    }

    fn is_auth_rate_limited(&self, ip: &str, account: Option<&str>) -> bool {
        self.record(FakeAuthCall::IsLimited(ip.to_string(), account.map(str::to_string)));
        self.rate_limited
    }

    async fn record_auth_failure(&self, ip: &str, account: Option<&str>) -> bool {
        self.record(FakeAuthCall::Record(ip.to_string(), account.map(str::to_string)));
        self.record_limits
    }

    fn reset_auth_failures(&self, ip: &str, account: Option<&str>) {
        self.record(FakeAuthCall::Reset(ip.to_string(), account.map(str::to_string)));
    }

    async fn get_user(&self, username: &str) -> Result<Option<User>> {
        self.record(FakeAuthCall::GetUser(username.to_string()));
        if self.lookup_fails {
            return Err(anyhow!("connection terminated"));
        }
        Ok(self.users.get(username).cloned())
    }

    async fn verify_password(&self, password: &str, stored_hash: Option<&str>) -> bool {
        self.record(FakeAuthCall::Verify(password.to_string(), stored_hash.map(str::to_string)));
        crate::verify_password::verify_password(Some(password), stored_hash).await
    }

    async fn get_user_by_id(&self, id: &str) -> Result<Option<User>> {
        self.record(FakeAuthCall::GetUserById(id.to_string()));
        Ok(self.users.values().find(|user| user.id.as_deref() == Some(id)).cloned())
    }

    async fn owned_domain_names(&self, user_id: &str) -> Result<Vec<String>> {
        Ok(self
            .domains
            .iter()
            .filter(|(holder, access)| holder == user_id && access.role == DomainRole::Owner)
            .map(|(_, access)| access.name.clone())
            .collect())
    }

    async fn get_domain_access(&self, user_id: &str, domain: &str) -> Result<Option<DomainAccess>> {
        self.record(FakeAuthCall::DomainAccess(user_id.to_string(), domain.to_string()));
        Ok(self.domains.iter().find(|(holder, access)| holder == user_id && access.name == domain).map(|(_, access)| access.clone()))
    }

    async fn check_app_password(
        &self,
        _sso: &Sso,
        username: &str,
        password: &str,
        ip: &str,
    ) -> Result<AppPasswordCheck> {
        self.record(FakeAuthCall::AppPassword(username.to_string(), password.to_string(), ip.to_string()));
        Ok(self.app_password.clone().unwrap_or(AppPasswordCheck::Rejected))
    }
}

/// A session on an in-memory socket, with what it wrote collected.
pub(crate) struct WireSession {
    pub(crate) session: Arc<super::session::ImapSession>,
    received: Arc<Mutex<Vec<u8>>>,
    _read: tokio::io::ReadHalf<super::socket::BoxedStream>,
}

/// A fresh session, not signed in, on a plain (non-TLS) socket.
pub(crate) fn wire_session() -> WireSession {
    use tokio::io::AsyncReadExt;

    let (socket, read, mut client) = super::socket::SocketHandle::for_test("203.0.113.7");
    let session = super::session::ImapSession::new(socket, false);
    let received: Arc<Mutex<Vec<u8>>> = Arc::default();
    tokio::spawn({
        let received = received.clone();
        async move {
            let mut chunk = vec![0u8; 64 * 1024];
            while let Ok(read) = client.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
                received.lock().unwrap().extend_from_slice(&chunk[..read]);
            }
        }
    });
    WireSession { session, received, _read: read }
}

impl WireSession {
    /// Everything written so far, once the writer has caught up.
    pub(crate) async fn wire_bytes(&self) -> Vec<u8> {
        let mut seen = usize::MAX;
        // Settled when two looks 20 ms apart agree.
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let now = self.received.lock().unwrap().len();
            if now == seen {
                return self.received.lock().unwrap().clone();
            }
            seen = now;
        }
    }

    /// Everything written so far, as text.
    pub(crate) async fn wire(&self) -> String {
        String::from_utf8_lossy(&self.wire_bytes().await).into_owned()
    }
}
