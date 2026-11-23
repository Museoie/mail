//! What happens to the mail an SMTP listener takes in: AUTH, MAIL FROM,
//! RCPT TO and DATA for inbound delivery and authenticated submission (the
//! mail-handling half of `smtp.ts`).
//!
//! ```ignore
//! let servers = smtp::handler::initialize_mail_smtp().await?;
//! // ...
//! smtp::close_smtp_servers(&servers).await;
//! ```

use std::collections::HashSet;
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
use crate::domains::{domain_of_address, is_in_served_zone, parse_scoped_login, resolve_delivery_domain, DeliveryDomain};
use crate::models::User;
use crate::postgres::repositories::domains::{find_domain, get_domain_access, DomainRole};
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

/// What AUTH, MAIL FROM and RCPT TO learn about a session's domains, for the
/// hooks after them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainScope {
    /// The one domain a `user@domain` login sends from.
    pub scope_domain: Option<String>,
    /// Whether this transaction's MAIL FROM names a domain this host serves.
    pub mail_from_served: bool,
    /// This transaction's envelope recipients this host delivers into, lower-cased.
    pub served_recipients: HashSet<String>,
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

    /// An authenticated user who owns no domain (or not the one their login
    /// is scoped to) has nothing to send from. Permanent, like a relay refusal.
    pub fn no_sending_domain() -> SmtpHookError {
        SmtpHookError::with_code(550, "5.7.1 No domain to send from")
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

    /// The role `user_id` holds on `domain`, `None` for none.
    fn domain_role(&self, user_id: &str, domain: &str) -> impl Future<Output = Result<Option<(DomainRole, String)>>> + Send;

    /// The domain this host delivers `address` into (`resolveDeliveryDomain`).
    fn resolve_delivery(&self, address: &str) -> impl Future<Output = Result<Option<DeliveryDomain>>> + Send;

    /// Whether a domain row names `domain`.
    fn has_domain(&self, domain: &str) -> impl Future<Output = Result<bool>> + Send;

    /// The names of the domains `user_id` owns.
    fn owned_domains(&self, user_id: &str) -> impl Future<Output = Result<Vec<String>>> + Send;
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

    async fn domain_role(&self, user_id: &str, domain: &str) -> Result<Option<(DomainRole, String)>> {
        Ok(get_domain_access(user_id, domain).await?.map(|access| (access.role, access.name)))
    }

    async fn resolve_delivery(&self, address: &str) -> Result<Option<DeliveryDomain>> {
        Ok(resolve_delivery_domain(address).await?)
    }

    async fn has_domain(&self, domain: &str) -> Result<bool> {
        Ok(find_domain(domain).await?.is_some())
    }

    async fn owned_domains(&self, user_id: &str) -> Result<Vec<String>> {
        Ok(crate::domains::owned_domain_names(user_id).await?)
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
/// A recipient is this host's when it is under the served zone, or one RCPT
/// TO found a domain row for (`scope.served_recipients`). `MAIL FROM` is a
/// value any sender chooses freely, so a local one (under the zone, or a
/// served domain as MAIL FROM found) selects submission only on a session
/// that authenticated. Without that, inbound mail whose envelope sender sits
/// under the served zone — a forwarder, a bounce, a spoof — would be routed
/// to submission and refused.
pub fn choose_lane(
    envelope: &SmtpEnvelope,
    user: Option<&str>,
    email_domain: &str,
    scope: Option<&DomainScope>,
) -> Option<DataLane> {
    let served = |address: &str| scope.is_some_and(|scope| scope.served_recipients.contains(&js_normalize(address)));
    let incoming = envelope
        .rcpt_to
        .iter()
        .any(|address| is_local_address(&address.address, email_domain) || served(&address.address));
    let mail_from_served = scope.is_some_and(|scope| scope.mail_from_served);
    let outgoing = user.is_some_and(|user| !user.is_empty())
        && envelope
            .mail_from
            .as_ref()
            .is_some_and(|from| is_local_address(&from.address, email_domain) || mail_from_served);
    if outgoing {
        Some(DataLane::Outgoing)
    } else if incoming {
        Some(DataLane::Incoming)
    } else {
        None
    }
}

/// `address.trim().toLowerCase()`.
fn js_normalize(address: &str) -> String {
    crate::js::trim(address).to_lowercase()
}

/// Whether this host receives mail for `domain`: the served zone or a domain row.
async fn is_served_domain<D: MailDirectory>(directory: &D, domain: &str) -> Result<bool> {
    Ok(!domain.is_empty() && (is_in_served_zone(domain) || directory.has_domain(domain).await?))
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
        // `bob@example.com` signs bob in to send only from example.com, which
        // bob has to own; plain `bob` sends from any domain bob owns.
        let SmtpAuthRequest { username: login, password, .. } = auth;
        let scoped = parse_scoped_login(&login);
        let (username, scope_domain) = (scoped.username, scoped.domain);
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
            if let (Some(user), Some(signed), true) = (&user, &signed, matches) {
                let version = user.password_version.unwrap_or(1);
                return self.grant(session, &ip, account, &username, scope_domain.as_deref(), &signed.id, version).await;
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
            AppPasswordCheck::Accepted { user, password_version } => {
                self.grant(session, &ip, account, &username, scope_domain.as_deref(), &user.id, password_version).await
            }
        }
    }

    /// Lets an authenticated credential in. A scope names a domain the person
    /// must own: a viewer reads a domain and never sends as it. Refused like
    /// a wrong password, so the answer says nothing about which half of the
    /// login was wrong.
    #[allow(clippy::too_many_arguments)]
    async fn grant(
        &self,
        session: &mut SmtpSession,
        ip: &str,
        account: Option<&str>,
        username: &str,
        scope_domain: Option<&str>,
        user_id: &str,
        password_version: i64,
    ) -> Result<SmtpAuthResponse, SmtpHookError> {
        let mut scope = DomainScope::default();
        if let Some(scope_domain) = scope_domain {
            let role = match self.directory.domain_role(user_id, scope_domain).await {
                Ok(role) => role,
                Err(error) => {
                    logger::error_with("SMTP: authentication lookup failed", json!({ "remoteAddress": ip }), &error);
                    return Err(refusals::lookup_failed());
                }
            };
            match role {
                Some((DomainRole::Owner, name)) => scope.scope_domain = Some(name),
                _ => {
                    record_auth_failure(ip, account).await;
                    return Ok(SmtpAuthResponse::reject());
                }
            }
        }
        // Scoped reset: forgive only the authenticating account's own bucket.
        reset_auth_failures(ip, account);
        // Credential-revocation fence: the generation the connection
        // authenticated on, which the per-command gates re-check.
        session.state.insert(CredentialStamp { password_version });
        session.state.insert(scope);
        Ok(SmtpAuthResponse::accept(username.to_string()))
    }

    /// Starts a transaction: re-checks the credential, and for an
    /// authenticated session records whether the sender names a domain this
    /// host serves, which DATA reads to choose the submission lane.
    async fn mail_from(&self, address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        let scope_domain = session.state.get::<DomainScope>().and_then(|scope| scope.scope_domain.clone());
        session.state.insert(DomainScope { scope_domain, ..DomainScope::default() });
        self.check_credential_version(session).await?;
        if session.user.is_some() {
            let served = match is_served_domain(&self.directory, &domain_of_address(&address.address)).await {
                Ok(served) => served,
                Err(error) => {
                    logger::error_with("SMTP: sender domain lookup failed", json!({ "remoteAddress": session.remote_address }), &error);
                    return Err(refusals::lookup_failed());
                }
            };
            if let Some(scope) = session.state.get_mut::<DomainScope>() {
                scope.mail_from_served = served;
            }
        }
        Ok(())
    }

    /// Accepts a recipient this host delivers into (a domain row, or an
    /// account's own name under the served zone) and refuses one inside the
    /// served zone that maps onto no account. The accepted ones are recorded
    /// on the session for DATA.
    ///
    /// The refusal is also a yes/no on username existence, and RCPT TO is
    /// taken unauthenticated, so each one under the zone is charged to a
    /// per-IP budget priced as a failed credential is. Past the budget the
    /// answer for a name under the zone is a uniform transient refusal and no
    /// lookup runs; any other domain is looked up freely, since which domains
    /// a host serves is public in DNS.
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
        session: &mut SmtpSession,
        ip: &str,
    ) -> Result<Result<(), SmtpHookError>, anyhow::Error> {
        self.check_credential_version(session).await.map_err(anyhow::Error::new)?;

        let Some(email_domain) = self.directory.email_domain() else {
            return Ok(Ok(()));
        };
        let local = is_local_address(&address.address, &email_domain);
        if local && is_recipient_probe_rate_limited(ip) {
            return Ok(Err(refusals::too_many_probes()));
        }

        if self.directory.resolve_delivery(&address.address).await?.is_some() {
            if session.state.get::<DomainScope>().is_none() {
                session.state.insert(DomainScope::default());
            }
            if let Some(scope) = session.state.get_mut::<DomainScope>() {
                scope.served_recipients.insert(js_normalize(&address.address));
            }
            return Ok(Ok(()));
        }
        // A foreign recipient is not this host's to judge: the relay decision
        // belongs to DATA, which needs the envelope complete.
        if !local {
            return Ok(Ok(()));
        }

        let username = domain_of_address(&address.address);
        logger::warn("SMTP: refused a recipient with no mailbox", json!({ "remoteAddress": ip, "username": username }));
        record_recipient_probe(ip).await;
        Ok(Err(refusals::no_such_mailbox()))
    }

    async fn data(&self, stream: SmtpDataStream, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        let Some(email_domain) = self.directory.email_domain() else {
            logger::warn("SMTP: EMAIL_DOMAIN not set, rejecting all emails.", json!({}));
            return Err(refusals::not_configured());
        };

        let scope = session.state.get::<DomainScope>();
        let Some(lane) = choose_lane(&session.envelope, session.user.as_deref(), &email_domain, scope) else {
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

    async fn on_mail_from(&self, address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        self.mail_from(address, session).await
    }

    async fn on_rcpt_to(&self, address: &SmtpAddress, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        self.rcpt_to(address, session).await
    }

    async fn on_data(&self, stream: SmtpDataStream, session: &mut SmtpSession) -> Result<(), SmtpHookError> {
        self.data(stream, session).await
    }
}
