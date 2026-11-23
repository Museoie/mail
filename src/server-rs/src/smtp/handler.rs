//! What happens to the mail an SMTP listener takes in: AUTH, MAIL FROM,
//! RCPT TO and DATA for inbound delivery and authenticated submission (the
//! mail-handling half of `smtp.ts`).
//!
//! ```ignore
//! let servers = smtp::handler::initialize_mail_smtp().await?;
//! // ...
//! smtp::close_smtp_servers(&servers).await;
//! ```

use std::future::Future;
use std::sync::Arc;

use anyhow::Result;
use serde_json::json;

use super::bounded::BoundedMessage;
use super::data_budget::with_smtp_data_budget;
use super::listeners::{initialize_smtp, SmtpListeners};
use super::server::SmtpServer;
use super::transaction_dir::sweep_leftover_transaction_dirs;
use super::types::{
    SharedSmtpHandler, SmtpAddress, SmtpAuthRequest, SmtpAuthResponse, SmtpDataStream, SmtpEnvelope, SmtpHandler,
    SmtpHookError, SmtpSession,
};
use crate::auth_rate_limit::{
    is_auth_rate_limited, is_recipient_probe_rate_limited, normalize_auth_account, record_auth_failure,
    record_recipient_probe, reset_auth_failures,
};
use crate::models::User;
use crate::read_only::ADMIN_RO_USERNAME;
use crate::sso::app_password::{check_app_password, AppPasswordCheck, AppPasswordScope};
use crate::sso::{get_sso, Sso};
use crate::users::{get_user, UserQuery};
use crate::util::is_local_address;
use crate::verify_password::verify_password;
use crate::{auth_rate_limit, env, logger};

/// The credential generation the connection authenticated on: what `onAuth`
/// writes onto the Bun session as `passwordVersion`. Unset reads as 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialStamp {
    pub password_version: i64,
}

/// The refusals this module answers with, each with its RFC 5321 code.
pub mod refusals {
    use super::SmtpHookError;

    /// Neither the sender nor any recipient is local: RFC 5321 §3.6.1 gives
    /// 550 to a message this host will not relay.
    pub fn relay_denied() -> SmtpHookError {
        SmtpHookError::with_code(550, "Error: relay access denied")
    }

    /// A recipient this host is responsible for but holds no mailbox for
    /// (RFC 5321 §3.5.3), answered at RCPT TO so the octets never travel.
    pub fn no_such_mailbox() -> SmtpHookError {
        SmtpHookError::with_code(550, "5.1.1 Error: no such mailbox here")
    }

    /// The mailbox RCPT TO accepted held nothing by the end of DATA, so it
    /// went away mid-transaction. Transient: a retry reaches the 550.
    pub fn mail_not_stored() -> SmtpHookError {
        SmtpHookError::with_code(451, "4.2.1 Error: mailbox unavailable, try again later")
    }

    /// A lookup needed to answer did not return (RFC 5321 §4.2.3).
    pub fn lookup_failed() -> SmtpHookError {
        SmtpHookError::with_code(451, "4.3.0 Error: temporary lookup failure")
    }

    /// The identity provider could not say whether an app password is valid
    /// (RFC 4954 §6).
    pub fn auth_unavailable() -> SmtpHookError {
        SmtpHookError::with_code(454, "4.7.0 Temporary authentication failure")
    }

    /// The connection authenticated before its credential was rotated.
    pub fn credential_changed() -> SmtpHookError {
        SmtpHookError::with_code(421, "4.4.2 Credential changed, please re-authenticate")
    }

    /// The peer has spent its recipient-probe budget: transient, and the same
    /// for every local recipient, so a throttled peer learns nothing.
    pub fn too_many_probes() -> SmtpHookError {
        SmtpHookError::with_code(451, "4.7.0 Error: too many unknown recipients, try again later")
    }

    /// The read-only credential tried to send.
    pub fn read_only() -> SmtpHookError {
        SmtpHookError::with_code(550, "READ-ONLY user cannot send")
    }

    /// A failed AUTH past the rate limit: the command's own 535.
    pub fn too_many_auth_failures() -> SmtpHookError {
        SmtpHookError::new("Too many failed authentication attempts")
    }

    /// DATA with no `EMAIL_DOMAIN` set: the command's own 450.
    pub fn not_configured() -> SmtpHookError {
        SmtpHookError::new("Email service not configured")
    }
}

/// What the hooks need from outside: accounts, single sign-on and the served
/// domain. [`ServerDirectory`] is the running server's; tests stand in their own.
pub trait MailDirectory: Send + Sync + 'static {
    /// The registered account named `username` (`getUser({ username })`).
    fn get_user(&self, username: &str) -> impl Future<Output = Result<Option<User>>> + Send;

    /// The running single sign-on, `None` while it is off.
    fn sso(&self) -> Option<Arc<Sso>>;

    /// `password` as an app password of `username`, checked with the provider.
    fn check_app_password(
        &self,
        sso: &Sso,
        username: &str,
        password: &str,
        remote_address: &str,
    ) -> impl Future<Output = Result<AppPasswordCheck>> + Send;

    /// `EMAIL_DOMAIN`, `None` when unset or empty.
    fn email_domain(&self) -> Option<String>;
}

/// The database, the running single sign-on and the environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServerDirectory;

impl MailDirectory for ServerDirectory {
    async fn get_user(&self, username: &str) -> Result<Option<User>> {
        get_user(&UserQuery::username(username)).await
    }

    fn sso(&self) -> Option<Arc<Sso>> {
        get_sso()
    }

    async fn check_app_password(
        &self,
        sso: &Sso,
        username: &str,
        password: &str,
        remote_address: &str,
    ) -> Result<AppPasswordCheck> {
        check_app_password(sso, username, password, AppPasswordScope::Smtp, remote_address).await
    }

    fn email_domain(&self) -> Option<String> {
        env::non_empty("EMAIL_DOMAIN")
    }
}

/// The mail handling behind every SMTP listener.
#[derive(Debug, Clone, Default)]
pub struct MailHandler<D: MailDirectory = ServerDirectory> {
    pub(crate) directory: D,
}

impl MailHandler<ServerDirectory> {
    /// The handler of the running server. Starts the hourly cleanup of the
    /// authentication rate limits, which the Bun module does on import.
    pub fn new() -> MailHandler<ServerDirectory> {
        auth_rate_limit::start_cleanup_scheduler();
        MailHandler { directory: ServerDirectory }
    }
}

impl<D: MailDirectory> MailHandler<D> {
    pub fn with_directory(directory: D) -> MailHandler<D> {
        MailHandler { directory }
    }
}

/// Removes what a previous process left in `attachments/.incoming`, then
/// starts the SMTP listeners with this module's mail handling
/// (`initializeSmtp`).
pub async fn initialize_mail_smtp() -> Result<Vec<SmtpServer>> {
    sweep_leftover_transaction_dirs().await;
    initialize_smtp(SmtpListeners {
        handler: SharedSmtpHandler::new(MailHandler::new()),
        single_sign_on: get_sso().is_some(),
    })
    .await
}

/// The client address the rate limits are keyed by (`remoteAddress ?? "unknown"`).
fn client_ip(session: &SmtpSession) -> &str {
    if session.remote_address.is_empty() {
        "unknown"
    } else {
        &session.remote_address
    }
}

fn stamped_version(session: &SmtpSession) -> i64 {
    session.state.get::<CredentialStamp>().map_or(1, |stamp| stamp.password_version)
}

/// What RCPT TO has to find out about a recipient before answering it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipientCheck {
    /// Not this host's to judge (no served domain, or a foreign address):
    /// accepted, and the relay decision waits for DATA.
    Pass,
    /// Inside the served zone: the account this username names must exist.
    /// Empty when the address names no username at all.
    Lookup(String),
}

/// `addressToUsername` against the served domain `domain`: `x@<domain>` and
/// `x@admin.<domain>` are admin's, `x@bob.<domain>` is bob's, and anything
/// else under the domain is the labels in front of it.
pub fn username_for_address(address: &str, domain: &str) -> String {
    let domain = domain.to_lowercase();
    let domain_in_address = address.rsplit('@').next().unwrap_or("").to_lowercase();
    let needle = format!(".{domain}");
    let sub_domain = match domain_in_address.find(&needle) {
        Some(at) => &domain_in_address[..at],
        None => domain_in_address.as_str(),
    };
    if sub_domain == domain {
        "admin".to_string()
    } else {
        sub_domain.to_string()
    }
}

/// Whether RCPT TO looks `address` up, and under which username.
pub fn recipient_check(address: &str, email_domain: Option<&str>) -> RecipientCheck {
    match email_domain {
        Some(domain) if is_local_address(address, domain) => {
            RecipientCheck::Lookup(username_for_address(address, domain))
        }
        _ => RecipientCheck::Pass,
    }
}

/// Which way a DATA transaction goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataLane {
    /// Stored for the local recipients.
    Incoming,
    /// An authenticated user's submission, sent through Mailgun.
    Outgoing,
}

/// The lane for a transaction, `None` for a relay this host refuses.
///
/// `MAIL FROM` is a value any sender chooses freely, so a local one selects
/// submission only on a session that authenticated. Without that, inbound
/// mail whose envelope sender sits under the served zone — a forwarder, a
/// bounce, a spoof — would be routed to submission and refused.
pub fn choose_lane(envelope: &SmtpEnvelope, user: Option<&str>, email_domain: &str) -> Option<DataLane> {
    let incoming = envelope.rcpt_to.iter().any(|address| is_local_address(&address.address, email_domain));
    let outgoing = user.is_some_and(|user| !user.is_empty())
        && envelope.mail_from.as_ref().is_some_and(|from| is_local_address(&from.address, email_domain));
    if outgoing {
        Some(DataLane::Outgoing)
    } else if incoming {
        Some(DataLane::Incoming)
    } else {
        None
    }
}

/// How a DATA lane failed.
pub(crate) enum DataFailure {
    /// A refusal with its own reply code, logged where it was decided at the
    /// severity its code implies.
    Refused(SmtpHookError),
    /// Anything else: answered 450 with its message, and logged as a failed
    /// transaction.
    Failed(anyhow::Error),
}

impl From<SmtpHookError> for DataFailure {
    fn from(error: SmtpHookError) -> DataFailure {
        DataFailure::Refused(error)
    }
}

/// The message a JavaScript `Error` built from `error` would carry: the
/// outermost description.
pub(crate) fn error_message(error: &anyhow::Error) -> String {
    error.to_string()
}

impl<D: MailDirectory> MailHandler<D> {
    /// Credential-revocation fence: the generation stamped at AUTH is
    /// compared against the credential row's current version, so a
    /// connection a sweep could not reach loses its grant at its next
    /// command, as does one whose account was deleted. Unauthenticated
    /// sessions carry no grant and are not checked;
    /// a lookup fault is answered transient, since a grant that cannot be
    /// proven current is not exercised.
    async fn check_credential_version(&self, session: &SmtpSession) -> Result<(), SmtpHookError> {
        let Some(username) = session.user.as_deref() else {
            return Ok(());
        };
        let stamped = stamped_version(session);
        let user = match self.directory.get_user(username).await {
            Ok(user) => user,
            Err(error) => {
                logger::error_with(
                    "SMTP: credential version lookup failed",
                    json!({ "remoteAddress": session.remote_address }),
                    &error,
                );
                return Err(refusals::lookup_failed());
            }
        };
        // A credential whose row is gone (deleted, or soft-deleted, which the
        // read leaves out) is revoked, as a rotated one is.
        let current = user.map(|user| user.password_version.unwrap_or(1));
        if current.is_none_or(|current| current > stamped) {
            let message = match current {
                Some(_) => "SMTP: refused a command on a rotated credential",
                None => "SMTP: refused a command on a removed account",
            };
            logger::info(message, json!({ "authenticatedAs": username }));
            return Err(refusals::credential_changed());
        }
        Ok(())
    }

    async fn authenticate(
        &self,
        auth: SmtpAuthRequest,
        session: &mut SmtpSession,
    ) -> Result<SmtpAuthResponse, SmtpHookError> {
        if let Some(user) = session.user.clone() {
            return Ok(SmtpAuthResponse::accept(user));
        }
        let ip = client_ip(session).to_string();
        let SmtpAuthRequest { username, password, .. } = auth;
        // The attempted identity: failures accumulate per (ip, account) and a
        // success forgives only this account's own bucket, never the per-IP
        // aggregate, so alternating accounts from one IP cannot wipe it.
        let account = normalize_auth_account(Some(&username));
        let account = account.as_deref();

        if is_auth_rate_limited(&ip, account) {
            return Err(refusals::too_many_auth_failures());
        }

        let sso = self.directory.sso();
        // While single sign-on and local passwords are both on, the local
        // password goes first: it is never sent to the provider.
        if sso.as_ref().is_none_or(|sso| sso.config.local_login) {
            let user = match self.directory.get_user(&username).await {
                Ok(user) => user,
                Err(error) => {
                    logger::error_with("SMTP: authentication lookup failed", json!({ "remoteAddress": ip }), &error);
                    return Err(refusals::lookup_failed());
                }
            };
            let matches =
                verify_password(Some(&password), user.as_ref().and_then(|user| user.password.as_deref())).await;
            let signed = user.as_ref().and_then(User::get_signed);
            if let (Some(user), Some(_), true) = (&user, &signed, matches) {
                reset_auth_failures(&ip, account);
                session.state.insert(CredentialStamp { password_version: user.password_version.unwrap_or(1) });
                // Read-only sessions pass this gate and are refused at MAIL
                // FROM, the one mutating gate this surface has.
                if username == ADMIN_RO_USERNAME {
                    logger::info("SMTP AUTH success (read-only)", json!({ "authenticatedAs": username, "ip": ip }));
                }
                return Ok(SmtpAuthResponse::accept(username));
            }
            if sso.is_none() {
                record_auth_failure(&ip, account).await;
                return Ok(SmtpAuthResponse::reject());
            }
        }

        let sso = sso.expect("single sign-on is on past the local check");
        let check = match self.directory.check_app_password(&sso, &username, &password, &ip).await {
            Ok(check) => check,
            Err(error) => {
                logger::error_with("SMTP: authentication lookup failed", json!({ "remoteAddress": ip }), &error);
                return Err(refusals::lookup_failed());
            }
        };
        match check {
            AppPasswordCheck::Unavailable => Err(refusals::auth_unavailable()),
            AppPasswordCheck::Rejected => {
                record_auth_failure(&ip, account).await;
                Ok(SmtpAuthResponse::reject())
            }
            AppPasswordCheck::Accepted { password_version, .. } => {
                reset_auth_failures(&ip, account);
                session.state.insert(CredentialStamp { password_version });
                Ok(SmtpAuthResponse::accept(username))
            }
        }
    }

    async fn mail_from(&self, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        // SMTP submission writes a Sent row and hands the message to Mailgun:
        // a mutation, which the read-only credential is refused. 550 keeps it
        // in the policy-denied family rather than one that invites a retry.
        if session.user.as_deref() == Some(ADMIN_RO_USERNAME) {
            return Err(refusals::read_only());
        }
        self.check_credential_version(session).await
    }

    /// Refuses a recipient inside the served zone that maps onto no account.
    ///
    /// The refusal is also a yes/no on username existence, and RCPT TO is
    /// taken unauthenticated, so each one is charged to a per-IP budget
    /// priced as a failed credential is. Past the budget the answer is a
    /// uniform transient refusal and no lookup runs.
    async fn rcpt_to(&self, address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        let ip = client_ip(session).to_string();
        match self.check_recipient(address, session, &ip).await {
            Ok(answer) => answer,
            // Every failure inside, the rotated credential included, becomes
            // the transient lookup failure.
            Err(error) => {
                logger::error_with("SMTP: recipient lookup failed", json!({ "remoteAddress": ip }), &error);
                Err(refusals::lookup_failed())
            }
        }
    }

    /// The recipient check, with the failures the Bun handler catches as `Err`.
    async fn check_recipient(
        &self,
        address: &SmtpAddress,
        session: &SmtpSession,
        ip: &str,
    ) -> Result<Result<(), SmtpHookError>, anyhow::Error> {
        self.check_credential_version(session).await.map_err(anyhow::Error::new)?;

        let RecipientCheck::Lookup(username) =
            recipient_check(&address.address, self.directory.email_domain().as_deref())
        else {
            return Ok(Ok(()));
        };
        if is_recipient_probe_rate_limited(ip) {
            return Ok(Err(refusals::too_many_probes()));
        }
        let user = if username.is_empty() { None } else { self.directory.get_user(&username).await? };
        if user.is_some() {
            return Ok(Ok(()));
        }
        logger::warn("SMTP: refused a recipient with no mailbox", json!({ "remoteAddress": ip, "username": username }));
        record_recipient_probe(ip).await;
        Ok(Err(refusals::no_such_mailbox()))
    }

    async fn data(&self, stream: SmtpDataStream, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        let Some(email_domain) = self.directory.email_domain() else {
            logger::warn("SMTP: EMAIL_DOMAIN not set, rejecting all emails.", json!({}));
            return Err(refusals::not_configured());
        };

        let Some(lane) = choose_lane(&session.envelope, session.user.as_deref(), &email_domain) else {
            logger::warn(
                "SMTP: refused to relay a message with no local party",
                json!({ "remoteAddress": session.remote_address }),
            );
            return Err(refusals::relay_denied());
        };

        // The slot is taken before either lane touches the stream, so an
        // unadmitted transaction stays at its stream buffer instead of a
        // whole decoded message.
        let session = &*session;
        let outcome = with_smtp_data_budget(|| async move {
            let message = BoundedMessage::new(stream, &session.remote_address);
            match lane {
                DataLane::Outgoing => self.outgoing(message, session).await,
                DataLane::Incoming => self.incoming(message, session).await,
            }
        })
        .await;

        match outcome {
            // A refused stream is dropped with the closure, and the server
            // reads the rest of it out before replying.
            Err(unavailable) => Err(unavailable.into()),
            Ok(Ok(())) => Ok(()),
            Ok(Err(DataFailure::Refused(refusal))) => Err(refusal),
            Ok(Err(DataFailure::Failed(error))) => {
                logger::error_with("Error handling DATA transaction", json!({}), &error);
                Err(SmtpHookError::new(error_message(&error)))
            }
        }
    }
}

impl<D: MailDirectory> SmtpHandler for MailHandler<D> {
    async fn on_auth(
        &self,
        auth: SmtpAuthRequest,
        session: &mut SmtpSession,
    ) -> Result<SmtpAuthResponse, SmtpHookError> {
        self.authenticate(auth, session).await
    }

    async fn on_mail_from(&self, _address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        self.mail_from(session).await
    }

    async fn on_rcpt_to(&self, address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        self.rcpt_to(address, session).await
    }

    async fn on_data(&self, stream: SmtpDataStream, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        self.data(stream, session).await
    }
}
