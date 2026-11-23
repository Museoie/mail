//! Provider events (sso/events.ts).

use std::future::Future;

use anyhow::Result;
use serde_json::{json, Value};

use crate::auth_client::{AuthError, AuthEvent};
use crate::imap::session_registry::evict_imap_sessions;
use crate::logger;
use crate::postgres::repositories::sessions::{delete_sessions_by_auth_sid, delete_sessions_for_user};
use crate::postgres::repositories::users::bump_password_version_by_auth_sub;
use crate::smtp::registry::evict_smtp_connections;
use crate::sso::Sso;

/// A security event token is a few hundred bytes; anything this size is not
/// one. The events route refuses a longer body with 413.
pub const EVENT_BODY_LIMIT: usize = 64 * 1024;

/// What applying an event writes and closes, so the rules run without a
/// database or live connections.
pub trait EventEffects: Sync {
    /// Moves the linked account's credential generation on. `(user_id,
    /// username)`, or `None` when no account is linked to `sub`.
    fn bump_password_version_by_auth_sub(
        &self,
        sub: &str,
    ) -> impl Future<Output = Result<Option<(String, String)>>> + Send;
    fn delete_sessions_for_user(&self, user_id: &str) -> impl Future<Output = Result<u64>> + Send;
    fn delete_sessions_by_auth_sid(&self, sid: &str) -> impl Future<Output = Result<u64>> + Send;
    fn evict_imap_sessions(&self, username: &str) -> usize;
    fn evict_smtp_connections(&self, username: &str) -> usize;
}

/// The database, and the mail connections this process holds.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveEventEffects;

impl EventEffects for LiveEventEffects {
    async fn bump_password_version_by_auth_sub(&self, sub: &str) -> Result<Option<(String, String)>> {
        Ok(bump_password_version_by_auth_sub(sub).await?)
    }

    async fn delete_sessions_for_user(&self, user_id: &str) -> Result<u64> {
        Ok(delete_sessions_for_user(user_id).await?)
    }

    async fn delete_sessions_by_auth_sid(&self, sid: &str) -> Result<u64> {
        Ok(delete_sessions_by_auth_sid(sid).await?)
    }

    fn evict_imap_sessions(&self, username: &str) -> usize {
        evict_imap_sessions(username)
    }

    fn evict_smtp_connections(&self, username: &str) -> usize {
        evict_smtp_connections(username)
    }
}

/// Takes away everything the linked account signed in with. The credential
/// generation moves first: every process checks it, so HTTP sessions and the
/// IMAP and SMTP connections other copies of the server hold fail their next
/// request or command. This copy then deletes the sessions and closes its own
/// connections at once.
async fn revoke_linked_account<E: EventEffects + ?Sized>(effects: &E, sub: &str, cause: &str) -> Result<()> {
    let Some((user_id, username)) = effects.bump_password_version_by_auth_sub(sub).await? else {
        return Ok(());
    };
    let deleted_sessions = effects.delete_sessions_for_user(&user_id).await?;
    let imap_connections = effects.evict_imap_sessions(&username);
    let smtp_connections = effects.evict_smtp_connections(&username);
    logger::info(
        "Revoked a linked account's sessions and connections",
        json!({
            "component": "sso",
            "cause": cause,
            "userId": user_id,
            "deletedSessions": deleted_sessions,
            "imapConnections": imap_connections,
            "smtpConnections": smtp_connections,
        }),
    );
    Ok(())
}

/// Applies one provider event. Delivery is at least once, and applying an
/// event again only ends sessions and connections again, so no record of
/// handled events is kept. A fault is an `Err`, and the provider retries.
pub async fn handle_sso_event(event: &AuthEvent) -> Result<()> {
    handle_sso_event_with(event, &LiveEventEffects).await
}

/// [`handle_sso_event`] with the effects it has given.
pub async fn handle_sso_event_with<E: EventEffects + ?Sized>(event: &AuthEvent, effects: &E) -> Result<()> {
    let sub = event.sub.as_deref().filter(|sub| !sub.is_empty());
    match event.event_type.as_str() {
        "session.ended" => {
            let Some(sid) = event.sid.as_deref().filter(|sid| !sid.is_empty()) else {
                return Ok(());
            };
            let deleted_sessions = effects.delete_sessions_by_auth_sid(sid).await?;
            logger::info(
                "Ended the sessions of a provider session",
                json!({ "component": "sso", "deletedSessions": deleted_sessions }),
            );
        }
        // Any role grants the whole app, so only its removal changes anything.
        "access.changed" => {
            if let (Some(sub), Some(None)) = (sub, &event.role) {
                revoke_linked_account(effects, sub, &event.event_type).await?;
            }
        }
        "user.disabled" | "user.deleted" | "credential.revoked" => {
            if let Some(sub) = sub {
                revoke_linked_account(effects, sub, &event.event_type).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// What the events route answers: a status and a JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct EventAnswer {
    pub status: u16,
    pub body: Value,
}

/// `POST /api/auth/events`, once the route has read the body as text (up to
/// [`EVENT_BODY_LIMIT`] bytes, whatever its content type): verifies the
/// event, applies it and acknowledges it. The body's signature is what
/// authenticates the call. `202` acknowledges; anything else makes the
/// provider deliver again. An `Err` is a fault applying the event, which the
/// route answers as any other fault: 500.
pub async fn receive_sso_event(sso: &Sso, body: &str) -> Result<EventAnswer> {
    receive_sso_event_with(sso, body, &LiveEventEffects).await
}

/// [`receive_sso_event`] with the effects it has given.
pub async fn receive_sso_event_with<E: EventEffects + ?Sized>(
    sso: &Sso,
    body: &str,
    effects: &E,
) -> Result<EventAnswer> {
    let event = match sso.client.read_event(body).await {
        Ok(event) => event,
        Err(AuthError::InvalidToken(reason)) => {
            logger::warn("Refused a provider event", json!({ "component": "sso", "reason": reason }));
            return Ok(EventAnswer { status: 400, body: json!({ "status": "failed", "message": "Invalid event." }) });
        }
        Err(AuthError::Unavailable(reason)) => {
            logger::warn("Could not verify a provider event", json!({ "component": "sso", "reason": reason }));
            return Ok(EventAnswer { status: 503, body: json!({ "status": "error", "message": "Try again later." }) });
        }
        Err(error) => return Err(error.into()),
    };
    logger::info("Provider event", json!({ "component": "sso", "type": event.event_type, "jti": event.jti }));
    handle_sso_event_with(&event, effects).await?;
    Ok(EventAnswer { status: 202, body: json!({ "status": "success" }) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::sync::Mutex;

    /// Records every effect in the order it was asked for.
    #[derive(Default)]
    pub(crate) struct RecordedEffects {
        pub calls: Mutex<Vec<String>>,
        /// The account `bump` finds; `None` for a subject nobody is linked to.
        pub linked: Option<(String, String)>,
        /// Fails the session delete, as a lost database connection would.
        pub fail_delete: bool,
    }

    impl RecordedEffects {
        pub(crate) fn linked_to_alice() -> RecordedEffects {
            RecordedEffects { linked: Some(("u1".into(), "alice".into())), ..Default::default() }
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl EventEffects for RecordedEffects {
        async fn bump_password_version_by_auth_sub(&self, sub: &str) -> Result<Option<(String, String)>> {
            self.record(format!("bump {sub}"));
            Ok(self.linked.clone())
        }

        async fn delete_sessions_for_user(&self, user_id: &str) -> Result<u64> {
            self.record(format!("delete sessions of {user_id}"));
            if self.fail_delete {
                return Err(anyhow!("connection lost"));
            }
            Ok(2)
        }

        async fn delete_sessions_by_auth_sid(&self, sid: &str) -> Result<u64> {
            self.record(format!("delete sessions by sid {sid}"));
            Ok(1)
        }

        fn evict_imap_sessions(&self, username: &str) -> usize {
            self.record(format!("evict imap {username}"));
            1
        }

        fn evict_smtp_connections(&self, username: &str) -> usize {
            self.record(format!("evict smtp {username}"));
            1
        }
    }

    fn event(event_type: &str) -> AuthEvent {
        AuthEvent { event_type: event_type.into(), jti: "evt_1".into(), ..Default::default() }
    }

    /// The fence moves before anything is deleted, so a session row an
    /// in-flight request writes back after the delete still fails it.
    const REVOKED_EVERYWHERE: [&str; 4] =
        ["bump usr_alice", "delete sessions of u1", "evict imap alice", "evict smtp alice"];

    #[tokio::test]
    async fn session_ended_deletes_the_sessions_of_that_provider_session_only() {
        let effects = RecordedEffects::linked_to_alice();
        let ended = AuthEvent { sid: Some("ses_alice".into()), ..event("session.ended") };
        handle_sso_event_with(&ended, &effects).await.unwrap();
        assert_eq!(effects.calls(), ["delete sessions by sid ses_alice"]);
    }

    #[tokio::test]
    async fn session_ended_without_a_session_id_does_nothing() {
        let effects = RecordedEffects::linked_to_alice();
        handle_sso_event_with(&event("session.ended"), &effects).await.unwrap();
        handle_sso_event_with(&AuthEvent { sid: Some(String::new()), ..event("session.ended") }, &effects)
            .await
            .unwrap();
        assert!(effects.calls().is_empty());
    }

    #[tokio::test]
    async fn access_changed_with_no_role_revokes_the_linked_account_everywhere() {
        let effects = RecordedEffects::linked_to_alice();
        let removed = AuthEvent { sub: Some("usr_alice".into()), role: Some(None), ..event("access.changed") };
        handle_sso_event_with(&removed, &effects).await.unwrap();
        assert_eq!(effects.calls(), REVOKED_EVERYWHERE);
    }

    #[tokio::test]
    async fn access_changed_to_another_role_changes_nothing_any_role_grants_the_app() {
        let effects = RecordedEffects::linked_to_alice();
        let changed =
            AuthEvent { sub: Some("usr_alice".into()), role: Some(Some("admin".into())), ..event("access.changed") };
        handle_sso_event_with(&changed, &effects).await.unwrap();
        // Nor does one that does not say what the role became.
        let unsaid = AuthEvent { sub: Some("usr_alice".into()), role: None, ..event("access.changed") };
        handle_sso_event_with(&unsaid, &effects).await.unwrap();
        assert!(effects.calls().is_empty());
    }

    #[tokio::test]
    async fn user_disabled_user_deleted_and_credential_revoked_revoke_the_linked_account_everywhere() {
        for event_type in ["user.disabled", "user.deleted", "credential.revoked"] {
            let effects = RecordedEffects::linked_to_alice();
            let revoking = AuthEvent { sub: Some("usr_alice".into()), id: Some("apw_1".into()), ..event(event_type) };
            handle_sso_event_with(&revoking, &effects).await.unwrap();
            assert_eq!(effects.calls(), REVOKED_EVERYWHERE, "{event_type}");
        }
    }

    #[tokio::test]
    async fn does_nothing_more_for_a_subject_no_account_is_linked_to() {
        let effects = RecordedEffects::default();
        let disabled = AuthEvent { sub: Some("usr_nobody".into()), ..event("user.disabled") };
        handle_sso_event_with(&disabled, &effects).await.unwrap();
        assert_eq!(effects.calls(), ["bump usr_nobody"]);
    }

    #[tokio::test]
    async fn does_nothing_without_a_subject() {
        let effects = RecordedEffects::linked_to_alice();
        handle_sso_event_with(&event("user.disabled"), &effects).await.unwrap();
        handle_sso_event_with(&AuthEvent { sub: Some(String::new()), ..event("user.deleted") }, &effects)
            .await
            .unwrap();
        handle_sso_event_with(&AuthEvent { role: Some(None), ..event("access.changed") }, &effects).await.unwrap();
        assert!(effects.calls().is_empty());
    }

    #[tokio::test]
    async fn acknowledges_and_ignores_other_events() {
        let effects = RecordedEffects::linked_to_alice();
        for event_type in ["invite.accepted", "something.new"] {
            let other = AuthEvent { sub: Some("usr_alice".into()), invite: Some("inv_1".into()), ..event(event_type) };
            handle_sso_event_with(&other, &effects).await.unwrap();
        }
        assert!(effects.calls().is_empty());
    }

    #[tokio::test]
    async fn fails_when_a_write_fails_so_the_provider_delivers_the_event_again() {
        let effects = RecordedEffects { fail_delete: true, ..RecordedEffects::linked_to_alice() };
        let disabled = AuthEvent { sub: Some("usr_alice".into()), ..event("user.disabled") };
        let error = handle_sso_event_with(&disabled, &effects).await.unwrap_err();
        assert_eq!(error.to_string(), "connection lost");
        // Nothing is closed for a revocation that did not land.
        assert_eq!(effects.calls(), ["bump usr_alice", "delete sessions of u1"]);
    }
}
