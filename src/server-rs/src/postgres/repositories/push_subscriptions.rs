//! The `push_subscriptions` table. Every function absorbs a database fault
//! into its "nothing happened" result, as the Bun repository does.

use chrono::{DateTime, Utc};
use serde_json::json;

use crate::models::SignedUser;
use crate::postgres::{pool, DbError, Param};
use crate::push_validation::PushSubscription;
use crate::{js, logger, params};

/// A stored subscription joined with the user it belongs to.
#[derive(Debug, Clone)]
pub struct ComputedPushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub push_subscription_id: String,
    pub username: String,
    pub last_notified: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
}

fn absorbed<T>(what: &str, fallback: T, result: Result<T, DbError>) -> T {
    result.unwrap_or_else(|error| {
        logger::error_with(what, json!({}), &error);
        fallback
    })
}

/// Stores a subscription and returns its new id.
pub async fn store_subscription(user_id: &str, subscription: &PushSubscription) -> Option<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let result = pool()
        .query(
            "INSERT INTO push_subscriptions (updated, push_subscription_id, user_id, endpoint, keys_p256dh, keys_auth, last_notified) VALUES (CURRENT_TIMESTAMP, $1, $2, $3, $4, $5, $6) RETURNING push_subscription_id",
            &params![&id, user_id, &subscription.endpoint, &subscription.p256dh, &subscription.auth, js::now_iso()],
        )
        .await
        .map(|result| result.rows.first().map(|row| row.text("push_subscription_id")));
    absorbed("Failed to store push subscription", None, result)
}

pub async fn delete_subscription(push_subscription_id: &str) -> bool {
    let result = pool()
        .query(
            "DELETE FROM push_subscriptions WHERE push_subscription_id = $1 RETURNING push_subscription_id",
            &params![push_subscription_id],
        )
        .await
        .map(|result| result.any());
    absorbed("Failed to delete push subscription", false, result)
}

/// Deletes the subscription only when it belongs to `user_id`.
pub async fn delete_subscription_for_user(push_subscription_id: &str, user_id: &str) -> bool {
    let result = pool()
        .query(
            "DELETE FROM push_subscriptions WHERE push_subscription_id = $1 AND user_id = $2 RETURNING push_subscription_id",
            &params![push_subscription_id, user_id],
        )
        .await
        .map(|result| result.any());
    absorbed("Failed to delete push subscription", false, result)
}

/// Deletes subscriptions not refreshed for a week and returns how many.
pub async fn clean_subscriptions() -> u64 {
    let cutoff = js::iso_from_ms(js::now_ms() - 7 * 24 * 60 * 60 * 1000);
    let result = pool()
        .query(
            "DELETE FROM push_subscriptions\n      WHERE updated < $1\n      RETURNING push_subscription_id",
            &params![cutoff],
        )
        .await
        .map(|result| result.row_count.unwrap_or(0));
    absorbed("Failed to clean push subscriptions", 0, result)
}

pub async fn get_subscriptions(users: &[SignedUser]) -> Vec<ComputedPushSubscription> {
    if users.is_empty() {
        return Vec::new();
    }
    let placeholders: Vec<String> = (1..=users.len()).map(|i| format!("${i}")).collect();
    let sql = format!("SELECT * FROM push_subscriptions WHERE user_id IN ({})", placeholders.join(", "));
    let values: Vec<Param> = users.iter().map(|user| Param::from(&user.id)).collect();
    let result = pool().query(&sql, &values).await.map(|result| {
        result
            .rows
            .iter()
            .filter_map(|row| {
                let user = users.iter().find(|user| user.id == row.text("user_id"))?;
                Some(ComputedPushSubscription {
                    endpoint: row.text("endpoint"),
                    p256dh: row.text("keys_p256dh"),
                    auth: row.text("keys_auth"),
                    push_subscription_id: row.text("push_subscription_id"),
                    username: user.username.clone(),
                    last_notified: row.opt_time("last_notified"),
                    updated: row.opt_time("updated"),
                })
            })
            .collect()
    });
    absorbed("Failed to get push subscriptions", Vec::new(), result)
}

/// Marks the caller's subscription as still in use: moves `updated`, the
/// column the weekly sweep reads, to now. `false` for an unknown id or one
/// that belongs to someone else.
pub async fn refresh_subscription(push_subscription_id: &str, user_id: &str) -> bool {
    let result = pool()
        .query(
            "UPDATE push_subscriptions SET updated = CURRENT_TIMESTAMP WHERE push_subscription_id = $1 AND user_id = $2 RETURNING push_subscription_id",
            &params![push_subscription_id, user_id],
        )
        .await
        .map(|result| result.any());
    absorbed("Failed to refresh push subscription", false, result)
}

pub async fn update_last_notified(push_subscription_id: &str) -> bool {
    let result = pool()
        .query(
            "UPDATE push_subscriptions SET updated = CURRENT_TIMESTAMP, last_notified = $1 WHERE push_subscription_id = $2 RETURNING push_subscription_id",
            &params![js::now_iso(), push_subscription_id],
        )
        .await
        .map(|result| !result.rows.is_empty());
    absorbed("Failed to update last notified", false, result)
}
