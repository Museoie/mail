//! Domains as resources (domains.ts): which domain an address belongs to, who
//! may read and send for it, and how a person proves they control a domain
//! they add.
//!
//! The coarse "may use mail at all" decision stays with the identity provider
//! when single sign-on is on; everything here is mail's own, keyed by mail's
//! user ids, and works the same with no provider at all.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock, RwLock};

use futures_util::future::BoxFuture;

use crate::postgres::repositories::domains::{
    assign_domain, find_domain, list_domain_access, list_former_domains, readers_of_domain, DomainAccess,
    DomainRole, DOMAIN_NAME_MAX_LENGTH,
};
use crate::postgres::repositories::users::find_user_by_username;
use crate::postgres::DbResult;
use crate::util::{get_domain, is_routable_username};

/// The zone `EMAIL_DOMAIN` names, lower-cased: every account's own name sits under it.
pub fn served_zone() -> String {
    get_domain().to_lowercase()
}

/// Whether `name` is the served zone or a name under it.
pub fn is_in_served_zone(name: &str) -> bool {
    let zone = served_zone();
    let target = name.to_lowercase();
    target == zone || target.ends_with(&format!(".{zone}"))
}

/// The account's own name under the served zone, e.g. `bob.example.com`.
pub fn account_domain(username: &str) -> String {
    format!("{}.{}", username.to_lowercase(), served_zone())
}

/// The lower-cased domain part of an address, or "" when it has none.
pub fn domain_of_address(address: &str) -> String {
    match address.rfind('@') {
        Some(at) => address[at + 1..].trim().to_lowercase(),
        None => String::new(),
    }
}

fn is_label(label: &str) -> bool {
    is_routable_username(label)
}

/// The canonical spelling of a domain name someone typed, or `None` when it is
/// not one: lower-case ASCII labels, at least two of them, no trailing dot, at
/// most 253 characters, and a top-level label that is not all digits.
pub fn normalize_domain_name(input: &str) -> Option<String> {
    let lowered = input.trim().to_lowercase();
    let name = lowered.strip_suffix('.').unwrap_or(&lowered);
    if name.is_empty() || name.chars().count() > DOMAIN_NAME_MAX_LENGTH {
        return None;
    }
    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 2 || !labels.iter().all(|label| is_label(label)) {
        return None;
    }
    if labels.last().is_some_and(|label| label.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    Some(name.to_string())
}

/// The DNS label a verification record goes under, ahead of the domain.
pub const VERIFICATION_LABEL: &str = "_mail-verification";

/// The TXT record that proves control of a domain for a claim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VerificationRecord {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub host: String,
    pub value: String,
}

/// The TXT record that proves control of `name` for the claim holding `token`.
pub fn verification_record(name: &str, token: &str) -> VerificationRecord {
    VerificationRecord {
        kind: "TXT",
        host: format!("{VERIFICATION_LABEL}.{name}"),
        value: format!("mail-verification={token}"),
    }
}

pub fn new_verification_token() -> String {
    let bytes: [u8; 24] = rand::random();
    hex::encode(bytes)
}

/// What a TXT lookup can end in, short of records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxtLookupError {
    /// The name has no such record.
    NoRecord,
    /// DNS could not be asked.
    Unavailable,
}

/// The TXT lookup a verification makes; replaceable for tests.
pub trait TxtResolver: Send + Sync {
    fn resolve_txt<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Result<Vec<Vec<String>>, TxtLookupError>>;
}

struct SystemTxtResolver;

impl TxtResolver for SystemTxtResolver {
    fn resolve_txt<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Result<Vec<Vec<String>>, TxtLookupError>> {
        Box::pin(async move {
            use hickory_resolver::error::ResolveErrorKind;
            let resolver = hickory_resolver::TokioAsyncResolver::tokio_from_system_conf()
                .map_err(|_| TxtLookupError::Unavailable)?;
            match resolver.txt_lookup(host).await {
                Ok(lookup) => Ok(lookup
                    .iter()
                    .map(|txt| txt.txt_data().iter().map(|chunk| String::from_utf8_lossy(chunk).into_owned()).collect())
                    .collect()),
                Err(error) => match error.kind() {
                    ResolveErrorKind::NoRecordsFound { .. } => Err(TxtLookupError::NoRecord),
                    _ => Err(TxtLookupError::Unavailable),
                },
            }
        })
    }
}

fn resolver_slot() -> &'static RwLock<Arc<dyn TxtResolver>> {
    static SLOT: OnceLock<RwLock<Arc<dyn TxtResolver>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(Arc::new(SystemTxtResolver)))
}

/// Replaces the TXT lookup; for tests. Returns the previous resolver.
pub fn set_txt_resolver(resolver: Arc<dyn TxtResolver>) -> Arc<dyn TxtResolver> {
    let mut slot = resolver_slot().write().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::replace(&mut *slot, resolver)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationCheck {
    Found,
    Missing,
    Unavailable,
}

/// Looks up the verification record for a claim. A TXT record may come back
/// split into several strings, which DNS joins, so each record's chunks are
/// joined before comparing.
pub async fn check_verification_record(name: &str, token: &str) -> VerificationCheck {
    let record = verification_record(name, token);
    let resolver = resolver_slot().read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    match resolver.resolve_txt(&record.host).await {
        Ok(records) => {
            if records.iter().any(|chunks| chunks.concat().trim() == record.value) {
                VerificationCheck::Found
            } else {
                VerificationCheck::Missing
            }
        }
        Err(TxtLookupError::NoRecord) => VerificationCheck::Missing,
        Err(TxtLookupError::Unavailable) => VerificationCheck::Unavailable,
    }
}

/// The domain an address is delivered into, and the owner whose rows hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryDomain {
    pub domain: String,
    pub owner_id: String,
}

/// The domain this server delivers `address` into, or `None` when it
/// delivers nowhere for it.
///
/// A domain row decides. An account's own name under the served zone that has
/// no row yet gets its row here, the same one the boot backfill gives it.
pub async fn resolve_delivery_domain(address: &str) -> DbResult<Option<DeliveryDomain>> {
    let domain = domain_of_address(address);
    if domain.is_empty() {
        return Ok(None);
    }
    if let Some(row) = find_domain(&domain).await? {
        // A soft-deleted account has no mailbox, and its name stays taken.
        return Ok((!row.owner_deleted).then(|| DeliveryDomain { domain: row.name, owner_id: row.owner_id }));
    }

    let zone = served_zone();
    let Some(label) = domain.strip_suffix(&format!(".{zone}")) else { return Ok(None) };
    if !is_routable_username(label) {
        return Ok(None);
    }
    let Some(account) = find_user_by_username(label).await? else { return Ok(None) };
    // A sign-up still setting its password counts, as in the boot backfill.
    if account.is_deleted == Some(true) {
        return Ok(None);
    }
    if !assign_domain(&account.user_id, &domain).await? {
        let raced = find_domain(&domain).await?;
        return Ok(raced.map(|row| DeliveryDomain { domain: row.name, owner_id: row.owner_id }));
    }
    Ok(Some(DeliveryDomain { domain, owner_id: account.user_id }))
}

/// Whether anyone but `account_id` can read mail delivered to `address`: its
/// domain's owner or a viewer of it. With no `account_id`, whether anyone can
/// read it at all.
pub async fn others_can_read_address(address: &str, account_id: Option<&str>) -> DbResult<bool> {
    let Some(delivery) = resolve_delivery_domain(address).await? else { return Ok(false) };
    let readers = readers_of_domain(&delivery.domain).await?;
    Ok(readers.iter().any(|reader| Some(reader.as_str()) != account_id))
}

/// The names of the domains `user_id` owns.
pub async fn owned_domain_names(user_id: &str) -> DbResult<Vec<String>> {
    Ok(list_domain_access(user_id)
        .await?
        .into_iter()
        .filter(|access| access.role == DomainRole::Owner)
        .map(|access| access.name)
        .collect())
}

/// The domains whose addresses are `user_id`'s own: the ones they own, and
/// the ones they used to own, whose mail stayed with them.
pub async fn own_address_domains(user_id: &str) -> DbResult<Vec<String>> {
    let owned = owned_domain_names(user_id).await?;
    let former = list_former_domains(user_id).await?;
    let mut seen = BTreeSet::new();
    Ok(owned.into_iter().chain(former).filter(|name| seen.insert(name.clone())).collect())
}

/// Whether an address is on one of a set of domains.
#[derive(Debug, Clone, Default)]
pub struct AddressOnDomains {
    names: BTreeSet<String>,
}

impl AddressOnDomains {
    pub fn new<S: AsRef<str>>(domains: &[S]) -> AddressOnDomains {
        AddressOnDomains { names: domains.iter().map(|domain| domain.as_ref().to_lowercase()).collect() }
    }

    pub fn matches(&self, address: &str) -> bool {
        self.names.contains(&domain_of_address(address))
    }
}

/// The domains `user_id` holds the viewer role on.
pub async fn viewed_domains(user_id: &str) -> DbResult<Vec<DomainAccess>> {
    Ok(list_domain_access(user_id).await?.into_iter().filter(|access| access.role == DomainRole::Viewer).collect())
}

/// The domain a send goes out from when the sender names none: the served
/// zone itself when the account owns it, then the account's own name, then
/// the first domain it owns.
pub fn default_domain(owned: &[String], username: &str) -> Option<String> {
    let zone = served_zone();
    if owned.contains(&zone) {
        return Some(zone);
    }
    let own = account_domain(username);
    if owned.contains(&own) {
        return Some(own);
    }
    owned.first().cloned()
}

/// The Mailgun sending domain for mail from `domain`: the served zone for any
/// name under it, and the domain itself otherwise.
pub fn mailgun_domain_for(domain: &str) -> String {
    if is_in_served_zone(domain) {
        get_domain()
    } else {
        domain.to_string()
    }
}

/// An IMAP or SMTP login split into the person and the domain it is scoped
/// to: `bob` signs bob in to everything bob owns, `bob@example.com` to the
/// one domain `example.com`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedLogin {
    pub username: String,
    pub domain: Option<String>,
}

pub fn parse_scoped_login(login: &str) -> ScopedLogin {
    match login.rfind('@') {
        None => ScopedLogin { username: login.to_string(), domain: None },
        Some(at) => ScopedLogin { username: login[..at].to_string(), domain: Some(login[at + 1..].to_lowercase()) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_names_are_normalized_or_refused() {
        assert_eq!(normalize_domain_name(" Example.NET. ").as_deref(), Some("example.net"));
        assert_eq!(normalize_domain_name("a.b.example").as_deref(), Some("a.b.example"));
        for bad in ["", "example", "-a.example", "a_b.example", "1.2.3.4", "a..example", &"a.".repeat(130)] {
            assert_eq!(normalize_domain_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn addresses_split_on_their_last_at() {
        assert_eq!(domain_of_address("A@B@Example.ORG "), "example.org");
        assert_eq!(domain_of_address("nobody"), "");
        assert_eq!(
            parse_scoped_login("bob@Example.com"),
            ScopedLogin { username: "bob".into(), domain: Some("example.com".into()) }
        );
        assert_eq!(parse_scoped_login("bob"), ScopedLogin { username: "bob".into(), domain: None });
    }

    #[test]
    fn the_default_domain_prefers_the_zone_then_the_own_name() {
        let zone = served_zone();
        let own = account_domain("bob");
        assert_eq!(default_domain(&["x.example".into(), own.clone()], "bob"), Some(own.clone()));
        assert_eq!(default_domain(&[own.clone(), zone.clone()], "bob"), Some(zone));
        assert_eq!(default_domain(&["x.example".into()], "bob").as_deref(), Some("x.example"));
        assert_eq!(default_domain(&[], "bob"), None);
    }

    #[test]
    fn verification_records_name_the_label_and_token() {
        let record = verification_record("example.net", "abc");
        assert_eq!(record.host, "_mail-verification.example.net");
        assert_eq!(record.value, "mail-verification=abc");
        assert_eq!(new_verification_token().len(), 48);
    }

    #[test]
    fn address_predicates_ignore_case() {
        let own = AddressOnDomains::new(&["Example.org"]);
        assert!(own.matches("Info@EXAMPLE.org"));
        assert!(!own.matches("info@other.example"));
    }
}
