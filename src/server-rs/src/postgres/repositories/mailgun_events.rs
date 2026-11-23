//! The `mailgun_events` table.

use serde_json::Value;

use crate::params;
use crate::postgres::{pool, DbError, Param};

/// One delivery-lifecycle event as Mailgun reported it.
pub struct MailgunEvent<'a> {
    pub event_id: &'a str,
    pub event: &'a str,
    pub message_id: Option<&'a str>,
    pub recipient: Option<&'a str>,
    pub severity: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub occurred_at: &'a str,
    pub received_at: &'a str,
    pub raw: &'a Value,
}

/// Inserts the event. Mailgun retries an event id after a failed delivery, so
/// a duplicate is the expected no-op.
pub async fn insert_mailgun_event(event: &MailgunEvent<'_>) -> Result<(), DbError> {
    pool()
        .query(
            "INSERT INTO mailgun_events (updated, event_id, event, message_id, recipient, severity, reason, occurred_at, received_at, raw) VALUES (CURRENT_TIMESTAMP, $1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (event_id) DO NOTHING RETURNING *",
            &params![
                event.event_id,
                event.event,
                event.message_id,
                event.recipient,
                event.severity,
                event.reason,
                event.occurred_at,
                event.received_at,
                Param::json(event.raw)
            ],
        )
        .await?;
    Ok(())
}
