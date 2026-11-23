//! The schema this server creates and maintains.
//!
//! `schema.json` is generated from the Bun server's table definitions by
//! `scripts/schema-snapshot.ts`, and a unit test there fails when it goes
//! stale. Embedding the statements instead of rewriting them here keeps the
//! two servers on one schema and one pair of `schema_meta` hashes, so either
//! can boot on a database the other created.

use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct TableSpec {
    pub name: String,
    #[serde(rename = "createSql")]
    pub create_sql: String,
    /// `(column, definition)` in declaration order.
    pub columns: Vec<(String, String)>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Statement {
    /// Identifies the statement in logs; for an index, its index name.
    pub name: String,
    pub sql: String,
    /// Re-issue until it reports no rows.
    #[serde(default)]
    pub drain: bool,
}

#[derive(Debug, Deserialize)]
pub struct Schema {
    #[serde(rename = "schemaHash")]
    pub schema_hash: String,
    #[serde(rename = "maintenanceHash")]
    pub maintenance_hash: String,
    pub tables: Vec<TableSpec>,
    #[serde(rename = "searchVectorDdl")]
    pub search_vector_ddl: Vec<String>,
    pub indexes: Vec<Statement>,
    pub statements: Vec<Statement>,
}

pub fn schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::from_str(include_str!("../../schema.json")).expect("schema.json is well formed")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_loads() {
        let schema = schema();
        assert_eq!(schema.schema_hash.len(), 16);
        assert_eq!(schema.tables[0].name, "users");
        assert!(schema.tables.iter().any(|t| t.name == "mails"));
        assert!(schema.statements.iter().any(|s| s.drain));
    }
}
