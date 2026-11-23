//! Whose mail rows a request reads (http/routes/mails/reader.ts).

use crate::domains::{domain_of_address, own_address_domains, AddressOnDomains};
use crate::models::SignedUser;
use crate::postgres::repositories::domains::{get_domain_access, DomainRole};
use crate::postgres::DbResult;

/// Whose mail rows a request reads, and which of their domains it may see.
#[derive(Debug, Clone)]
pub struct MailReader {
    /// The store the rows are keyed by: the user, or the owner of a viewed domain.
    pub store_id: String,
    /// `None` reads the whole store (the user's own); set, only those domains.
    pub domains: Option<Vec<String>>,
    /// The addresses the reader may see as a received mail's envelope recipients.
    pub readers: AddressOnDomains,
}

/// The reader for an account (an address): its owner's rows, filtered to the
/// domain, for a domain the user views; otherwise the user's own rows, which
/// hold whatever mail they have for the address. `None` for an account with
/// no domain. The role is read in the same request, so a revoked one stops
/// the next read.
pub async fn resolve_mail_reader(user: &SignedUser, account: &str) -> DbResult<Option<MailReader>> {
    let domain = domain_of_address(account);
    if domain.is_empty() {
        return Ok(None);
    }
    if let Some(access) = get_domain_access(&user.id, &domain).await? {
        if access.role == DomainRole::Viewer {
            return Ok(Some(MailReader {
                store_id: access.owner_id,
                readers: AddressOnDomains::new(&[access.name.clone()]),
                domains: Some(vec![access.name]),
            }));
        }
    }
    Ok(Some(MailReader {
        store_id: user.id.clone(),
        domains: None,
        readers: AddressOnDomains::new(&own_address_domains(&user.id).await?),
    }))
}
