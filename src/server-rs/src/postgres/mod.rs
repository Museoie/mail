//! The PostgreSQL layer: the pool, the boot-time schema work, and the
//! repositories every other module reads and writes through.

pub mod initialize;
pub mod maintenance;
pub mod migration;
pub mod pool;
pub mod repositories;
pub mod schema;

use std::sync::OnceLock;
use std::time::Duration;

pub use pool::{Client, DbError, DbResult, Param, Pool, PoolOptions, QueryResult, Row};

use crate::env;

/// Matches rows that are not soft-deleted.
pub const SOFT_DELETE_CONDITION: &str = "(is_deleted IS NULL OR is_deleted = FALSE)";

fn options_from_env() -> PoolOptions {
    // A variable that is set but empty stays empty, as a destructuring default would leave it.
    let port = env::var("POSTGRES_PORT").unwrap_or_else(|| "5432".to_string());
    PoolOptions {
        host: env::var("POSTGRES_HOST").unwrap_or_else(|| "localhost".to_string()),
        port: port.trim().parse().unwrap_or(5432),
        user: env::var("POSTGRES_USER").unwrap_or_else(|| "postgres".to_string()),
        password: env::var("POSTGRES_PASSWORD"),
        database: env::var("POSTGRES_DATABASE").unwrap_or_else(|| "mail".to_string()),
        max: 10,
        connection_timeout: Duration::from_millis(2000),
        statement_timeout: Duration::from_millis(30_000),
        query_timeout: Duration::from_millis(30_000),
    }
}

/// The process-wide pool, created from `POSTGRES_*` on first use.
pub fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool::new(options_from_env()))
}
