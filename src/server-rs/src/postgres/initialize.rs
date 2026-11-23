//! Boot-time database work: wait for PostgreSQL, bring the schema to the
//! state this code expects, and reconcile the built-in accounts.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::maintenance::{run_boot_maintenance, MaintenanceResult, MaintenanceWork};
use super::migration::{check_schema_at_target, run_migrations, write_schema_marker, MarkerKey};
use super::repositories::sessions::{delete_sessions_authenticated_as, delete_sessions_for_user};
use super::repositories::users::{find_user_by_username, search_user, write_user, UserFilter, WriteUser};
use super::schema::schema;
use super::pool;
use crate::alarm::send_alarm;
use crate::read_only::ADMIN_RO_USERNAME;
use crate::util::with_timeout;
use crate::verify_password::verify_password;
use crate::{env, logger};

/// The app version the login route reports.
pub const VERSION: &str = "1";

/// Ceiling on alarm delivery, so a wedged webhook cannot delay a stop.
const MAINTENANCE_ALARM_TIMEOUT_MS: u64 = 5_000;

pub async fn postgres_is_available() -> Result<()> {
    const MAX_RETRIES: u32 = 30;
    for attempt in 1..=MAX_RETRIES {
        match pool().connect().await {
            Ok(_) => {
                logger::info("PostgreSQL connection established.", json!({}));
                return Ok(());
            }
            Err(error) => {
                logger::info(
                    format!("PostgreSQL connection attempt {attempt}/{MAX_RETRIES} failed: {error}"),
                    json!({}),
                );
                if attempt == MAX_RETRIES {
                    bail!("Failed to connect to PostgreSQL after maximum retries");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    unreachable!("the loop returns or bails")
}

async fn apply_schema() -> Result<()> {
    let schema = schema();
    for table in &schema.tables {
        pool().query(&table.create_sql, &[]).await?;
    }
    // Before any index: new columns must exist before an index names them.
    run_migrations(&schema.tables).await?;
    for sql in &schema.search_vector_ddl {
        pool().query(sql, &[]).await?;
    }
    // Written only after every statement above succeeded.
    write_schema_marker(&schema.schema_hash, MarkerKey::Schema).await?;
    Ok(())
}

/// Creates the tables, adds missing columns and installs the search trigger,
/// unless the database already carries this code's schema hash.
pub async fn initialize_postgres() -> Result<()> {
    logger::info("PostgreSQL initialization started.", json!({}));
    postgres_is_available().await?;

    if check_schema_at_target(&schema().schema_hash, MarkerKey::Schema).await {
        logger::info("[Fast-path] Schema already at target — skipping DDL.", json!({}));
    } else if let Err(error) = apply_schema().await {
        logger::error_with("Failed to create tables", json!({}), &error);
        bail!("Failed to setup PostgreSQL tables.");
    }

    logger::info("Database tables created/verified successfully.", json!({}));
    Ok(())
}

/// The row-count-scaled half of the boot DDL: index builds and the
/// search-vector reindex. Not fatal and not awaited by the listeners; it
/// carries its own marker so a steady-state boot skips it entirely.
pub async fn boot_maintenance(cancel: CancellationToken) {
    let schema = schema();
    if check_schema_at_target(&schema.maintenance_hash, MarkerKey::Maintenance).await {
        logger::info("[Maintenance] Already at target — skipping.", json!({}));
        return;
    }
    let work = MaintenanceWork { indexes: schema.indexes.clone(), statements: schema.statements.clone() };
    match run_boot_maintenance(&work, &cancel).await {
        MaintenanceResult::Complete => {
            if let Err(error) = write_schema_marker(&schema.maintenance_hash, MarkerKey::Maintenance).await {
                logger::error_with("[Maintenance] Phase failed unexpectedly.", json!({}), &error);
            }
        }
        // Another instance is doing the work, or shutdown cancelled it.
        MaintenanceResult::Skipped => {}
        MaintenanceResult::Incomplete => {
            let message = "Boot maintenance did not complete — an index or the search-vector reindex \
                           is outstanding. The maintenance marker is withheld, so the next boot retries.";
            logger::warn(format!("[Maintenance] {message}"), json!({}));
            with_timeout(
                send_alarm("Boot Maintenance Incomplete", message, None),
                MAINTENANCE_ALARM_TIMEOUT_MS,
            )
            .await;
        }
    }
}

/// Published in `.env.example`, so it is a credential only until an operator
/// supplies their own.
const DEFAULT_ADMIN_PASSWORD: &str = "inbox";

#[derive(Debug, PartialEq)]
pub enum AdminPasswordPlan {
    /// No admin row yet: this boot creates one with this password.
    Seed { password: String, using_default: bool },
    /// The environment asked for the stored password to be replaced.
    Reset { password: String },
    /// The stored password survives the boot untouched.
    Keep { reset_without_password: bool, unrecognized_reset: bool },
}

/// Decides what a boot may do to admin's password. The stored password is the
/// live credential, so the environment seeds it and otherwise only touches it
/// when `ADMIN_PASSWORD_RESET=1` asks.
pub fn plan_admin_password(
    admin_exists: bool,
    admin_password: Option<&str>,
    admin_password_reset: Option<&str>,
) -> AdminPasswordPlan {
    let password = admin_password.filter(|p| !p.is_empty());
    if !admin_exists {
        return AdminPasswordPlan::Seed {
            password: password.unwrap_or(DEFAULT_ADMIN_PASSWORD).to_string(),
            using_default: password.is_none() || password == Some(DEFAULT_ADMIN_PASSWORD),
        };
    }
    if admin_password_reset != Some("1") {
        return AdminPasswordPlan::Keep {
            reset_without_password: false,
            unrecognized_reset: admin_password_reset.is_some_and(|v| !v.is_empty()),
        };
    }
    match password {
        // Resetting to the published default is the outcome nobody asks for.
        None => AdminPasswordPlan::Keep { reset_without_password: true, unrecognized_reset: false },
        Some(password) => AdminPasswordPlan::Reset { password: password.to_string() },
    }
}

fn log_admin_password_plan(plan: &AdminPasswordPlan, admin_password: Option<&str>) {
    match plan {
        AdminPasswordPlan::Seed { using_default: true, .. } => logger::warn(
            format!(
                "[CONFIG WARNING] The admin account was created with '{DEFAULT_ADMIN_PASSWORD}', the value\n  published in .env.example — anyone who can reach this server can sign in as admin.\n  Set ADMIN_PASSWORD in your .env file to something else, then boot once with\n  ADMIN_PASSWORD_RESET=1 to apply it."
            ),
            json!({}),
        ),
        AdminPasswordPlan::Reset { .. } => logger::warn(
            "[CONFIG WARNING] ADMIN_PASSWORD_RESET=1 replaced the stored admin password with ADMIN_PASSWORD.\n  Remove ADMIN_PASSWORD_RESET from the environment: while it is set, every restart applies\n  ADMIN_PASSWORD again and discards whatever password the admin set afterwards.",
            json!({}),
        ),
        AdminPasswordPlan::Keep { reset_without_password: true, .. } => logger::warn(
            "[CONFIG WARNING] ADMIN_PASSWORD_RESET=1 is set but ADMIN_PASSWORD is empty.\n  The stored admin password is unchanged — set ADMIN_PASSWORD and restart to apply the reset.",
            json!({}),
        ),
        AdminPasswordPlan::Keep { unrecognized_reset: true, .. } => logger::warn(
            "[CONFIG WARNING] ADMIN_PASSWORD_RESET is set to a value other than '1', so it was ignored\n  and the stored admin password is unchanged. Only the exact string '1' requests a reset.",
            json!({}),
        ),
        _ => {
            if admin_password.is_some_and(|p| !p.is_empty()) {
                logger::info(
                    "Admin already exists, so ADMIN_PASSWORD was not applied. Boot once with ADMIN_PASSWORD_RESET=1 to replace the stored password with it.",
                    json!({}),
                );
            }
        }
    }
}

/// Creates the `admin` account on the first boot, and applies an explicit
/// password reset on a later one.
pub async fn initialize_admin_user() -> Result<()> {
    let admin_password = env::var("ADMIN_PASSWORD");
    let admin_password_reset = env::var("ADMIN_PASSWORD_RESET");

    let existing = search_user(&UserFilter { username: Some("admin".into()), ..Default::default() }).await?;
    let plan = plan_admin_password(
        existing.is_some(),
        admin_password.as_deref(),
        admin_password_reset.as_deref(),
    );

    let password = match &plan {
        AdminPasswordPlan::Keep { .. } => None,
        AdminPasswordPlan::Seed { password, .. } | AdminPasswordPlan::Reset { password } => Some(password.clone()),
    };
    // A reset replaces the hash, so the credential generation moves in the
    // same write. A keep or seed boot must not bump it, or every redeploy
    // would log admin out.
    let password_version = match &plan {
        AdminPasswordPlan::Reset { .. } => {
            Some(existing.as_ref().and_then(|u| u.password_version).unwrap_or(1) + 1)
        }
        _ => None,
    };
    let domain = env::non_empty("EMAIL_DOMAIN").unwrap_or_else(|| "localhost".to_string());
    let admin_id = write_user(WriteUser {
        user_id: existing.as_ref().map(|u| u.user_id.clone()),
        username: "admin".into(),
        password,
        password_version,
        email: Some(format!("admin@{domain}")),
    })
    .await?
    .ok_or_else(|| anyhow!("Failed to create admin user"))?;

    if matches!(plan, AdminPasswordPlan::Reset { .. }) {
        let deleted = delete_sessions_for_user(&admin_id).await?;
        logger::info(
            "ADMIN_PASSWORD_RESET applied — deleted the sessions the previous password issued.",
            json!({ "deletedSessions": deleted }),
        );
    }

    logger::info("Successfully initialized PostgreSQL database and setup admin user.", json!({}));
    log_admin_password_plan(&plan, admin_password.as_deref());

    if env::non_empty("EMAIL_DOMAIN").is_none() {
        logger::warn(
            "[CONFIG WARNING] EMAIL_DOMAIN is not set. Defaulting to 'mydomain'.\n  The inbox will appear empty if your emails are addressed to a different domain.\n  Set EMAIL_DOMAIN=yourdomain.com in your .env file to see incoming emails.",
            json!({}),
        );
    }
    Ok(())
}

/// Reconciles the read-only `admin-ro` account with `ADMIN_RO_PASSWORD`: set
/// seeds or re-seeds it (deleting its sessions when the value changed), unset
/// revokes a previously seeded account and the sessions it issued.
pub async fn initialize_admin_read_only_user() -> Result<()> {
    let Some(password) = env::non_empty("ADMIN_RO_PASSWORD") else {
        return revoke_admin_read_only_user().await;
    };

    let existing =
        search_user(&UserFilter { username: Some(ADMIN_RO_USERNAME.into()), ..Default::default() }).await?;
    // Compared before the write, which replaces the hash it is compared against.
    let password_rotated = match &existing {
        Some(user) => !verify_password(Some(&password), user.password.as_deref()).await,
        None => false,
    };

    // Revoked before the write: the stored hash is the only rotation signal,
    // so failing here keeps the next boot's retry honest.
    let deleted_sessions = if password_rotated {
        Some(delete_sessions_authenticated_as(ADMIN_RO_USERNAME).await?)
    } else {
        None
    };

    let domain = env::non_empty("EMAIL_DOMAIN").unwrap_or_else(|| "localhost".to_string());
    write_user(WriteUser {
        user_id: existing.as_ref().map(|u| u.user_id.clone()),
        username: ADMIN_RO_USERNAME.into(),
        password: Some(password),
        password_version: password_rotated
            .then(|| existing.as_ref().and_then(|u| u.password_version).unwrap_or(1) + 1),
        email: Some(format!("{ADMIN_RO_USERNAME}@{domain}")),
    })
    .await?
    .ok_or_else(|| anyhow!("Failed to create read-only admin user"))?;

    if password_rotated {
        logger::info(
            "ADMIN_RO_PASSWORD changed — deleted the sessions the previous password issued.",
            json!({ "deletedSessions": deleted_sessions }),
        );
    }
    logger::info("Successfully initialized read-only admin user.", json!({}));
    Ok(())
}

/// Revokes a previously seeded read-only account: the row stays so a later
/// re-enable reuses its id, but its password becomes the hash of a secret
/// that exists nowhere, and the sessions it issued are deleted.
async fn revoke_admin_read_only_user() -> Result<()> {
    let existing =
        search_user(&UserFilter { username: Some(ADMIN_RO_USERNAME.into()), ..Default::default() }).await?;
    let Some(existing) = existing else {
        logger::debug(
            "[CONFIG] ADMIN_RO_PASSWORD is not set — skipping read-only admin user seed. Set it to enable the read-only role.",
            json!({}),
        );
        return Ok(());
    };

    let secret: [u8; 32] = rand::random();
    write_user(WriteUser {
        user_id: Some(existing.user_id.clone()),
        username: ADMIN_RO_USERNAME.into(),
        password: Some(hex::encode(secret)),
        email: existing.email.clone(),
        password_version: Some(existing.password_version.unwrap_or(1) + 1),
    })
    .await?
    .ok_or_else(|| anyhow!("Failed to revoke read-only admin user"))?;

    let deleted = delete_sessions_authenticated_as(ADMIN_RO_USERNAME).await?;
    logger::info(
        "ADMIN_RO_PASSWORD is not set — revoked the previously seeded read-only admin user.",
        json!({ "deletedSessions": deleted }),
    );
    Ok(())
}

/// Whether an account with this username exists, registered or not.
pub async fn username_exists(username: &str) -> Result<bool> {
    Ok(find_user_by_username(username).await?.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_password_plans() {
        assert_eq!(
            plan_admin_password(false, None, None),
            AdminPasswordPlan::Seed { password: "inbox".into(), using_default: true }
        );
        assert_eq!(
            plan_admin_password(false, Some("hunter2"), None),
            AdminPasswordPlan::Seed { password: "hunter2".into(), using_default: false }
        );
        assert_eq!(
            plan_admin_password(true, Some("hunter2"), None),
            AdminPasswordPlan::Keep { reset_without_password: false, unrecognized_reset: false }
        );
        assert_eq!(
            plan_admin_password(true, Some("hunter2"), Some("true")),
            AdminPasswordPlan::Keep { reset_without_password: false, unrecognized_reset: true }
        );
        assert_eq!(
            plan_admin_password(true, None, Some("1")),
            AdminPasswordPlan::Keep { reset_without_password: true, unrecognized_reset: false }
        );
        assert_eq!(
            plan_admin_password(true, Some("hunter2"), Some("1")),
            AdminPasswordPlan::Reset { password: "hunter2".into() }
        );
    }
}
