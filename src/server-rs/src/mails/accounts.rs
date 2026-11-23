//! The account sidebar: mail counts per address (mails/accounts.ts).

use serde::Serialize;

use crate::js;
use crate::models::SignedUser;
use crate::postgres::repositories::mails::http::{get_account_stats, search_account_stats, AccountStat};
use crate::util::get_user_domain;

/// One address and its counts, as the client's `Account` serializes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Account {
    pub key: String,
    pub doc_count: i64,
    pub unread_doc_count: i64,
    pub saved_doc_count: i64,
    /// ISO time of the newest mail.
    pub updated: String,
}

impl From<AccountStat> for Account {
    fn from(stat: AccountStat) -> Account {
        Account {
            key: stat.address,
            doc_count: stat.count,
            unread_doc_count: stat.unread,
            saved_doc_count: stat.saved,
            updated: if stat.latest.is_empty() { js::now_iso() } else { stat.latest },
        }
    }
}

/// Received, sent and spam accounts, each newest first.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Accounts {
    pub received: Vec<Account>,
    pub sent: Vec<Account>,
    pub spam: Vec<Account>,
}

/// The user's accounts: received groups by recipient (To, Cc, Bcc and the
/// envelope), sent by From, spam by recipient of spam. Only addresses on the
/// user's own domain are kept.
pub async fn get_accounts(user: &SignedUser) -> Accounts {
    let user_domain = get_user_domain(&user.username);
    let domain = Some(user_domain.as_str());
    let (received, sent, spam) = tokio::join!(
        get_account_stats(&user.id, false, domain, false),
        get_account_stats(&user.id, true, domain, false),
        get_account_stats(&user.id, false, domain, true),
    );
    Accounts {
        received: received.into_iter().map(Account::from).collect(),
        sent: sent.into_iter().map(Account::from).collect(),
        spam: spam.into_iter().map(Account::from).collect(),
    }
}

/// Received accounts that own at least one mail matching `value`.
pub async fn search_accounts(user: &SignedUser, value: &str) -> Vec<Account> {
    let value = js::trim(value);
    if value.is_empty() {
        return Vec::new();
    }
    let user_domain = get_user_domain(&user.username);
    search_account_stats(&user.id, value, Some(&user_domain)).await.into_iter().map(Account::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_serializes_in_the_client_order() {
        let account = Account::from(AccountStat {
            address: "a@bob.mail.test".into(),
            count: 2,
            unread: 1,
            saved: 0,
            latest: "2026-01-01T00:00:00.000Z".into(),
        });
        assert_eq!(
            serde_json::to_string(&account).unwrap(),
            r#"{"key":"a@bob.mail.test","doc_count":2,"unread_doc_count":1,"saved_doc_count":0,"updated":"2026-01-01T00:00:00.000Z"}"#
        );
    }
}
