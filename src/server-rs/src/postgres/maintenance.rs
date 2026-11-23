//! Boot maintenance: the statements whose cost grows with the size of
//! `mails` — index builds and the search-vector reindex.
//!
//! They run on a dedicated session with a build-sized budget, each statement
//! independently fallible, serialized across instances by an advisory lock,
//! after the listeners are bound. Nothing here is fatal: the app serves
//! correctly without an index.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::pool::Client;
use super::schema::Statement;
use super::pool;
use crate::{logger, params};

/// Per-statement budget, and the deadline a drain loop measures itself against.
pub const MAINTENANCE_TIMEOUT_MS: u64 = 10 * 60_000;

const CLIENT_READ_TIMEOUT: Duration = Duration::from_millis(MAINTENANCE_TIMEOUT_MS + 30_000);

/// Distinct from the migration lock: the phases must not collide across instances.
const MAINTENANCE_ADVISORY_LOCK_KEY: i64 = 6908003;

pub struct MaintenanceWork {
    /// Index builds. Invalid leftovers are swept before these run.
    pub indexes: Vec<Statement>,
    /// Row-count-scaled statements that build no index.
    pub statements: Vec<Statement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceResult {
    /// The database reflects every statement; the marker may be written.
    Complete,
    /// Something did not land and someone should know.
    Incomplete,
    /// Another instance is doing the work, or shutdown cancelled it.
    Skipped,
}

/// Drops indexes a failed `CREATE INDEX CONCURRENTLY` left behind invalid, so
/// the rebuild is not a silent no-op. Returns the names it could not clear.
async fn drop_invalid_indexes(
    client: &mut Client,
    names: &[String],
    cancel: &CancellationToken,
) -> Result<HashSet<String>, super::DbError> {
    let mut unresolved = HashSet::new();
    let probe = client
        .query_with_timeout(
            "SELECT c.relname
             FROM pg_index i
             JOIN pg_class c ON c.oid = i.indexrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE NOT i.indisvalid
              AND n.nspname = current_schema()
              AND c.relname = ANY($1)",
            &params![names],
            CLIENT_READ_TIMEOUT,
        )
        .await?;
    for row in probe.rows {
        let relname = row.text("relname");
        if cancel.is_cancelled() {
            unresolved.insert(relname);
            continue;
        }
        logger::warn(
            format!("[Maintenance] Dropping invalid leftover index {relname} before rebuild."),
            json!({}),
        );
        // `relname` matched one of the app's own index names, so it is safe to interpolate.
        let drop = format!("DROP INDEX CONCURRENTLY IF EXISTS {relname}");
        if let Err(error) = client.query_with_timeout(&drop, &[], CLIENT_READ_TIMEOUT).await {
            logger::error_with("[Maintenance] Could not drop invalid index.", json!({ "index": relname }), &error);
            unresolved.insert(relname);
        }
    }
    Ok(unresolved)
}

async fn run_locked(
    client: &mut Client,
    work: &MaintenanceWork,
    cancel: &CancellationToken,
) -> Result<MaintenanceResult, super::DbError> {
    // CONCURRENTLY cannot run in a transaction, so the budget is a session GUC.
    client.query(&format!("SET statement_timeout = {MAINTENANCE_TIMEOUT_MS}"), &[]).await?;

    let mut complete = true;
    let names: Vec<String> = work.indexes.iter().map(|s| s.name.clone()).collect();
    let unresolved = match drop_invalid_indexes(client, &names, cancel).await {
        Ok(unresolved) => unresolved,
        Err(error) => {
            logger::error_with("[Maintenance] Invalid-index sweep failed.", json!({}), &error);
            complete = false;
            HashSet::new()
        }
    };
    if !unresolved.is_empty() {
        complete = false;
    }
    if cancel.is_cancelled() {
        return Ok(MaintenanceResult::Skipped);
    }

    for statement in work.indexes.iter().chain(work.statements.iter()) {
        if cancel.is_cancelled() {
            logger::info("[Maintenance] Cancelled — stopping before the remaining statements.", json!({}));
            return Ok(MaintenanceResult::Skipped);
        }
        if unresolved.contains(&statement.name) {
            continue;
        }
        let deadline = Instant::now() + Duration::from_millis(MAINTENANCE_TIMEOUT_MS);
        let mut drained = false;
        loop {
            match client.query_with_timeout(&statement.sql, &[], CLIENT_READ_TIMEOUT).await {
                Ok(result) => {
                    if !statement.drain || result.row_count.unwrap_or(0) == 0 {
                        drained = true;
                        break;
                    }
                }
                Err(error) => {
                    if cancel.is_cancelled() {
                        logger::info("[Maintenance] Cancelled — stopping.", json!({}));
                        return Ok(MaintenanceResult::Skipped);
                    }
                    complete = false;
                    drained = true;
                    logger::error_with(
                        "[Maintenance] Statement failed.",
                        json!({ "statement": statement.name }),
                        &error,
                    );
                    break;
                }
            }
            if cancel.is_cancelled() || Instant::now() >= deadline {
                break;
            }
        }
        if !drained {
            if !cancel.is_cancelled() {
                logger::warn(
                    "[Maintenance] Drain budget exhausted before the backlog cleared.",
                    json!({ "statement": statement.name }),
                );
            }
            complete = false;
        }
    }
    if cancel.is_cancelled() && !complete {
        return Ok(MaintenanceResult::Skipped);
    }
    if complete {
        logger::info("[Maintenance] Complete — schema is at target.", json!({}));
    }
    Ok(if complete { MaintenanceResult::Complete } else { MaintenanceResult::Incomplete })
}

/// Runs every statement, skipping index builds already satisfied. Cancelling
/// stops the phase at the next statement boundary and cancels the backend
/// running the current one. Never fails.
pub async fn run_boot_maintenance(work: &MaintenanceWork, cancel: &CancellationToken) -> MaintenanceResult {
    if cancel.is_cancelled() {
        return MaintenanceResult::Skipped;
    }
    let mut client = match pool().connect().await {
        Ok(client) => client,
        Err(error) => {
            if cancel.is_cancelled() {
                return MaintenanceResult::Skipped;
            }
            logger::error_with("[Maintenance] Could not acquire a connection.", json!({}), &error);
            return MaintenanceResult::Incomplete;
        }
    };

    let locked = match client
        .query("SELECT pg_try_advisory_lock($1) AS locked", &params![MAINTENANCE_ADVISORY_LOCK_KEY])
        .await
    {
        Ok(result) => result.rows.first().is_some_and(|row| row.bool("locked")),
        Err(error) => {
            client.release_destroy();
            if cancel.is_cancelled() {
                return MaintenanceResult::Skipped;
            }
            logger::error_with("[Maintenance] Aborted.", json!({}), &error);
            return MaintenanceResult::Incomplete;
        }
    };
    if !locked {
        logger::info("[Maintenance] Another instance holds the lock — skipping this boot.", json!({}));
        client.release_destroy();
        return MaintenanceResult::Skipped;
    }

    let pid = client
        .query("SELECT pg_backend_pid() AS pid", &[])
        .await
        .ok()
        .and_then(|result| result.rows.first().and_then(|row| row.opt_i64("pid")));

    // The in-flight statement runs on this session, so only another one can stop it.
    let watcher = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            let Some(pid) = pid else { return };
            logger::info("[Maintenance] Cancelling in-flight statement for shutdown.", json!({}));
            if let Err(error) = pool().query("SELECT pg_cancel_backend($1)", &params![pid]).await {
                logger::error_with(
                    "[Maintenance] Could not cancel the in-flight statement.",
                    json!({ "pid": pid }),
                    &error,
                );
            }
        })
    };

    let result = if cancel.is_cancelled() {
        MaintenanceResult::Skipped
    } else {
        match run_locked(&mut client, work, cancel).await {
            Ok(result) => result,
            Err(_) if cancel.is_cancelled() => MaintenanceResult::Skipped,
            Err(error) => {
                logger::error_with("[Maintenance] Aborted.", json!({}), &error);
                MaintenanceResult::Incomplete
            }
        }
    };

    watcher.abort();
    let _ = client
        .query("SELECT pg_advisory_unlock($1)", &params![MAINTENANCE_ADVISORY_LOCK_KEY])
        .await;
    // The session carries a statement_timeout override; discard it rather than return it.
    client.release_destroy();
    result
}
