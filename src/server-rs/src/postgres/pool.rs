//! The connection pool and the value conversions every query goes through.
//!
//! Parameters are bound as text and typed by the statement, and rows come
//! back as [`Value`]s shaped the way the HTTP API serializes them: BIGINT and
//! NUMERIC are numbers, a `timestamptz` is an ISO 8601 string in milliseconds.
//!
//! ```ignore
//! let result = pool().query("SELECT user_id FROM users WHERE username = $1", &params![name]).await?;
//! let id = result.rows.first().map(|row| row.text("user_id"));
//! ```

use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use bytes::BytesMut;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use deadpool_postgres::{Manager, ManagerConfig, Object, RecyclingMethod, Timeouts};
use fallible_iterator::FallibleIterator;
use futures_util::{pin_mut, TryStreamExt};
use serde_json::{Map, Number, Value};
use tokio_postgres::types::{to_sql_checked, Format, FromSql, IsNull, Kind, ToSql, Type};
use tokio_postgres::{NoTls, Row as PgRow};

use crate::js;

/// A statement parameter, already in the text form Postgres parses.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Null,
    Text(String),
    Bytes(Vec<u8>),
}

impl Param {
    /// A value for a JSON or JSONB column.
    pub fn json<T: serde::Serialize + ?Sized>(value: &T) -> Param {
        match serde_json::to_string(value) {
            Ok(text) => Param::Text(text),
            Err(_) => Param::Null,
        }
    }

    /// A Postgres array, such as `{"a","b c",NULL}`, for `= ANY($1)`.
    pub fn array<T: AsRef<str>>(values: &[T]) -> Param {
        let items: Vec<String> = values
            .iter()
            .map(|value| format!("\"{}\"", value.as_ref().replace('\\', "\\\\").replace('"', "\\\"")))
            .collect();
        Param::Text(format!("{{{}}}", items.join(",")))
    }
}

impl ToSql for Param {
    fn to_sql(&self, _ty: &Type, out: &mut BytesMut) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        match self {
            Param::Null => Ok(IsNull::Yes),
            Param::Text(text) => {
                out.extend_from_slice(text.as_bytes());
                Ok(IsNull::No)
            }
            Param::Bytes(bytes) => {
                out.extend_from_slice(bytes);
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        match self {
            Param::Bytes(_) => Format::Binary,
            _ => Format::Text,
        }
    }

    to_sql_checked!();
}

macro_rules! param_from_display {
    ($($ty:ty),*) => {$(
        impl From<$ty> for Param {
            fn from(value: $ty) -> Param { Param::Text(value.to_string()) }
        }
    )*};
}
param_from_display!(&str, String, &String, i16, i32, i64, u16, u32, u64, usize, bool, uuid::Uuid);

impl From<f64> for Param {
    fn from(value: f64) -> Param {
        Param::Text(js::number_to_string(value))
    }
}

impl From<DateTime<Utc>> for Param {
    fn from(value: DateTime<Utc>) -> Param {
        Param::Text(js::iso_string(&value))
    }
}

impl From<Vec<u8>> for Param {
    fn from(value: Vec<u8>) -> Param {
        Param::Bytes(value)
    }
}

impl From<&[String]> for Param {
    fn from(values: &[String]) -> Param {
        Param::array(values)
    }
}

impl From<&Vec<String>> for Param {
    fn from(values: &Vec<String>) -> Param {
        Param::array(values)
    }
}

impl From<Vec<String>> for Param {
    fn from(values: Vec<String>) -> Param {
        Param::array(&values)
    }
}

impl<T: Into<Param>> From<Option<T>> for Param {
    fn from(value: Option<T>) -> Param {
        value.map(Into::into).unwrap_or(Param::Null)
    }
}

/// Builds a `Vec<Param>` from values of mixed types.
///
/// ```ignore
/// pool().query("UPDATE mails SET read = $1 WHERE mail_id = $2", &params![true, mail_id]).await?;
/// ```
#[macro_export]
macro_rules! params {
    () => { Vec::<$crate::postgres::Param>::new() };
    ($($value:expr),+ $(,)?) => { vec![$($crate::postgres::Param::from($value)),+] };
}

/// A failed statement. `code` is the SQLSTATE, such as `23505` on a unique
/// violation.
#[derive(Debug, Clone)]
pub struct DbError {
    pub message: String,
    pub code: Option<String>,
    pub detail: Option<String>,
    pub constraint: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> DbError {
        DbError {
            message: message.into(),
            code: None,
            detail: None,
            constraint: None,
            table: None,
            column: None,
        }
    }

    pub fn is_unique_violation(&self) -> bool {
        self.code.as_deref() == Some("23505")
    }

    fn timeout() -> DbError {
        DbError::new("Query read timeout")
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for DbError {}

impl From<tokio_postgres::Error> for DbError {
    fn from(error: tokio_postgres::Error) -> DbError {
        match error.as_db_error() {
            Some(db) => DbError {
                message: db.message().to_string(),
                code: Some(db.code().code().to_string()),
                detail: db.detail().map(str::to_string),
                constraint: db.constraint().map(str::to_string),
                table: db.table().map(str::to_string),
                column: db.column().map(str::to_string),
            },
            None => DbError::new(error.to_string()),
        }
    }
}

pub type DbResult<T> = Result<T, DbError>;

/// A column value decoded by its Postgres type rather than by a Rust type the
/// caller names, so one row shape serves every query.
#[derive(Debug, Clone)]
struct Dynamic(Value);

fn numeric_to_f64(raw: &[u8]) -> Option<f64> {
    if raw.len() < 8 {
        return None;
    }
    let read = |at: usize| i16::from_be_bytes([raw[at], raw[at + 1]]);
    let ndigits = read(0) as usize;
    let weight = read(2) as i32;
    let sign = u16::from_be_bytes([raw[4], raw[5]]);
    if sign == 0xC000 {
        return Some(f64::NAN);
    }
    if raw.len() < 8 + ndigits * 2 {
        return None;
    }
    let mut value = 0f64;
    for i in 0..ndigits {
        let digit = read(8 + i * 2) as f64;
        value += digit * 10_000f64.powi(weight - i as i32);
    }
    Some(if sign == 0x4000 { -value } else { value })
}

fn number(value: f64) -> Value {
    if value.is_finite() && value == value.trunc() && value.abs() < 9.007_199_254_740_992e15 {
        Value::Number(Number::from(value as i64))
    } else {
        Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
    }
}

fn decode(ty: &Type, raw: &[u8]) -> Result<Value, Box<dyn StdError + Sync + Send>> {
    if let Kind::Array(inner) = ty.kind() {
        let array = postgres_protocol::types::array_from_sql(raw)?;
        let mut items = Vec::new();
        let mut values = array.values();
        while let Some(item) = values.next()? {
            items.push(match item {
                Some(bytes) => decode(inner, bytes)?,
                None => Value::Null,
            });
        }
        return Ok(Value::Array(items));
    }
    Ok(match *ty {
        Type::BOOL => Value::Bool(bool::from_sql(ty, raw)?),
        Type::INT2 => Value::from(i16::from_sql(ty, raw)?),
        Type::INT4 => Value::from(i32::from_sql(ty, raw)?),
        Type::INT8 => Value::from(i64::from_sql(ty, raw)?),
        Type::OID => Value::from(u32::from_sql(ty, raw)?),
        Type::FLOAT4 => number(f32::from_sql(ty, raw)? as f64),
        Type::FLOAT8 => number(f64::from_sql(ty, raw)?),
        Type::NUMERIC => numeric_to_f64(raw).map(number).unwrap_or(Value::Null),
        Type::UUID => Value::String(uuid::Uuid::from_sql(ty, raw)?.to_string()),
        Type::TIMESTAMPTZ => Value::String(js::iso_string(&DateTime::<Utc>::from_sql(ty, raw)?)),
        Type::TIMESTAMP => Value::String(js::iso_string(&NaiveDateTime::from_sql(ty, raw)?.and_utc())),
        Type::DATE => Value::String(NaiveDate::from_sql(ty, raw)?.format("%Y-%m-%d").to_string()),
        Type::JSON | Type::JSONB => {
            tokio_postgres::types::Json::<Value>::from_sql(ty, raw)?.0
        }
        Type::BYTEA => Value::String(hex::encode(raw)),
        _ => Value::String(String::from_utf8_lossy(raw).into_owned()),
    })
}

impl<'a> FromSql<'a> for Dynamic {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        decode(ty, raw).map(Dynamic)
    }

    fn from_sql_null(_ty: &Type) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(Dynamic(Value::Null))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

/// One result row, with every column decoded to JSON.
#[derive(Debug, Clone, Default)]
pub struct Row {
    columns: Map<String, Value>,
}

impl Row {
    fn from_pg(row: &PgRow) -> DbResult<Row> {
        let mut columns = Map::with_capacity(row.len());
        for (index, column) in row.columns().iter().enumerate() {
            let value: Dynamic = row.try_get(index).map_err(DbError::from)?;
            columns.insert(column.name().to_string(), value.0);
        }
        Ok(Row { columns })
    }

    /// The column's value; `Value::Null` for SQL NULL or a column the query did not select.
    pub fn get(&self, column: &str) -> &Value {
        self.columns.get(column).unwrap_or(&Value::Null)
    }

    /// The value moved out of the row, leaving NULL behind.
    pub fn take(&mut self, column: &str) -> Value {
        self.columns.get_mut(column).map(Value::take).unwrap_or(Value::Null)
    }

    pub fn has(&self, column: &str) -> bool {
        self.columns.contains_key(column)
    }

    /// A text, UUID or timestamp column; empty when NULL.
    pub fn text(&self, column: &str) -> String {
        self.opt_text(column).unwrap_or_default()
    }

    pub fn opt_text(&self, column: &str) -> Option<String> {
        self.get(column).as_str().map(str::to_string)
    }

    pub fn bool(&self, column: &str) -> bool {
        self.get(column).as_bool().unwrap_or(false)
    }

    pub fn opt_bool(&self, column: &str) -> Option<bool> {
        self.get(column).as_bool()
    }

    pub fn i64(&self, column: &str) -> i64 {
        self.opt_i64(column).unwrap_or(0)
    }

    pub fn opt_i64(&self, column: &str) -> Option<i64> {
        let value = self.get(column);
        value.as_i64().or_else(|| value.as_f64().map(|f| f as i64))
    }

    /// A `timestamptz` column as a point in time.
    pub fn opt_time(&self, column: &str) -> Option<DateTime<Utc>> {
        let text = self.get(column).as_str()?;
        DateTime::parse_from_rfc3339(text).ok().map(|t| t.with_timezone(&Utc))
    }

    /// A JSON or JSONB column, `None` when NULL.
    pub fn opt_json(&self, column: &str) -> Option<&Value> {
        match self.get(column) {
            Value::Null => None,
            value => Some(value),
        }
    }

    /// The whole row, column name to value, in select order.
    pub fn into_map(self) -> Map<String, Value> {
        self.columns
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.columns
    }
}

#[derive(Debug, Default)]
pub struct QueryResult {
    pub rows: Vec<Row>,
    /// Rows returned by a SELECT or affected by an INSERT, UPDATE or DELETE.
    /// `None` for a statement that reports no count, such as DDL.
    pub row_count: Option<u64>,
}

impl QueryResult {
    /// The first row, consumed.
    pub fn into_first(self) -> Option<Row> {
        self.rows.into_iter().next()
    }

    /// Whether the statement returned or affected at least one row.
    pub fn any(&self) -> bool {
        self.row_count.unwrap_or(self.rows.len() as u64) > 0
    }
}

async fn run(
    client: &tokio_postgres::Client,
    sql: &str,
    params: &[Param],
    timeout: Duration,
) -> DbResult<QueryResult> {
    let execute = async {
        let typed = params.iter().map(|param| (param, Type::UNKNOWN));
        let stream = client.query_typed_raw(sql, typed).await?;
        pin_mut!(stream);
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await? {
            rows.push(Row::from_pg(&row)?);
        }
        let affected = stream.rows_affected();
        // A SELECT reports its row count in the same tag an UPDATE reports
        // the rows it touched; DDL and SET report none.
        let row_count = match affected {
            Some(count) => Some(count),
            None if !rows.is_empty() => Some(rows.len() as u64),
            None => None,
        };
        Ok::<_, DbError>(QueryResult { rows, row_count })
    };

    if timeout.is_zero() {
        return execute.await;
    }
    match tokio::time::timeout(timeout, execute).await {
        Ok(result) => result,
        Err(_) => {
            // The backend is still working on the statement; cancel it so the
            // connection is usable again.
            let token = client.cancel_token();
            tokio::spawn(async move {
                let _ = token.cancel_query(NoTls).await;
            });
            Err(DbError::timeout())
        }
    }
}

pub struct PoolOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub database: String,
    pub max: usize,
    pub connection_timeout: Duration,
    /// Server-side `statement_timeout`, set on every connection.
    pub statement_timeout: Duration,
    /// Client-side backstop that fails a statement the server never answers.
    pub query_timeout: Duration,
}

/// The connection pool. Statements run unprepared, one round trip each.
pub struct Pool {
    inner: deadpool_postgres::Pool,
    query_timeout: Duration,
}

impl Pool {
    pub fn new(options: PoolOptions) -> Pool {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&options.host)
            .port(options.port)
            .user(&options.user)
            .dbname(&options.database)
            .connect_timeout(options.connection_timeout)
            .options(format!("-c statement_timeout={}", options.statement_timeout.as_millis()));
        if let Some(password) = &options.password {
            config.password(password);
        }
        let manager = Manager::from_config(
            config,
            NoTls,
            ManagerConfig { recycling_method: RecyclingMethod::Fast },
        );
        let inner = deadpool_postgres::Pool::builder(manager)
            .max_size(options.max)
            .runtime(deadpool_postgres::Runtime::Tokio1)
            .timeouts(Timeouts {
                create: Some(options.connection_timeout),
                wait: None,
                recycle: Some(options.connection_timeout),
            })
            .build()
            .expect("the pool configuration is valid");
        Pool { inner, query_timeout: options.query_timeout }
    }

    async fn checkout(&self) -> DbResult<Object> {
        // A connection the server refused carries the server's own message
        // ("database … is not currently accepting connections"), which is
        // what an alarm about it should say.
        self.inner.get().await.map_err(|error| match error {
            deadpool_postgres::PoolError::Backend(backend) => DbError::from(backend),
            other => DbError::new(other.to_string()),
        })
    }

    pub async fn query(&self, sql: &str, params: &[Param]) -> DbResult<QueryResult> {
        let client = self.checkout().await?;
        run(&client, sql, params, self.query_timeout).await
    }

    /// Checks out one connection for statements that must share a session: a
    /// transaction, an advisory lock, a session-level `SET`.
    pub async fn connect(&self) -> DbResult<Client> {
        Ok(Client {
            object: Some(self.checkout().await?),
            query_timeout: self.query_timeout,
            in_transaction: false,
        })
    }

    /// Closes the pool; statements in flight finish on their own connections.
    pub fn end(&self) {
        self.inner.close();
    }
}

/// One checked-out connection. Dropping it returns the connection to the
/// pool, unless a transaction was left open or [`Client::release_destroy`]
/// was called, in which case the connection is closed instead.
pub struct Client {
    object: Option<Object>,
    query_timeout: Duration,
    in_transaction: bool,
}

impl Client {
    fn track(&mut self, sql: &str) {
        let head = sql.trim_start().get(..8).unwrap_or(sql.trim_start()).to_ascii_uppercase();
        if head.starts_with("BEGIN") || head.starts_with("START") {
            self.in_transaction = true;
        } else if head.starts_with("COMMIT") || head.starts_with("ROLLBACK") || head.starts_with("END") {
            self.in_transaction = false;
        }
    }

    pub async fn query(&mut self, sql: &str, params: &[Param]) -> DbResult<QueryResult> {
        let timeout = self.query_timeout;
        self.query_with_timeout(sql, params, timeout).await
    }

    /// Like `query`, with a client-side bound of its own for this statement.
    pub async fn query_with_timeout(
        &mut self,
        sql: &str,
        params: &[Param],
        timeout: Duration,
    ) -> DbResult<QueryResult> {
        let object = self.object.as_ref().expect("the client is live until dropped");
        let result = run(object, sql, params, timeout).await;
        if result.is_ok() {
            self.track(sql);
        }
        result
    }

    /// Closes the connection instead of returning it to the pool.
    pub fn release_destroy(mut self) {
        if let Some(object) = self.object.take() {
            drop(Object::take(object));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.in_transaction {
            if let Some(object) = self.object.take() {
                drop(Object::take(object));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_literals() {
        assert_eq!(Param::array(&["a", "b c"]), Param::Text("{\"a\",\"b c\"}".into()));
        assert_eq!(Param::array(&["q\"uote", "back\\slash"]), Param::Text("{\"q\\\"uote\",\"back\\\\slash\"}".into()));
        assert_eq!(Param::array::<&str>(&[]), Param::Text("{}".into()));
    }

    #[test]
    fn params_macro() {
        let values = params!["a", 5, true, Option::<String>::None, 1.5];
        assert_eq!(
            values,
            vec![
                Param::Text("a".into()),
                Param::Text("5".into()),
                Param::Text("true".into()),
                Param::Null,
                Param::Text("1.5".into()),
            ]
        );
    }
}
