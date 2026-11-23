//! Boot-time database work: wait for PostgreSQL, bring the schema to the
//! state this code expects, and bring the domain rows up to date.

use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::maintenance::{run_boot_maintenance, MaintenanceResult, MaintenanceWork};
use super::migration::{check_schema_at_target, run_migrations, write_schema_marker, MarkerKey};
use super::repositories::domains::{backfill_account_domains, find_domain};
use super::repositories::users::find_user_by_username;
use super::schema::schema;
use super::pool;
use crate::alarm::send_alarm;
use crate::util::{get_domain, with_timeout};
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

/// Brings the domain rows to what this version expects (initialize.ts
/// `initializeDomains`). No account is created here: an operator creates
/// accounts with `bun run user`, or single sign-on does.
///
/// Every boot gives each account that has none its own name under the served
/// zone (idempotent; a taken name stays with its owner), and warns while no
/// account owns the zone itself, whose mail is refused until one does.
pub async fn initialize_domains() -> Result<()> {
    let zone = get_domain().to_lowercase();

    if env::non_empty("EMAIL_DOMAIN").is_none() {
        logger::warn(
            "[CONFIG WARNING] EMAIL_DOMAIN is not set. Defaulting to 'mydomain'.\n  Accounts receive mail at <username>.<EMAIL_DOMAIN>, so set EMAIL_DOMAIN=yourdomain.com\n  in your .env file to receive mail.",
            json!({}),
        );
    }

    let backfilled = backfill_account_domains(&zone).await?;
    if backfilled > 0 {
        logger::info(
            "Gave accounts their own domain under the served zone.",
            json!({ "backfilled": backfilled }),
        );
    }

    if find_domain(&zone).await?.is_none() {
        logger::warn(
            format!(
                "[CONFIG WARNING] No account owns {zone}, so mail to *@{zone} is refused.\n  Give it to an account with: bun run user add-domain <username> {zone}\n  (from the built files, as in the Docker image: bun ./build/cli/user.js add-domain <username> {zone})"
            ),
            json!({}),
        );
    }
    Ok(())
}

/// Whether an account with this username exists, registered or not.
pub async fn username_exists(username: &str) -> Result<bool> {
    Ok(find_user_by_username(username).await?.is_some())
}
