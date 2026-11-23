//! Statement builders and the table wrapper the mail and mailbox
//! repositories write through: the parts of `postgres/database.ts` and
//! `models/base.ts` (`Table`) those repositories use.
//!
//! The builders are pure, so the SQL text and the bound parameters can be
//! compared with what the Bun server issues.
//!
//! ```ignore
//! let rows = mails_table()
//!     .update_where(
//!         &[("mail_id", Filter::any_of(&mail_ids))],
//!         &[("expunged", true.into()), ("updated", SetValue::DbNow), ("modseq", modseq.into())],
//!         &["mail_id"],
//!     )
//!     .await?;
//! ```

use serde_json::{Map, Value};

use crate::postgres::schema::schema;
use crate::postgres::{DbError, DbResult, Param};

/// A statement and the parameters it binds.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedQuery {
    pub sql: String,
    pub values: Vec<Param>,
}

/// One result row, column name to value, in select order.
pub type Record = Map<String, Value>;

/// The rows a statement returned and the count it reported.
#[derive(Debug, Clone, Default)]
pub struct Rows {
    pub rows: Vec<Record>,
    /// Rows returned by a SELECT or affected by an INSERT, UPDATE or DELETE.
    pub row_count: Option<u64>,
}

/// Runs one statement on the pool. Every statement the mail and mailbox
/// repositories issue goes through here.
pub(crate) async fn run(sql: &str, values: &[Param]) -> DbResult<Rows> {
    #[cfg(test)]
    if let Some(scripted) = script::intercept(sql, values) {
        return scripted;
    }
    let result = crate::postgres::pool().query(sql, values).await?;
    Ok(Rows {
        row_count: result.row_count,
        rows: result.rows.into_iter().map(|row| row.into_map()).collect(),
    })
}

pub(crate) async fn run_query(query: &PreparedQuery) -> DbResult<Rows> {
    run(&query.sql, &query.values).await
}

/// A value in an UPDATE's data bag.
#[derive(Debug, Clone, PartialEq)]
pub enum SetValue {
    Value(Param),
    /// Stamped from the database clock (`CURRENT_TIMESTAMP`), binding no
    /// parameter. Keeps every mutation on one timeline: the `?since=` delta
    /// cursor reads the database clock too.
    DbNow,
}

impl<T: Into<Param>> From<T> for SetValue {
    fn from(value: T) -> SetValue {
        SetValue::Value(value.into())
    }
}

/// A comparison operator for [`Filter::Compare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    fn sql(self) -> &'static str {
        match self {
            CompareOp::Eq => "=",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
        }
    }
}

/// One predicate of an UPDATE or DELETE.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// The caller named the column and had no value for it. A mutation never
    /// runs with one of these: dropping the predicate would widen the
    /// statement to every row the remaining ones match.
    Missing,
    /// `column = $n`. A NULL value binds as `= NULL`, which matches nothing.
    Equals(Param),
    /// `column <op> $n`, preceded by `column IS NOT NULL` when `not_null`.
    Compare { op: CompareOp, value: Option<Param>, not_null: bool },
    /// `column IN ($n, …)`, preceded by `column IS NOT NULL` when `not_null`.
    In { values: Option<Vec<Param>>, not_null: bool },
    /// `column IS NULL`.
    IsNull,
    /// `column IS NOT TRUE`: FALSE or NULL, the soft-delete test.
    IsNotTrue,
}

impl Filter {
    /// `column IN (…)` over the given values.
    pub fn any_of<T: Clone + Into<Param>>(values: &[T]) -> Filter {
        Filter::In { values: Some(values.iter().cloned().map(Into::into).collect()), not_null: false }
    }

    pub fn compare(op: CompareOp, value: impl Into<Param>) -> Filter {
        Filter::Compare { op, value: Some(value.into()), not_null: false }
    }

    /// A plain equality on a value that may be absent; `None` is
    /// [`Filter::Missing`], which a mutation refuses.
    pub fn optional<T: Into<Param>>(value: Option<T>) -> Filter {
        match value {
            Some(value) => Filter::Equals(value.into()),
            None => Filter::Missing,
        }
    }
}

macro_rules! filter_from {
    ($($ty:ty),*) => {$(
        impl From<$ty> for Filter {
            fn from(value: $ty) -> Filter { Filter::Equals(Param::from(value)) }
        }
    )*};
}
filter_from!(&str, String, &String, i32, i64, u32, u64, usize, bool, f64, Param);

/// Checks the filters of a mutation, rejecting any bag that would produce a
/// wider statement than the caller asked for: one that names a column with
/// no value, or names nothing at all.
pub fn resolve_mutation_filters(method: &str, filters: &[(&str, Filter)]) -> DbResult<()> {
    let missing: Vec<&str> = filters
        .iter()
        .filter(|(_, filter)| {
            matches!(
                filter,
                Filter::Missing | Filter::Compare { value: None, .. } | Filter::In { values: None, .. }
            )
        })
        .map(|(column, _)| *column)
        .collect();
    if !missing.is_empty() {
        return Err(DbError::new(format!(
            "{method} received undefined for {} — refusing to run a mutation without the intended predicate",
            missing.join(", ")
        )));
    }
    if filters.is_empty() {
        return Err(DbError::new(format!("{method} requires at least one filter")));
    }
    Ok(())
}

/// A WHERE clause from mutation filters, with placeholders numbered from
/// `start_index`. An empty IN list is refused: Postgres rejects `col IN ()`.
pub fn build_filter_clauses(filters: &[(&str, Filter)], start_index: usize) -> DbResult<PreparedQuery> {
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Param> = Vec::new();
    let mut index = start_index;
    for (column, filter) in filters {
        match filter {
            Filter::IsNull => clauses.push(format!("{column} IS NULL")),
            Filter::IsNotTrue => clauses.push(format!("{column} IS NOT TRUE")),
            Filter::In { values: list, not_null } => {
                if *not_null {
                    clauses.push(format!("{column} IS NOT NULL"));
                }
                let list = list.as_deref().unwrap_or(&[]);
                if list.is_empty() {
                    return Err(DbError::new(format!("IN filter for {column} requires a non-empty array")));
                }
                let placeholders: Vec<String> = list
                    .iter()
                    .map(|_| {
                        index += 1;
                        format!("${}", index - 1)
                    })
                    .collect();
                clauses.push(format!("{column} IN ({})", placeholders.join(", ")));
                values.extend(list.iter().cloned());
            }
            Filter::Compare { op, value, not_null } => {
                if *not_null {
                    clauses.push(format!("{column} IS NOT NULL"));
                }
                clauses.push(format!("{column} {} ${index}", op.sql()));
                index += 1;
                values.push(value.clone().unwrap_or(Param::Null));
            }
            Filter::Equals(value) => {
                clauses.push(format!("{column} = ${index}"));
                index += 1;
                values.push(value.clone());
            }
            Filter::Missing => {
                clauses.push(format!("{column} = ${index}"));
                index += 1;
                values.push(Param::Null);
            }
        }
    }
    Ok(PreparedQuery { sql: clauses.join(" AND "), values })
}

/// One predicate of a SELECT.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectFilter {
    /// `column = $n`; a NULL value reads as `column IS NULL`.
    Equals(Param),
    /// `column @> $n::jsonb`: the JSONB column contains this document. This
    /// is how a mail is found by one of its attachments.
    Contains(Value),
}

impl<T: Into<Param>> From<T> for SelectFilter {
    fn from(value: T) -> SelectFilter {
        SelectFilter::Equals(value.into())
    }
}

/// `SELECT <columns> FROM <table> [WHERE …] [LIMIT $n]`.
pub fn build_select_with_filters(
    table: &str,
    columns: Option<&[&str]>,
    filters: &[(&str, SelectFilter)],
    limit: Option<i64>,
) -> PreparedQuery {
    let mut conditions: Vec<String> = Vec::new();
    let mut values: Vec<Param> = Vec::new();
    for (column, filter) in filters {
        match filter {
            SelectFilter::Equals(Param::Null) => conditions.push(format!("{column} IS NULL")),
            SelectFilter::Equals(value) => {
                values.push(value.clone());
                conditions.push(format!("{column} = ${}", values.len()));
            }
            SelectFilter::Contains(document) => {
                values.push(Param::json(document));
                conditions.push(format!("{column} @> ${}::jsonb", values.len()));
            }
        }
    }
    let column_list = match columns {
        Some(columns) => columns.join(", "),
        None => "*".to_string(),
    };
    let mut sql = format!("SELECT {column_list} FROM {table}");
    if !conditions.is_empty() {
        sql.push_str(&format!(" WHERE {}", conditions.join(" AND ")));
    }
    if let Some(limit) = limit {
        values.push(Param::from(limit));
        sql.push_str(&format!(" LIMIT ${}", values.len()));
    }
    PreparedQuery { sql, values }
}

/// `INSERT INTO <table> (…) VALUES (…) [RETURNING …]`, stamping `updated`
/// from the database clock when `stamp_updated`.
pub fn build_insert(
    table: &str,
    data: &[(&str, Param)],
    returning: &[&str],
    stamp_updated: bool,
) -> PreparedQuery {
    let mut columns: Vec<String> = Vec::new();
    let mut placeholders: Vec<String> = Vec::new();
    if stamp_updated {
        columns.push("updated".to_string());
        placeholders.push("CURRENT_TIMESTAMP".to_string());
    }
    let mut values: Vec<Param> = Vec::new();
    for (column, value) in data {
        values.push(value.clone());
        columns.push((*column).to_string());
        placeholders.push(format!("${}", values.len()));
    }
    let mut sql = format!("INSERT INTO {table} ({}) VALUES ({})", columns.join(", "), placeholders.join(", "));
    if !returning.is_empty() {
        sql.push_str(&format!(" RETURNING {}", returning.join(", ")));
    }
    PreparedQuery { sql, values }
}

/// `UPDATE <table> SET … WHERE <primary key> = $n [RETURNING …]`, or `None`
/// when the data bag sets nothing.
pub fn build_update(
    table: &str,
    primary_key: &str,
    primary_key_value: Param,
    data: &[(&str, Param)],
    returning: &[&str],
    stamp_updated: bool,
) -> Option<PreparedQuery> {
    let mut set_clauses: Vec<String> = Vec::new();
    if stamp_updated {
        set_clauses.push("updated = CURRENT_TIMESTAMP".to_string());
    }
    let mut values: Vec<Param> = Vec::new();
    for (column, value) in data {
        if *column == "raw" {
            continue;
        }
        values.push(value.clone());
        set_clauses.push(format!("{column} = ${}", values.len()));
    }
    if values.is_empty() {
        return None;
    }
    values.push(primary_key_value);
    let mut sql = format!(
        "UPDATE {table} SET {} WHERE {primary_key} = ${}",
        set_clauses.join(", "),
        values.len()
    );
    if !returning.is_empty() {
        sql.push_str(&format!(" RETURNING {}", returning.join(", ")));
    }
    Some(PreparedQuery { sql, values })
}

/// One table: its name, its primary key, and the statements the repositories
/// run against it.
#[derive(Debug, Clone, Copy)]
pub struct Table {
    pub name: &'static str,
    pub primary_key: &'static str,
}

impl Table {
    /// The table's columns in declaration order, from the schema snapshot.
    pub fn columns(&self) -> Vec<&'static str> {
        schema()
            .tables
            .iter()
            .find(|table| table.name == self.name)
            .map(|table| table.columns.iter().map(|(column, _)| column.as_str()).collect())
            .unwrap_or_default()
    }

    /// The builders stamp `updated` only where the DDL declares the column.
    fn stamp_updated(&self) -> bool {
        self.columns().contains(&"updated")
    }

    pub fn prepare_select(
        &self,
        columns: Option<&[&str]>,
        filters: &[(&str, SelectFilter)],
        limit: Option<i64>,
    ) -> PreparedQuery {
        build_select_with_filters(self.name, columns, filters, limit)
    }

    /// Every row matching the filters.
    pub async fn query(&self, filters: &[(&str, SelectFilter)]) -> DbResult<Vec<Record>> {
        Ok(run_query(&self.prepare_select(None, filters, None)).await?.rows)
    }

    pub fn prepare_count(&self, filters: &[(&str, SelectFilter)]) -> PreparedQuery {
        build_select_with_filters(self.name, Some(&["COUNT(*)::int AS count"]), filters, None)
    }

    pub async fn count(&self, filters: &[(&str, SelectFilter)]) -> DbResult<i64> {
        let result = run_query(&self.prepare_count(filters)).await?;
        Ok(result.rows.first().and_then(|row| row.get("count")).and_then(Value::as_i64).unwrap_or(0))
    }

    /// The statement `query_one` runs. A column the table does not declare
    /// fails here instead of as a Postgres error at run time.
    pub fn prepare_query_one(
        &self,
        filters: &[(&str, SelectFilter)],
        columns: Option<&[&str]>,
    ) -> DbResult<PreparedQuery> {
        if let Some(columns) = columns {
            let known = self.columns();
            let unknown: Vec<&str> = columns.iter().copied().filter(|column| !known.contains(column)).collect();
            if !unknown.is_empty() {
                return Err(DbError::new(format!(
                    "queryOne: unknown columns {} for table {}",
                    unknown.join(", "),
                    self.name
                )));
            }
        }
        Ok(self.prepare_select(columns, filters, Some(1)))
    }

    /// The first row matching the filters, whole or projected to `columns`.
    pub async fn query_one(
        &self,
        filters: &[(&str, SelectFilter)],
        columns: Option<&[&str]>,
    ) -> DbResult<Option<Record>> {
        let query = self.prepare_query_one(filters, columns)?;
        Ok(run_query(&query).await?.rows.into_iter().next())
    }

    pub fn prepare_insert(&self, data: &[(&str, Param)], returning: &[&str]) -> PreparedQuery {
        build_insert(self.name, data, returning, self.stamp_updated())
    }

    /// Inserts one row; `None` when nothing came back.
    pub async fn insert(&self, data: &[(&str, Param)], returning: &[&str]) -> DbResult<Option<Record>> {
        Ok(run_query(&self.prepare_insert(data, returning)).await?.rows.into_iter().next())
    }

    pub fn prepare_update(
        &self,
        primary_key_value: Param,
        data: &[(&str, Param)],
        returning: &[&str],
    ) -> Option<PreparedQuery> {
        build_update(self.name, self.primary_key, primary_key_value, data, returning, self.stamp_updated())
    }

    /// Updates the row with this primary key; `None` when no row matched or
    /// the data bag sets nothing.
    pub async fn update(
        &self,
        primary_key_value: Param,
        data: &[(&str, Param)],
        returning: &[&str],
    ) -> DbResult<Option<Record>> {
        let Some(query) = self.prepare_update(primary_key_value, data, returning) else {
            return Ok(None);
        };
        Ok(run_query(&query).await?.rows.into_iter().next())
    }

    pub fn prepare_hard_delete(&self, primary_key_value: Param) -> PreparedQuery {
        PreparedQuery {
            sql: format!(
                "DELETE FROM {} WHERE {} = $1 RETURNING {}",
                self.name, self.primary_key, self.primary_key
            ),
            values: vec![primary_key_value],
        }
    }

    pub async fn hard_delete(&self, primary_key_value: Param) -> DbResult<bool> {
        let result = run_query(&self.prepare_hard_delete(primary_key_value)).await?;
        Ok(result.row_count.is_some_and(|count| count > 0))
    }

    pub fn prepare_delete_where(&self, filters: &[(&str, Filter)]) -> DbResult<PreparedQuery> {
        resolve_mutation_filters("deleteWhere", filters)?;
        let clauses = build_filter_clauses(filters, 1)?;
        Ok(PreparedQuery {
            sql: format!("DELETE FROM {} WHERE {} RETURNING {}", self.name, clauses.sql, self.primary_key),
            values: clauses.values,
        })
    }

    /// Deletes every row matching the filters and returns how many there were.
    pub async fn delete_where(&self, filters: &[(&str, Filter)]) -> DbResult<u64> {
        let query = self.prepare_delete_where(filters)?;
        Ok(run_query(&query).await?.row_count.unwrap_or(0))
    }

    /// The statement `update_where` runs; `None` when the data bag sets
    /// nothing.
    pub fn prepare_update_where(
        &self,
        filters: &[(&str, Filter)],
        data: &[(&str, SetValue)],
        returning: &[&str],
    ) -> DbResult<Option<PreparedQuery>> {
        resolve_mutation_filters("updateWhere", filters)?;
        if data.is_empty() {
            return Ok(None);
        }
        let mut values: Vec<Param> = Vec::new();
        let set_clauses: Vec<String> = data
            .iter()
            .map(|(column, value)| match value {
                SetValue::DbNow => format!("{column} = CURRENT_TIMESTAMP"),
                SetValue::Value(value) => {
                    values.push(value.clone());
                    format!("{column} = ${}", values.len())
                }
            })
            .collect();
        let clauses = build_filter_clauses(filters, values.len() + 1)?;
        let returning_clause = if returning.is_empty() {
            String::new()
        } else {
            format!(" RETURNING {}", returning.join(", "))
        };
        values.extend(clauses.values);
        Ok(Some(PreparedQuery {
            sql: format!(
                "UPDATE {} SET {} WHERE {}{returning_clause}",
                self.name,
                set_clauses.join(", "),
                clauses.sql
            ),
            values,
        }))
    }

    /// Updates every row matching the filters and returns the `returning`
    /// columns of each.
    pub async fn update_where(
        &self,
        filters: &[(&str, Filter)],
        data: &[(&str, SetValue)],
        returning: &[&str],
    ) -> DbResult<Vec<Record>> {
        let Some(query) = self.prepare_update_where(filters, data, returning)? else {
            return Ok(Vec::new());
        };
        Ok(run_query(&query).await?.rows)
    }
}

/// A scripted stand-in for the pool: unit tests hand it the results each
/// statement gets, and read back the statements that were issued.
#[cfg(test)]
pub(crate) mod script {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::{Arc, Mutex};

    use super::{DbResult, Param, PreparedQuery, Rows};

    #[derive(Default)]
    struct Script {
        responses: VecDeque<DbResult<Rows>>,
        trace: Vec<PreparedQuery>,
    }

    tokio::task_local! {
        static SCRIPT: Arc<Mutex<Script>>;
    }

    pub(crate) fn intercept(sql: &str, values: &[Param]) -> Option<DbResult<Rows>> {
        SCRIPT
            .try_with(|script| {
                let mut script = script.lock().unwrap();
                script.trace.push(PreparedQuery { sql: sql.to_string(), values: values.to_vec() });
                let at = script.trace.len();
                script.responses.pop_front().unwrap_or_else(|| panic!("no scripted response for statement {at}: {sql}"))
            })
            .ok()
    }

    /// A response of these rows, given as a JSON array of objects.
    pub(crate) fn rows(rows: serde_json::Value) -> DbResult<Rows> {
        let rows: Vec<super::Record> = rows
            .as_array()
            .expect("the rows are a JSON array")
            .iter()
            .map(|row| row.as_object().expect("a row is a JSON object").clone())
            .collect();
        Ok(Rows { row_count: Some(rows.len() as u64), rows })
    }

    /// A response with no rows that reports `count` rows affected.
    pub(crate) fn affected(count: u64) -> DbResult<Rows> {
        Ok(Rows { rows: Vec::new(), row_count: Some(count) })
    }

    /// A failed statement with this SQLSTATE.
    pub(crate) fn fault(code: &str) -> DbResult<Rows> {
        Err(super::DbError { code: Some(code.to_string()), ..super::DbError::new(format!("scripted fault {code}")) })
    }

    /// Runs `work` with every statement answered from `responses` in order,
    /// and returns its output with the statements it issued.
    pub(crate) async fn with_script<F: Future>(
        responses: Vec<DbResult<Rows>>,
        work: F,
    ) -> (F::Output, Vec<PreparedQuery>) {
        let script = Arc::new(Mutex::new(Script { responses: responses.into(), trace: Vec::new() }));
        let output = SCRIPT.scope(script.clone(), work).await;
        let trace = std::mem::take(&mut script.lock().unwrap().trace);
        (output, trace)
    }
}

#[cfg(test)]
mod tests {
    use super::script::with_script;
    use super::*;

    const USER_ID: &str = "11111111-1111-1111-1111-111111111111";
    const MAIL_ID: &str = "22222222-2222-2222-2222-222222222222";

    const MAILS: Table = Table { name: "mails", primary_key: "mail_id" };

    fn refusal(method: &str, filters: &[(&str, Filter)]) -> String {
        resolve_mutation_filters(method, filters).expect_err("the filters are refused").message
    }

    // models.test.ts — "resolveMutationFilters rejects an undefined filter value"

    #[test]
    fn refuses_rather_than_dropping_the_predicate_and_widening_the_statement() {
        let message = refusal("updateWhere", &[("mail_id", Filter::Missing), ("user_id", USER_ID.into())]);
        assert!(message.contains("updateWhere received undefined for mail_id"));
    }

    #[test]
    fn names_the_calling_method() {
        let message = refusal("deleteWhere", &[("mail_id", Filter::Missing), ("user_id", USER_ID.into())]);
        assert!(message.contains("deleteWhere received undefined for mail_id"));
    }

    #[test]
    fn names_every_undefined_column_not_just_the_first() {
        let message = refusal(
            "updateWhere",
            &[("mail_id", Filter::Missing), ("message_id", Filter::Missing), ("user_id", USER_ID.into())],
        );
        assert!(message.contains("mail_id, message_id"));
    }

    #[test]
    fn refuses_even_when_a_scoping_column_survives() {
        // The shape a mark with no mail id produced: the owner alone would
        // have matched the whole mailbox.
        let filters = [("mail_id", Filter::optional(None::<&str>)), ("user_id", Filter::from(USER_ID))];
        assert!(resolve_mutation_filters("updateWhere", &filters).is_err());
    }

    #[test]
    fn refuses_an_entirely_empty_filter_bag() {
        assert!(refusal("updateWhere", &[]).contains("updateWhere requires at least one filter"));
        assert!(refusal("deleteWhere", &[]).contains("deleteWhere requires at least one filter"));
    }

    // models.test.ts — "resolveMutationFilters keeps every legitimate filter shape"

    #[test]
    fn accepts_the_scoped_id_and_owner_case() {
        let filters = [("mail_id", Filter::from(MAIL_ID)), ("user_id", Filter::from(USER_ID))];
        assert!(resolve_mutation_filters("updateWhere", &filters).is_ok());
        let clauses = build_filter_clauses(&filters, 1).unwrap();
        assert_eq!(clauses.sql, "mail_id = $1 AND user_id = $2");
        assert_eq!(clauses.values, vec![Param::from(MAIL_ID), Param::from(USER_ID)]);
    }

    #[test]
    fn an_absent_key_is_not_an_undefined_key() {
        let quarantined = false;
        let mut filters =
            vec![("user_id", Filter::from(USER_ID)), ("sent", Filter::from(false)), ("expunged", Filter::from(false))];
        if quarantined {
            filters.push(("is_spam", Filter::from(false)));
        }
        assert!(resolve_mutation_filters("updateWhere", &filters).is_ok());
        assert_eq!(build_filter_clauses(&filters, 1).unwrap().sql, "user_id = $1 AND sent = $2 AND expunged = $3");
    }

    #[test]
    fn includes_the_conditional_key_when_it_is_given() {
        let filters =
            [("user_id", Filter::from(USER_ID)), ("expunged", Filter::from(false)), ("is_spam", Filter::from(false))];
        assert!(resolve_mutation_filters("updateWhere", &filters).is_ok());
        assert_eq!(
            build_filter_clauses(&filters, 1).unwrap().sql,
            "user_id = $1 AND expunged = $2 AND is_spam = $3"
        );
    }

    #[test]
    fn refuses_a_condition_whose_value_is_missing() {
        let filters = [
            ("mail_id", Filter::Compare { op: CompareOp::Eq, value: None, not_null: false }),
            ("user_id", Filter::from(USER_ID)),
        ];
        assert!(refusal("updateWhere", &filters).contains("updateWhere received undefined for mail_id"));
    }

    #[test]
    fn passes_an_in_list_through_untouched() {
        let filters = [("mail_id", Filter::any_of(&[MAIL_ID]))];
        assert!(resolve_mutation_filters("updateWhere", &filters).is_ok());
        let clauses = build_filter_clauses(&filters, 1).unwrap();
        assert_eq!(clauses.sql, "mail_id IN ($1)");
        assert_eq!(clauses.values, vec![Param::from(MAIL_ID)]);
    }

    #[test]
    fn treats_false_zero_empty_string_and_null_as_real_predicates() {
        let filters = [
            ("expunged", Filter::from(false)),
            ("uid_domain", Filter::from(0)),
            ("subject", Filter::from("")),
            ("insight", Filter::Equals(Param::Null)),
        ];
        assert!(resolve_mutation_filters("deleteWhere", &filters).is_ok());
        let clauses = build_filter_clauses(&filters, 1).unwrap();
        assert_eq!(clauses.sql, "expunged = $1 AND uid_domain = $2 AND subject = $3 AND insight = $4");
        assert_eq!(clauses.values.len(), 4);
    }

    // models.test.ts — "Table wires both mutation builders through the guard"

    #[tokio::test]
    async fn update_where_and_delete_where_both_run_the_guard_before_any_statement() {
        let unguarded = [("mail_id", Filter::Missing), ("user_id", Filter::from(USER_ID))];
        let data = [("read", SetValue::from(true))];

        let (updated, trace) = with_script(vec![], MAILS.update_where(&unguarded, &data, &[])).await;
        assert!(updated.unwrap_err().message.contains("updateWhere received undefined for mail_id"));
        assert!(trace.is_empty());

        let (deleted, trace) = with_script(vec![], MAILS.delete_where(&unguarded)).await;
        assert!(deleted.unwrap_err().message.contains("deleteWhere received undefined for mail_id"));
        assert!(trace.is_empty());

        // The data bag legitimately sets nothing without failing: only
        // filters are strict.
        let (updated, trace) = with_script(vec![], MAILS.update_where(&[("mail_id", MAIL_ID.into())], &[], &[])).await;
        assert!(updated.unwrap().is_empty());
        assert!(trace.is_empty());
    }

    #[test]
    fn stamps_updated_only_where_the_table_declares_the_column() {
        // `mailboxes` declares `created` and no `updated`: a stamped INSERT
        // would fail every IMAP CREATE.
        let mailboxes = Table { name: "mailboxes", primary_key: "mailbox_id" };
        let insert = mailboxes.prepare_insert(&[("name", Param::from("Archive"))], &["*"]);
        assert_eq!(insert.sql, "INSERT INTO mailboxes (name) VALUES ($1) RETURNING *");
        let insert = MAILS.prepare_insert(&[("subject", Param::from("s"))], &["mail_id"]);
        assert_eq!(
            insert.sql,
            "INSERT INTO mails (updated, subject) VALUES (CURRENT_TIMESTAMP, $1) RETURNING mail_id"
        );
    }
}
