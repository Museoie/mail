//! Automatic schema migration: compares the schema definitions with the
//! columns the database actually has and adds the missing ones at startup.

use std::collections::HashMap;

use regex::Regex;
use serde_json::json;

use super::pool::{Client, DbResult};
use super::schema::TableSpec;
use super::{pool, DbError};
use crate::{logger, params};

/// Serializes migrations across instances starting at once.
const MIGRATION_ADVISORY_LOCK_KEY: i64 = 6908002;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

/// The normalized type a schema column definition declares.
pub fn parse_column_type(definition: &str) -> Option<String> {
    let upper = definition.trim().to_uppercase();
    let parts: Vec<&str> = upper.split_whitespace().collect();
    let raw = *parts.first()?;
    let two_words = if parts.len() > 1 { format!("{} {}", parts[0], parts[1]) } else { String::new() };
    let normalized = if raw.starts_with("VARCHAR") {
        "VARCHAR"
    } else if raw.starts_with("CHAR") && raw != "CHAR" {
        "CHAR"
    } else if raw == "TIMESTAMPTZ" || raw == "TIMESTAMP" {
        "TIMESTAMP"
    } else if raw == "INTEGER" || raw == "INT" || raw == "SERIAL" {
        "INTEGER"
    } else if raw == "BIGINT" || raw == "BIGSERIAL" {
        "BIGINT"
    } else if raw == "DECIMAL" || raw == "NUMERIC" {
        "NUMERIC"
    } else if matches!(raw, "REAL" | "FLOAT" | "FLOAT4" | "FLOAT8") || two_words == "DOUBLE PRECISION" {
        "FLOAT"
    } else {
        raw
    };
    Some(normalized.to_string())
}

/// The normalized type of an existing column, from `information_schema`.
fn normalize_db_type(data_type: &str, udt_name: &str) -> String {
    let ty = data_type.to_uppercase();
    let udt = udt_name.to_uppercase();
    if ty == "USER-DEFINED" {
        return udt;
    }
    if ty.contains("TIMESTAMP") {
        return "TIMESTAMP".into();
    }
    if ty.contains("CHARACTER VARYING") {
        return "VARCHAR".into();
    }
    if ty.contains("CHARACTER") {
        return "CHAR".into();
    }
    match ty.as_str() {
        "TEXT" | "INTEGER" | "BIGINT" | "SMALLINT" | "BOOLEAN" | "UUID" => return ty,
        "NUMERIC" | "DECIMAL" => return "NUMERIC".into(),
        "REAL" | "DOUBLE PRECISION" => return "FLOAT".into(),
        _ => {}
    }
    if ty == "JSONB" || udt == "JSONB" {
        return "JSONB".into();
    }
    if ty == "JSON" || udt == "JSON" {
        return "JSON".into();
    }
    if ty == "ARRAY" {
        let element = udt_name.strip_prefix('_').unwrap_or(udt_name);
        return format!("{}[]", normalize_db_type(element, udt_name));
    }
    ty
}

fn types_compatible(schema_type: &str, db_type: &str) -> bool {
    let either = |a: &str, b: &str| {
        (schema_type == a || schema_type == b) && (db_type == a || db_type == b)
    };
    schema_type == db_type
        || either("VARCHAR", "TEXT")
        || either("JSON", "JSONB")
        || either("INTEGER", "INT4")
        || either("FLOAT", "FLOAT8")
}

/// The `ALTER TABLE … ADD COLUMN` for a column the table lacks. A NOT NULL
/// column without a default gets one inferred from its type, since existing
/// rows need a value.
pub fn build_add_column_sql(table: &str, column: &str, definition: &str) -> String {
    let mut clean = re(r"(?i)PRIMARY\s+KEY").replace_all(definition, "").into_owned();
    let not_null = re(r"(?i)NOT\s+NULL");
    if not_null.is_match(definition) && !re(r"(?i)DEFAULT\s+").is_match(definition) {
        let ty = definition.split_whitespace().next().unwrap_or("").to_uppercase();
        let default = match ty.as_str() {
            "BOOLEAN" => "FALSE",
            "INTEGER" | "BIGINT" | "SMALLINT" => "0",
            "UUID" => "gen_random_uuid()",
            "JSONB" => "'{}'::jsonb",
            "JSON" => "'{}'::json",
            _ if ty.contains("TIMESTAMP") => "CURRENT_TIMESTAMP",
            _ => "''",
        };
        clean = not_null
            .replace(&clean, format!("DEFAULT {default} NOT NULL").as_str())
            .into_owned();
    }
    format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {column} {}", clean.trim())
}

#[derive(Default)]
struct MigrationResult {
    added: Vec<String>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

async fn migrate_table(client: &mut Client, table: &TableSpec) -> DbResult<MigrationResult> {
    let mut result = MigrationResult::default();
    let existing = client
        .query(
            "SELECT column_name, data_type, udt_name, is_nullable, column_default
    FROM information_schema.columns
    WHERE table_name = $1 AND table_schema = 'public'
    ORDER BY ordinal_position",
            &params![&table.name],
        )
        .await?;
    // No columns means the table does not exist yet; CREATE TABLE handles it.
    if existing.rows.is_empty() {
        return Ok(result);
    }
    let existing: HashMap<String, (String, String)> = existing
        .rows
        .iter()
        .map(|row| (row.text("column_name"), (row.text("data_type"), row.text("udt_name"))))
        .collect();

    for (column, definition) in &table.columns {
        match existing.get(column) {
            None => {
                let sql = build_add_column_sql(&table.name, column, definition);
                match client.query(&sql, &[]).await {
                    Ok(_) => {
                        logger::info(format!("[Migration] Added column {}.{column}", table.name), json!({}));
                        result.added.push(column.clone());
                    }
                    Err(error) => result.errors.push(format!("Failed to add {column}: {error}")),
                }
            }
            Some((data_type, udt_name)) => {
                if let Some(schema_type) = parse_column_type(definition) {
                    let db_type = normalize_db_type(data_type, udt_name);
                    if !types_compatible(&schema_type, &db_type) {
                        result.errors.push(format!(
                            "Type mismatch for {column}: schema expects {schema_type}, database has {db_type}"
                        ));
                    }
                }
            }
        }
    }
    for column in existing.keys() {
        if !table.columns.iter().any(|(name, _)| name == column) {
            result.warnings.push(format!("Extra column in database: {column} (not in schema)"));
        }
    }
    Ok(result)
}

async fn migrate_all(client: &mut Client, tables: &[TableSpec]) -> DbResult<()> {
    client.query("SELECT pg_advisory_lock($1)", &params![MIGRATION_ADVISORY_LOCK_KEY]).await?;
    client.query("BEGIN", &[]).await?;

    let mut fatal = Vec::new();
    let mut added = 0;
    for table in tables {
        let result = migrate_table(client, table).await?;
        added += result.added.len();
        for warning in &result.warnings {
            logger::warn(format!("[Migration] {}: {warning}", table.name), json!({}));
        }
        fatal.extend(result.errors.iter().map(|e| format!("{}: {e}", table.name)));
    }
    if added > 0 {
        logger::info(format!("[Migration] Added {added} column(s)"), json!({}));
    }
    if !fatal.is_empty() {
        let message = format!("Schema migration failed:\n{}", fatal.join("\n"));
        logger::error(format!("[Migration] {message}"), json!({}));
        return Err(DbError::new(message));
    }
    client.query("COMMIT", &[]).await?;
    Ok(())
}

/// Brings every table in line with its definition, in one transaction under
/// an advisory lock. A type mismatch is fatal.
pub async fn run_migrations(tables: &[TableSpec]) -> DbResult<()> {
    logger::info("[Migration] Starting schema migration check...", json!({}));
    let mut client = pool().connect().await?;
    let outcome = migrate_all(&mut client, tables).await;
    if outcome.is_err() {
        let _ = client.query("ROLLBACK", &[]).await;
    }
    let _ = client
        .query("SELECT pg_advisory_unlock($1)", &params![MIGRATION_ADVISORY_LOCK_KEY])
        .await;
    if outcome.is_ok() {
        logger::info("[Migration] Schema migration check complete.", json!({}));
    }
    outcome
}

/// Which half of the boot DDL a marker in `schema_meta` vouches for.
#[derive(Clone, Copy)]
pub enum MarkerKey {
    Schema,
    Maintenance,
}

impl MarkerKey {
    fn name(self) -> &'static str {
        match self {
            MarkerKey::Schema => "schema_hash",
            MarkerKey::Maintenance => "maintenance_hash",
        }
    }
}

/// Whether the marker row exists and equals `expected_hash`. Any failure to
/// read it is "no": the caller falls back to the full, idempotent path.
pub async fn check_schema_at_target(expected_hash: &str, key: MarkerKey) -> bool {
    let key = key.name();
    match pool().query("SELECT value FROM schema_meta WHERE key = $1", &params![key]).await {
        Ok(result) => match result.rows.first() {
            None => {
                logger::info(format!("[Fast-path] {key} marker not present — falling back to slow path"), json!({}));
                false
            }
            Some(row) if row.text("value") != expected_hash => {
                logger::info(
                    format!(
                        "[Fast-path] {key} mismatch (DB={}, expected={expected_hash}) — falling back to slow path",
                        row.text("value")
                    ),
                    json!({}),
                );
                false
            }
            Some(_) => true,
        },
        Err(error) => {
            logger::info(
                format!("[Fast-path] schema_meta read failed ({error}) — falling back to slow path"),
                json!({}),
            );
            false
        }
    }
}

/// Records that the database reflects the DDL `hash` was computed from.
pub async fn write_schema_marker(hash: &str, key: MarkerKey) -> DbResult<()> {
    pool()
        .query(
            "CREATE TABLE IF NOT EXISTS schema_meta (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL
    )",
            &[],
        )
        .await?;
    pool()
        .query(
            "INSERT INTO schema_meta (key, value) VALUES ($1, $2)
     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            &params![key.name(), hash],
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_types() {
        assert_eq!(parse_column_type("VARCHAR(255) UNIQUE NOT NULL").as_deref(), Some("VARCHAR"));
        assert_eq!(parse_column_type("TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP").as_deref(), Some("TIMESTAMP"));
        assert_eq!(parse_column_type("BIGINT").as_deref(), Some("BIGINT"));
        assert!(types_compatible("VARCHAR", "TEXT"));
        assert!(types_compatible("JSONB", "JSONB"));
        assert!(!types_compatible("INTEGER", "BIGINT"));
        assert_eq!(normalize_db_type("timestamp with time zone", "timestamptz"), "TIMESTAMP");
        assert_eq!(normalize_db_type("USER-DEFINED", "tsvector"), "TSVECTOR");
        assert_eq!(normalize_db_type("character varying", "varchar"), "VARCHAR");
    }

    #[test]
    fn add_column_statements() {
        assert_eq!(
            build_add_column_sql("mails", "is_spam", "BOOLEAN NOT NULL DEFAULT FALSE"),
            "ALTER TABLE mails ADD COLUMN IF NOT EXISTS is_spam BOOLEAN NOT NULL DEFAULT FALSE"
        );
        assert_eq!(
            build_add_column_sql("t", "n", "INTEGER NOT NULL"),
            "ALTER TABLE t ADD COLUMN IF NOT EXISTS n INTEGER DEFAULT 0 NOT NULL"
        );
        assert_eq!(
            build_add_column_sql("t", "id", "UUID PRIMARY KEY DEFAULT gen_random_uuid()"),
            "ALTER TABLE t ADD COLUMN IF NOT EXISTS id UUID  DEFAULT gen_random_uuid()"
        );
    }
}
