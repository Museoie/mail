//! Finding, linking or creating the local account for a sign-in (sso/accounts.ts).

use std::collections::HashMap;
use std::future::Future;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::js;
use crate::models::SignedUser;
use crate::postgres::repositories::users::{
    find_unlinked_users_by_email, find_user_by_auth_sub, find_user_by_username, insert_sso_user, link_user_auth_sub,
    update_user_email, UserModel,
};
use crate::read_only::{is_reserved_username, ADMIN_RO_USERNAME, ADMIN_USERNAME};
use crate::users::is_valid_email;
use crate::util::{get_user_domain, is_routable_username};

/// A local account as the sign-in callback sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAccount {
    pub user_id: String,
    pub username: String,
    pub email: Option<String>,
    pub auth_sub: Option<String>,
    pub password_version: i64,
}

impl LocalAccount {
    /// The identity a session started for this account carries.
    pub fn to_signed_user(&self) -> SignedUser {
        let mut user = SignedUser::new(Some(self.user_id.clone()), Some(self.username.clone()), self.email.clone());
        user.password_version = Some(self.password_version);
        user
    }
}

impl From<&UserModel> for LocalAccount {
    fn from(model: &UserModel) -> LocalAccount {
        LocalAccount {
            user_id: model.user_id.clone(),
            username: model.username.clone(),
            email: model.email.clone(),
            auth_sub: model.auth_sub.clone(),
            password_version: model.password_version.unwrap_or(1),
        }
    }
}

/// The account reads and writes the callback makes, so the rules run without a database.
pub trait AccountDirectory: Sync {
    fn find_by_sub(&self, sub: &str) -> impl Future<Output = Result<Option<LocalAccount>>> + Send;
    /// Any account with this username, linked or not.
    fn find_by_username(&self, username: &str) -> impl Future<Output = Result<Option<LocalAccount>>> + Send;
    /// Signed-up accounts with this address ignoring case, not linked or
    /// deleted, minus `excluded_usernames`.
    fn find_unlinked_by_email(
        &self,
        email: &str,
        excluded_usernames: &[String],
    ) -> impl Future<Output = Result<Vec<LocalAccount>>> + Send;
    /// `None` when the account or the subject was linked by another sign-in first.
    fn link(&self, user_id: &str, sub: &str) -> impl Future<Output = Result<Option<LocalAccount>>> + Send;
    /// `None` when the username or the subject is taken.
    fn create(
        &self,
        username: &str,
        email: &str,
        sub: &str,
    ) -> impl Future<Output = Result<Option<LocalAccount>>> + Send;
    fn update_email(&self, user_id: &str, email: &str) -> impl Future<Output = Result<()>> + Send;
}

/// Who the provider says signed in: the access token's claims and the ID token's address.
#[derive(Debug, Clone, PartialEq)]
pub struct SsoIdentity {
    pub sub: String,
    pub username: String,
    /// The ID token's `email` claim as it came; `Value::Null` when absent.
    pub email: Value,
    /// Whether the token marks the address verified (the claim is exactly `true`).
    pub email_verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsoAccountRefusal {
    AccountExists,
    InvalidUsername,
}

impl SsoAccountRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            SsoAccountRefusal::AccountExists => "account_exists",
            SsoAccountRefusal::InvalidUsername => "invalid_username",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsoAccountOutcome {
    Existing,
    Linked,
    Created,
}

impl SsoAccountOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            SsoAccountOutcome::Existing => "existing",
            SsoAccountOutcome::Linked => "linked",
            SsoAccountOutcome::Created => "created",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsoAccountResult {
    Found { account: LocalAccount, outcome: SsoAccountOutcome },
    Refused(SsoAccountRefusal),
}

const EMAIL_MAX_LENGTH: usize = 255;

/// The address of the identity when it is one this app would store. An
/// `email` claim that is not text is the fault the Bun server makes of it.
fn email_of(identity: &SsoIdentity) -> Result<Option<&str>> {
    // What JavaScript raises when the address check calls a string method on it.
    let not_text = || anyhow!("email.split is not a function. (In 'email.split(\"@\")', 'email.split' is undefined)");
    match &identity.email {
        Value::Null | Value::Bool(false) => Ok(None),
        Value::String(email) => {
            let usable = !email.is_empty() && js::utf16_len(email) <= EMAIL_MAX_LENGTH && is_valid_email(email);
            Ok(usable.then_some(email.as_str()))
        }
        Value::Number(number) if number.as_f64() == Some(0.0) => Ok(None),
        // A list longer than an address may be is dismissed by the length check first.
        Value::Array(list) if list.len() > EMAIL_MAX_LENGTH => Ok(None),
        Value::Object(object)
            if object.get("length").and_then(Value::as_f64).is_some_and(|length| length > EMAIL_MAX_LENGTH as f64) =>
        {
            Ok(None)
        }
        _ => Err(not_text()),
    }
}

/// Finds the local account a single sign-on belongs to, linking or creating it
/// on the first one:
///
/// 1. the account already linked to the subject;
/// 2. the account `link_users` names for the provider username, if not linked yet;
/// 3. the one signed-up account, not linked yet, whose address is the ID
///    token's email, verified or not. This app's sign-up proved that address,
///    and the provider sets an address a person chose only after they confirm
///    it (one it has not verified was entered by its administrator, on account
///    creation or import), so the match is as trustworthy as `link_users`. A
///    provider that lets people claim addresses they have not confirmed must
///    not be used this way. The reserved accounts never link this way:
///    `admin-ro` is a local account only, and mail to `admin`'s address is
///    readable by the read-only `admin-ro` login.
/// 4. a new account named after the provider username, unless that is not a
///    routable mailbox name or an account by that name exists: guessing
///    whose it is would hand someone else's mail over.
///
/// A new account takes the token's email, verified or not. Each sign-in copies
/// a changed address onto the account only when the token marks it verified.
///
/// Every link is a conditional write, so a sign-in that loses a race to link
/// the same account or subject takes the winner's result instead.
pub async fn resolve_sso_account<D: AccountDirectory + ?Sized>(
    identity: &SsoIdentity,
    link_users: &HashMap<String, String>,
    directory: &D,
) -> Result<SsoAccountResult> {
    let SsoIdentity { sub, username, .. } = identity;
    let email = email_of(identity)?;
    let verified_email = if identity.email_verified { email } else { None };

    let found = |account: LocalAccount, outcome: SsoAccountOutcome| async move {
        let account = match verified_email {
            Some(verified) if account.email.as_deref() != Some(verified) => {
                // The reserved accounts' address is derived from EMAIL_DOMAIN at boot.
                if is_reserved_username(Some(&account.username)) {
                    account
                } else {
                    directory.update_email(&account.user_id, verified).await?;
                    LocalAccount { email: Some(verified.to_string()), ..account }
                }
            }
            _ => account,
        };
        Ok(SsoAccountResult::Found { account, outcome })
    };

    let try_link = |candidate: Option<LocalAccount>| async move {
        match candidate {
            Some(candidate) if candidate.auth_sub.is_none() => match directory.link(&candidate.user_id, sub).await? {
                Some(linked) => Ok(Some(linked)),
                None => directory.find_by_sub(sub).await,
            },
            _ => Ok(None),
        }
    };

    if let Some(existing) = directory.find_by_sub(sub).await? {
        return found(existing, SsoAccountOutcome::Existing).await;
    }

    if let Some(mapped) = link_users.get(username) {
        if let Some(linked) = try_link(directory.find_by_username(mapped).await?).await? {
            return found(linked, SsoAccountOutcome::Linked).await;
        }
    }

    if let Some(email) = email {
        let reserved = [ADMIN_USERNAME.to_string(), ADMIN_RO_USERNAME.to_string()];
        let mut candidates = directory.find_unlinked_by_email(email, &reserved).await?;
        if candidates.len() == 1 {
            if let Some(linked) = try_link(candidates.pop()).await? {
                return found(linked, SsoAccountOutcome::Linked).await;
            }
        }
    }

    if !is_routable_username(username) || username == ADMIN_RO_USERNAME {
        return Ok(SsoAccountResult::Refused(SsoAccountRefusal::InvalidUsername));
    }
    if directory.find_by_username(username).await?.is_some() {
        return Ok(SsoAccountResult::Refused(SsoAccountRefusal::AccountExists));
    }
    // Without an address in the token the account's own mailbox stands in:
    // the reset mail then reaches only its owner.
    let own_mailbox = format!("{username}@{}", get_user_domain(username));
    let created = directory.create(username, email.unwrap_or(&own_mailbox), sub).await?;
    if let Some(created) = created {
        return found(created, SsoAccountOutcome::Created).await;
    }
    match directory.find_by_sub(sub).await? {
        Some(raced) => found(raced, SsoAccountOutcome::Existing).await,
        None => Ok(SsoAccountResult::Refused(SsoAccountRefusal::AccountExists)),
    }
}

/// The accounts in the `users` table.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresAccountDirectory;

fn to_found_account(model: Option<UserModel>) -> Option<LocalAccount> {
    model.as_ref().map(LocalAccount::from)
}

impl AccountDirectory for PostgresAccountDirectory {
    async fn find_by_sub(&self, sub: &str) -> Result<Option<LocalAccount>> {
        Ok(to_found_account(find_user_by_auth_sub(sub).await?))
    }

    async fn find_by_username(&self, username: &str) -> Result<Option<LocalAccount>> {
        Ok(to_found_account(find_user_by_username(username).await?))
    }

    async fn find_unlinked_by_email(&self, email: &str, excluded_usernames: &[String]) -> Result<Vec<LocalAccount>> {
        let models = find_unlinked_users_by_email(email, excluded_usernames).await?;
        Ok(models.iter().map(LocalAccount::from).collect())
    }

    async fn link(&self, user_id: &str, sub: &str) -> Result<Option<LocalAccount>> {
        Ok(to_found_account(link_user_auth_sub(user_id, sub).await?))
    }

    async fn create(&self, username: &str, email: &str, sub: &str) -> Result<Option<LocalAccount>> {
        Ok(to_found_account(insert_sso_user(username, email, sub).await?))
    }

    async fn update_email(&self, user_id: &str, email: &str) -> Result<()> {
        Ok(update_user_email(user_id, email).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    pub(crate) struct Row {
        pub account: LocalAccount,
        /// False for a sign-up still waiting for its link: it has no password.
        pub signed_up: bool,
        pub deleted: bool,
    }

    /// What a test wants a write to do instead of what the table would.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub(crate) enum Race {
        #[default]
        None,
        /// `link` loses to another sign-in that links the same account to the subject.
        LinkLost,
        /// `create` loses to another sign-in that creates the account for the subject.
        CreateLostToSameSubject,
        /// `create` loses and nobody holds the subject.
        CreateLost,
    }

    /// The users table as the rules see it, with the same conditional writes:
    /// a link only lands on an account that is not linked yet, and a subject or
    /// username is unique.
    #[derive(Default)]
    pub(crate) struct MemoryDirectory {
        pub rows: Mutex<Vec<Row>>,
        pub writes: Mutex<Vec<String>>,
        pub race: Race,
        /// Fails every read and write, as a lost database connection would.
        pub broken: bool,
    }

    impl MemoryDirectory {
        pub(crate) fn add(&self, username: &str, email: Option<&str>, auth_sub: Option<&str>) -> LocalAccount {
            self.add_row(username, email, auth_sub, true, false)
        }

        pub(crate) fn add_row(
            &self,
            username: &str,
            email: Option<&str>,
            auth_sub: Option<&str>,
            signed_up: bool,
            deleted: bool,
        ) -> LocalAccount {
            let mut rows = self.rows.lock().unwrap();
            let account = LocalAccount {
                user_id: format!("u{}", rows.len() + 1),
                username: username.to_string(),
                email: email.map(str::to_string),
                auth_sub: auth_sub.map(str::to_string),
                password_version: 1,
            };
            rows.push(Row { account: account.clone(), signed_up, deleted });
            account
        }

        pub(crate) fn account(&self, username: &str) -> LocalAccount {
            let rows = self.rows.lock().unwrap();
            rows.iter().find(|row| row.account.username == username).expect("the account exists").account.clone()
        }

        pub(crate) fn linked(&self) -> Vec<String> {
            let rows = self.rows.lock().unwrap();
            rows.iter().filter(|row| row.account.auth_sub.is_some()).map(|row| row.account.username.clone()).collect()
        }

        pub(crate) fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }

        fn check(&self) -> Result<()> {
            if self.broken {
                return Err(anyhow!("connection lost"));
            }
            Ok(())
        }
    }

    impl AccountDirectory for MemoryDirectory {
        async fn find_by_sub(&self, sub: &str) -> Result<Option<LocalAccount>> {
            self.check()?;
            let rows = self.rows.lock().unwrap();
            Ok(rows.iter().find(|row| row.account.auth_sub.as_deref() == Some(sub)).map(|row| row.account.clone()))
        }

        async fn find_by_username(&self, username: &str) -> Result<Option<LocalAccount>> {
            self.check()?;
            let rows = self.rows.lock().unwrap();
            Ok(rows.iter().find(|row| row.account.username == username).map(|row| row.account.clone()))
        }

        async fn find_unlinked_by_email(&self, email: &str, excluded: &[String]) -> Result<Vec<LocalAccount>> {
            self.check()?;
            let rows = self.rows.lock().unwrap();
            Ok(rows
                .iter()
                .filter(|row| {
                    row.account.email.as_deref().is_some_and(|own| own.to_lowercase() == email.to_lowercase())
                        && row.account.auth_sub.is_none()
                        && row.signed_up
                        && !row.deleted
                        && !excluded.contains(&row.account.username)
                })
                .take(2)
                .map(|row| row.account.clone())
                .collect())
        }

        async fn link(&self, user_id: &str, sub: &str) -> Result<Option<LocalAccount>> {
            self.check()?;
            let mut rows = self.rows.lock().unwrap();
            if self.race == Race::LinkLost {
                if let Some(row) = rows.iter_mut().find(|row| row.account.user_id == user_id) {
                    row.account.auth_sub = Some(sub.to_string());
                }
                return Ok(None);
            }
            self.writes.lock().unwrap().push(format!("link {user_id} {sub}"));
            if rows.iter().any(|row| row.account.auth_sub.as_deref() == Some(sub)) {
                return Ok(None);
            }
            match rows.iter_mut().find(|row| row.account.user_id == user_id) {
                Some(row) if row.account.auth_sub.is_none() => {
                    row.account.auth_sub = Some(sub.to_string());
                    Ok(Some(row.account.clone()))
                }
                _ => Ok(None),
            }
        }

        async fn create(&self, username: &str, email: &str, sub: &str) -> Result<Option<LocalAccount>> {
            self.check()?;
            match self.race {
                Race::CreateLostToSameSubject => {
                    self.add(username, None, Some(sub));
                    return Ok(None);
                }
                Race::CreateLost => return Ok(None),
                _ => {}
            }
            self.writes.lock().unwrap().push(format!("create {username}"));
            let taken = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .any(|row| row.account.username == username || row.account.auth_sub.as_deref() == Some(sub));
            if taken {
                return Ok(None);
            }
            Ok(Some(self.add(username, Some(email), Some(sub))))
        }

        async fn update_email(&self, user_id: &str, email: &str) -> Result<()> {
            self.check()?;
            self.writes.lock().unwrap().push(format!("email {user_id} {email}"));
            let mut rows = self.rows.lock().unwrap();
            if let Some(row) = rows.iter_mut().find(|row| row.account.user_id == user_id) {
                row.account.email = Some(email.to_string());
            }
            Ok(())
        }
    }

    pub(crate) fn alice() -> SsoIdentity {
        SsoIdentity {
            sub: "usr_alice".into(),
            username: "alice".into(),
            email: json!("alice@example.com"),
            email_verified: true,
        }
    }

    fn no_links() -> HashMap<String, String> {
        HashMap::new()
    }

    fn link_alice_to_admin() -> HashMap<String, String> {
        HashMap::from([("alice".to_string(), "admin".to_string())])
    }

    async fn resolve(
        identity: &SsoIdentity,
        links: &HashMap<String, String>,
        dir: &MemoryDirectory,
    ) -> SsoAccountResult {
        resolve_sso_account(identity, links, dir).await.expect("the directory answers")
    }

    fn found(result: SsoAccountResult) -> (LocalAccount, SsoAccountOutcome) {
        match result {
            SsoAccountResult::Found { account, outcome } => (account, outcome),
            SsoAccountResult::Refused(reason) => panic!("refused: {reason:?}"),
        }
    }

    #[tokio::test]
    async fn rule_1_signs_in_the_account_already_linked_to_the_subject() {
        let dir = MemoryDirectory::default();
        let linked = dir.add("alice", Some("alice@example.com"), Some("usr_alice"));
        dir.add("other", Some("alice@example.com"), None);
        let result = resolve(&alice(), &no_links(), &dir).await;
        assert_eq!(result, SsoAccountResult::Found { account: linked, outcome: SsoAccountOutcome::Existing });
        assert!(dir.writes().is_empty());
    }

    #[tokio::test]
    async fn rule_2_links_the_account_auth_link_users_names_for_the_auth_username() {
        let dir = MemoryDirectory::default();
        let admin = dir.add("admin", Some("admin@example.test"), None);
        let (account, outcome) = found(resolve(&alice(), &link_alice_to_admin(), &dir).await);
        assert_eq!(account.user_id, admin.user_id);
        assert_eq!(outcome, SsoAccountOutcome::Linked);
        assert_eq!(dir.account("admin").auth_sub.as_deref(), Some("usr_alice"));
        // The reserved account keeps the address its boot seeds.
        assert_eq!(dir.account("admin").email.as_deref(), Some("admin@example.test"));
    }

    #[tokio::test]
    async fn rule_2_skips_a_named_account_that_is_linked_already_and_goes_on_to_the_next_rule() {
        let dir = MemoryDirectory::default();
        dir.add("admin", None, Some("usr_someone"));
        let bob = dir.add("bob", Some("ALICE@example.com"), None);
        let (account, _) = found(resolve(&alice(), &link_alice_to_admin(), &dir).await);
        assert_eq!(account.user_id, bob.user_id);
    }

    #[tokio::test]
    async fn rule_3_links_the_one_signed_up_account_with_the_verified_address_ignoring_case() {
        let dir = MemoryDirectory::default();
        let bob = dir.add("bob", Some("Alice@Example.com"), None);
        let result = resolve(&alice(), &no_links(), &dir).await;
        assert_eq!(
            result,
            SsoAccountResult::Found {
                account: LocalAccount {
                    email: Some("alice@example.com".into()),
                    auth_sub: Some("usr_alice".into()),
                    ..bob.clone()
                },
                outcome: SsoAccountOutcome::Linked,
            }
        );
        assert_eq!(
            dir.writes(),
            [format!("link {} usr_alice", bob.user_id), format!("email {} alice@example.com", bob.user_id)]
        );
    }

    #[tokio::test]
    async fn rule_3_links_by_an_address_the_provider_has_not_verified_keeping_the_accounts_own() {
        let dir = MemoryDirectory::default();
        let bob = dir.add("bob", Some("Alice@Example.com"), None);
        let result = resolve(&SsoIdentity { email_verified: false, ..alice() }, &no_links(), &dir).await;
        assert_eq!(
            result,
            SsoAccountResult::Found {
                account: LocalAccount {
                    user_id: bob.user_id.clone(),
                    username: "bob".into(),
                    email: Some("Alice@Example.com".into()),
                    auth_sub: Some("usr_alice".into()),
                    password_version: 1,
                },
                outcome: SsoAccountOutcome::Linked,
            }
        );
        assert_eq!(dir.writes(), [format!("link {} usr_alice", bob.user_id)]);
    }

    #[tokio::test]
    async fn rule_3_does_not_guess_between_two_accounts_with_the_address_verified_or_not() {
        for email_verified in [true, false] {
            let dir = MemoryDirectory::default();
            dir.add("bob", Some("alice@example.com"), None);
            dir.add("carol", Some("ALICE@example.com"), None);
            let (_, outcome) = found(resolve(&SsoIdentity { email_verified, ..alice() }, &no_links(), &dir).await);
            assert_eq!(outcome, SsoAccountOutcome::Created);
            assert_eq!(dir.linked(), ["alice"]);
        }
    }

    #[tokio::test]
    async fn rule_3_never_links_a_reserved_unfinished_or_deleted_account_verified_or_not() {
        // (username, signed up, deleted)
        let candidates =
            [("admin", true, false), ("admin-ro", true, false), ("user_1234abcd", false, false), ("dave", true, true)];
        for email_verified in [true, false] {
            for (username, signed_up, deleted) in candidates {
                let dir = MemoryDirectory::default();
                dir.add_row(username, Some("alice@example.com"), None, signed_up, deleted);
                let (_, outcome) = found(resolve(&SsoIdentity { email_verified, ..alice() }, &no_links(), &dir).await);
                assert_eq!(outcome, SsoAccountOutcome::Created, "{username}");
                assert_eq!(dir.linked(), ["alice"], "{username}");
            }
        }
    }

    #[tokio::test]
    async fn rule_4_creates_an_account_named_after_the_username_with_the_verified_address() {
        let dir = MemoryDirectory::default();
        let result = resolve(&alice(), &no_links(), &dir).await;
        assert_eq!(
            result,
            SsoAccountResult::Found {
                outcome: SsoAccountOutcome::Created,
                account: LocalAccount {
                    user_id: "u1".into(),
                    username: "alice".into(),
                    email: Some("alice@example.com".into()),
                    auth_sub: Some("usr_alice".into()),
                    password_version: 1,
                },
            }
        );
    }

    #[tokio::test]
    async fn rule_4_gives_a_new_account_the_address_the_provider_has_not_verified() {
        let dir = MemoryDirectory::default();
        let (account, _) = found(resolve(&SsoIdentity { email_verified: false, ..alice() }, &no_links(), &dir).await);
        assert_eq!(account.email.as_deref(), Some("alice@example.com"));
        assert_eq!(dir.account("alice").email.as_deref(), Some("alice@example.com"));
        assert_eq!(dir.writes(), ["create alice"]);
    }

    #[tokio::test]
    async fn rule_4_gives_an_account_without_a_usable_address_its_own_mailbox_as_its_address() {
        let too_long = format!("{}@example.com", "b".repeat(244));
        for email in [json!(null), json!(""), json!("not an address"), json!(too_long), json!(false), json!(0)] {
            let dir = MemoryDirectory::default();
            let identity = SsoIdentity { sub: "usr_bob".into(), username: "bob".into(), email, email_verified: false };
            let (account, _) = found(resolve(&identity, &no_links(), &dir).await);
            assert_eq!(account.email, Some(format!("bob@{}", get_user_domain("bob"))));
        }
        // The longest address the column takes is still an address.
        let longest = format!("{}@example.com", "b".repeat(243));
        let dir = MemoryDirectory::default();
        let identity = SsoIdentity { email: json!(longest), ..alice() };
        assert_eq!(found(resolve(&identity, &no_links(), &dir).await).0.email, Some(longest));
    }

    #[tokio::test]
    async fn rule_4_refuses_a_username_that_cannot_name_a_mailbox() {
        let long = "a".repeat(64);
        for username in ["Alice", "al_ice", "-alice", "alice-", long.as_str(), "admin-ro", ""] {
            let dir = MemoryDirectory::default();
            let result = resolve(&SsoIdentity { username: username.into(), ..alice() }, &no_links(), &dir).await;
            assert_eq!(result, SsoAccountResult::Refused(SsoAccountRefusal::InvalidUsername), "{username}");
            assert!(dir.writes().is_empty());
        }
    }

    #[tokio::test]
    async fn rule_4_refuses_to_take_over_an_account_that_has_the_username_but_is_not_linked() {
        for username in ["alice", "admin"] {
            let dir = MemoryDirectory::default();
            dir.add(username, Some("someone@example.com"), None);
            let result = resolve(&SsoIdentity { username: username.into(), ..alice() }, &no_links(), &dir).await;
            assert_eq!(result, SsoAccountResult::Refused(SsoAccountRefusal::AccountExists));
            assert!(dir.writes().is_empty());
        }
    }

    #[tokio::test]
    async fn takes_the_winner_of_a_race_to_link_the_same_subject() {
        let dir = MemoryDirectory { race: Race::LinkLost, ..Default::default() };
        let bob = dir.add("bob", Some("alice@example.com"), None);
        let (account, _) = found(resolve(&alice(), &no_links(), &dir).await);
        assert_eq!(account.user_id, bob.user_id);
    }

    #[tokio::test]
    async fn takes_the_winner_of_a_race_to_create_the_account_or_refuses_when_another_took_the_name() {
        let won = MemoryDirectory { race: Race::CreateLostToSameSubject, ..Default::default() };
        let (_, outcome) = found(resolve(&alice(), &no_links(), &won).await);
        assert_eq!(outcome, SsoAccountOutcome::Existing);

        let lost = MemoryDirectory { race: Race::CreateLost, ..Default::default() };
        assert_eq!(
            resolve(&alice(), &no_links(), &lost).await,
            SsoAccountResult::Refused(SsoAccountRefusal::AccountExists)
        );
    }

    #[tokio::test]
    async fn copies_a_changed_verified_address_at_each_sign_in_and_nothing_unverified() {
        let dir = MemoryDirectory::default();
        dir.add("alice", Some("old@example.com"), Some("usr_alice"));
        let with_email =
            |email: &str, email_verified: bool| SsoIdentity { email: json!(email), email_verified, ..alice() };

        resolve(&with_email("new@example.com", false), &no_links(), &dir).await;
        assert_eq!(dir.account("alice").email.as_deref(), Some("old@example.com"));
        resolve(&with_email("new@example.com", true), &no_links(), &dir).await;
        assert_eq!(dir.account("alice").email.as_deref(), Some("new@example.com"));
        resolve(&with_email("not an address", true), &no_links(), &dir).await;
        assert_eq!(dir.account("alice").email.as_deref(), Some("new@example.com"));
        // The same address again writes nothing.
        let writes = dir.writes().len();
        resolve(&with_email("new@example.com", true), &no_links(), &dir).await;
        assert_eq!(dir.writes().len(), writes);
    }

    #[tokio::test]
    async fn an_email_claim_that_is_not_text_is_a_fault_as_it_is_for_bun() {
        for email in [json!(42), json!(true), json!(["alice@example.com"]), json!({ "address": "alice@example.com" })] {
            let dir = MemoryDirectory::default();
            let error = resolve_sso_account(&SsoIdentity { email, ..alice() }, &no_links(), &dir).await.unwrap_err();
            assert!(error.to_string().starts_with("email.split is not a function"));
            assert!(dir.writes().is_empty());
        }
    }

    #[tokio::test]
    async fn a_failed_directory_fails_the_sign_in() {
        let dir = MemoryDirectory { broken: true, ..Default::default() };
        let error = resolve_sso_account(&alice(), &no_links(), &dir).await.unwrap_err();
        assert_eq!(error.to_string(), "connection lost");
    }

    #[test]
    fn a_session_user_is_the_account_with_its_credential_generation() {
        let account = LocalAccount {
            user_id: "u1".into(),
            username: "alice".into(),
            email: Some("alice@example.com".into()),
            auth_sub: Some("usr_alice".into()),
            password_version: 3,
        };
        assert_eq!(
            serde_json::to_value(account.to_signed_user()).unwrap(),
            json!({ "id": "u1", "email": "alice@example.com", "username": "alice", "passwordVersion": 3 })
        );
    }
}
