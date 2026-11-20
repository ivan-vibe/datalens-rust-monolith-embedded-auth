//! MySQL executor: the third real database behind the Rust data-api.
//!
//! The Python `dl_connector_mysql` speaks MySQL through `pymysql`/SQLAlchemy; the port speaks
//! it through `sqlx`'s MySQL driver, the same way `dl_postgres` uses `tokio-postgres`. The
//! boundary stays the one [`dl_api::Executor`] already has: rows are text, and the dataset's
//! field types decide the JSON value, so the executor owns only the driver type -> text
//! mapping (see [`CellKind`]).
//!
//! [`MysqlConfig`] parses the DSN `dl_us` resolves from the dataset's `connection` entry
//! (`mysql://user:password@host:port/database`, `mysqls://` for TLS), and [`MysqlExecutor`]
//! keeps one `sqlx` pool per DSN — a dashboard runs many charts, and one connection per
//! database would serialise them.

use std::collections::HashMap;

use async_trait::async_trait;
use dl_api::{Executor, ExecutorError, RowStream};
use sqlx::mysql::{MySqlConnection, MySqlPool as SqlxPool, MySqlPoolOptions, MySqlRow};
use sqlx::{Column, Connection, Row, TypeInfo};
use tokio::sync::RwLock;

pub mod dsn;

pub use dsn::{DsnError, MysqlConfig};

/// Connections one DSN may use at the same time.
///
/// The same reasoning as `dl_postgres::DEFAULT_POOL_SIZE`: a dashboard opens a dozen charts at
/// once and every chart runs two queries, while the server's `max_connections` is shared by
/// every data-api process. `DL_DB_POOL_SIZE` overrides it from the server.
pub const DEFAULT_POOL_SIZE: usize = 8;

/// How long the first connection of a database may take.
///
/// Python's `pymysql` defaults `connect_timeout` to 10 s; `sqlx`'s MySQL options have no such
/// knob, so the probe connect is bounded here. Without it a blackholed host would hold the
/// request until the OS gives up (tens of seconds).
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How one MySQL column is turned into the text cell `dl_api` decodes.
///
/// The mapping follows `dl_connector_mysql.core.type_transformer.MySQLTypeTransformer`:
/// TINYINT..BIGINT -> integer, FLOAT/DOUBLE/DECIMAL -> float, BIT -> boolean, CHAR/VARCHAR/
/// TEXT/_BLOB -> string, DATE -> date, DATETIME/TIMESTAMP -> genericdatetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    /// `BOOLEAN` (TINYINT(1)) and `BIT`.
    Bool,
    /// Signed `TINYINT`..`BIGINT`.
    Int,
    /// The `UNSIGNED` spellings of the same.
    UInt,
    Float,
    /// `DECIMAL`: MySQL sends it as a text number, and no decimal crate is available offline.
    Decimal,
    /// Character types and `ENUM`.
    Text,
    /// `SET`: a comma-separated list, sent as text but not `Type`-compatible with `String`.
    Set,
    /// Binary types: `BINARY`/`VARBINARY`/`*BLOB`/`GEOMETRY`.
    Bytes,
    Date,
    DateTime,
    Time,
    /// `YEAR`: a two-byte integer.
    Year,
    /// `JSON`, sent as the JSON document text.
    Json,
}

/// The type name -> [`CellKind`] table. Unknown names are a decode error, never silent text.
fn cell_kind(type_name: &str) -> Option<CellKind> {
    Some(match type_name {
        "BOOLEAN" | "BIT" => CellKind::Bool,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => CellKind::Int,
        "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
        | "BIGINT UNSIGNED" => CellKind::UInt,
        "FLOAT" | "DOUBLE" => CellKind::Float,
        "DECIMAL" => CellKind::Decimal,
        "CHAR" | "VARCHAR" | "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" => {
            CellKind::Text
        }
        "SET" => CellKind::Set,
        "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB" | "GEOMETRY" => {
            CellKind::Bytes
        }
        "DATE" => CellKind::Date,
        "DATETIME" | "TIMESTAMP" => CellKind::DateTime,
        "TIME" => CellKind::Time,
        "YEAR" => CellKind::Year,
        "JSON" => CellKind::Json,
        _ => return None,
    })
}

/// One row as the text cells `dl_api` decodes.
fn text_row(row: &MySqlRow) -> Result<Vec<Option<String>>, String> {
    (0..row.len()).map(|index| text_cell(row, index)).collect()
}

fn text_cell(row: &MySqlRow, index: usize) -> Result<Option<String>, String> {
    let type_name = row.column(index).type_info().name();
    let kind = cell_kind(type_name).ok_or_else(|| format!("unsupported MySQL type {type_name}"))?;
    let cell = match kind {
        // `BOOLEAN` is TINYINT(1) and `BIT` is bit string; both are the connector's boolean.
        CellKind::Bool => row
            .try_get::<Option<bool>, _>(index)
            .map_err(|error| error.to_string())?
            .map(|value| {
                if value {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }),
        CellKind::Int => row
            .try_get::<Option<i64>, _>(index)
            .map_err(|error| error.to_string())?
            .map(|value| value.to_string()),
        CellKind::UInt => row
            .try_get::<Option<u64>, _>(index)
            .map_err(|error| error.to_string())?
            .map(|value| value.to_string()),
        CellKind::Float => row
            .try_get::<Option<f64>, _>(index)
            .map_err(|error| error.to_string())?
            .map(|value| value.to_string()),
        // `sqlx` has no float decoder for DECIMAL on purpose (`f32`/`f64` semantics differ),
        // and MySQL sends the value itself as text, so it reaches the API unchanged; the field
        // type then runs `python_float` exactly as it does for PostgreSQL's `numeric`.
        CellKind::Decimal => row
            .try_get_unchecked::<Option<String>, _>(index)
            .map_err(|error| error.to_string())?,
        CellKind::Text => row
            .try_get::<Option<String>, _>(index)
            .map_err(|error| error.to_string())?,
        CellKind::Set | CellKind::Json => row
            .try_get_unchecked::<Option<String>, _>(index)
            .map_err(|error| error.to_string())?,
        // `GEOMETRY` is a length-encoded byte string too, and `sqlx` has no `Type` impl for
        // it, so every binary kind goes through the unchecked decoder.
        CellKind::Bytes => row
            .try_get_unchecked::<Option<Vec<u8>>, _>(index)
            .map_err(|error| error.to_string())?
            .map(|value| value.iter().map(|byte| format!("{byte:02x}")).collect()),
        CellKind::Date => row
            .try_get::<Option<chrono::NaiveDate>, _>(index)
            .map_err(|error| error.to_string())?
            .map(|value| value.format("%Y-%m-%d").to_string()),
        // The same text the PostgreSQL executor produces, so `dl_api::decode_value` handles
        // both backends with one datetime postprocessor.
        CellKind::DateTime => row
            .try_get::<Option<chrono::NaiveDateTime>, _>(index)
            .map_err(|error| error.to_string())?
            .map(|value| value.format("%Y-%m-%dT%H:%M:%S").to_string()),
        CellKind::Time => row
            .try_get::<Option<chrono::NaiveTime>, _>(index)
            .map_err(|error| error.to_string())?
            .map(|value| value.format("%H:%M:%S").to_string()),
        CellKind::Year => row
            .try_get_unchecked::<Option<u64>, _>(index)
            .map_err(|error| error.to_string())?
            .map(|value| value.to_string()),
    };
    Ok(cell)
}

/// The text of a driver error, with what `Display` leaves out.
///
/// `sqlx::Error`'s `Display` for a server-side error is just the message; the MySQL code is in
/// the `DatabaseError`, and `dl_api::ApiError::database` classifies the text (1146 -> "doesn't
/// exist", 1064 -> syntax error), so it has to survive into the message.
fn describe(error: &sqlx::Error) -> String {
    match error {
        sqlx::Error::Database(database) => {
            let code = database
                .code()
                .map(|code| code.into_owned())
                .unwrap_or_default();
            let message = database.message();
            match code.as_str() {
                "" => message.to_string(),
                // `dl_core.connectors.base.error_transformer` maps 1064 to `InvalidQuery`;
                // `classify_db_error` looks for the words "syntax error".
                "1064" => format!("syntax error: {message} (MySQL {code})"),
                _ => format!("{message} (MySQL {code})"),
            }
        }
        other => other.to_string(),
    }
}

/// The pool settings every MySQL database gets: bounded size and a bounded wait for a
/// connection, so one saturated database cannot hold a request forever.
fn pool_options(max_connections: usize) -> MySqlPoolOptions {
    MySqlPoolOptions::new()
        .max_connections(max_connections as u32)
        .acquire_timeout(std::time::Duration::from_secs(10))
}

fn describe_query(error: &sqlx::Error) -> String {
    format!("query: {}", describe(error))
}

/// `dl_api::error` recognises a driver connection problem by the `connect:` prefix.
fn describe_connect(error: &sqlx::Error) -> String {
    format!("connect: {}", describe(error))
}

/// A MySQL executor that opens the database each DSN names.
///
/// The DSN comes from the dataset's `connection` entry (`dl_us::UsConnectionCache`), so one
/// process serves MySQL datasets of any tenant, next to PostgreSQL and ClickHouse ones.
pub struct MysqlExecutor {
    pools: RwLock<HashMap<String, SqlxPool>>,
    max_connections: usize,
}

impl Default for MysqlExecutor {
    fn default() -> Self {
        Self::with_max_connections(DEFAULT_POOL_SIZE)
    }
}

impl MysqlExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// A pool that opens at most `max_connections` connections per database
    /// (`DL_DB_POOL_SIZE` in the server).
    pub fn with_max_connections(max_connections: usize) -> Self {
        Self {
            pools: RwLock::new(HashMap::new()),
            max_connections: max_connections.max(1),
        }
    }

    /// The pool for `dsn`, created on first use.
    ///
    /// # Errors
    /// [`ExecutorError`] when the DSN is not a MySQL DSN or the server refuses the connection.
    pub async fn pool(&self, dsn: &str) -> Result<SqlxPool, ExecutorError> {
        if let Some(pool) = self.pools.read().await.get(dsn) {
            return Ok(pool.clone());
        }
        // The write lock is held across the first connect on purpose: two simultaneous
        // requests for a new database would otherwise open two pools and leak one.
        let mut pools = self.pools.write().await;
        if let Some(pool) = pools.get(dsn) {
            return Ok(pool.clone());
        }
        let config = MysqlConfig::from_dsn(dsn)
            .map_err(|error| ExecutorError::new(format!("mysql dsn: {error}")))?;
        let options = config.connect_options();
        // Open one connection now so a bad host, password or database fails on the request
        // that named it (with the `connect:` prefix `dl_api` classifies), not 30 s later on
        // the first query inside the pool's acquire timeout. The pool then grows lazily.
        let probe = tokio::time::timeout(CONNECT_TIMEOUT, MySqlConnection::connect_with(&options))
            .await
            .map_err(|_| ExecutorError::new("connect: timed out after 10 s"))?
            .map_err(|error| ExecutorError::new(describe_connect(&error)))?;
        probe
            .close()
            .await
            .map_err(|error| ExecutorError::new(describe_connect(&error)))?;
        let pool = pool_options(self.max_connections).connect_lazy_with(options);
        pools.insert(dsn.to_string(), pool.clone());
        Ok(pool)
    }

    /// Databases this executor has opened a pool for.
    pub async fn len(&self) -> usize {
        self.pools.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Run one statement on the database `dsn` names and return its rows as text.
    pub async fn query(
        &self,
        dsn: &str,
        sql: &str,
    ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
        let pool = self.pool(dsn).await?;
        let rows = sqlx::query(sql)
            .fetch_all(&pool)
            .await
            .map_err(|error| ExecutorError::new(describe_query(&error)))?;
        rows.iter()
            .map(|row| text_row(row).map_err(ExecutorError::new))
            .collect()
    }
}

impl std::fmt::Debug for MysqlExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MysqlExecutor")
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Executor for MysqlExecutor {
    async fn fetch(
        &self,
        sql: &str,
        target: Option<&str>,
    ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
        let Some(dsn) = target else {
            return Err(ExecutorError::new(
                "no MySQL connection: the dataset resolved to no database",
            ));
        };
        self.query(dsn, sql).await
    }

    /// MySQL streaming is not implemented yet: the exported/result path that uses
    /// `fetch_stream` is PostgreSQL-only in the server, so this is the trait's own answer.
    async fn fetch_stream(
        &self,
        _sql: &str,
        _target: Option<&str>,
    ) -> Result<RowStream, ExecutorError> {
        Err(ExecutorError::new(
            "MySQL streaming is not implemented by this executor",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::error::Error as StdError;

    use super::*;

    #[test]
    fn the_type_mapping_follows_the_python_type_transformer() {
        let cases = [
            ("BOOLEAN", Some(CellKind::Bool)),
            ("BIT", Some(CellKind::Bool)),
            ("TINYINT", Some(CellKind::Int)),
            ("BIGINT", Some(CellKind::Int)),
            ("BIGINT UNSIGNED", Some(CellKind::UInt)),
            ("FLOAT", Some(CellKind::Float)),
            ("DOUBLE", Some(CellKind::Float)),
            ("DECIMAL", Some(CellKind::Decimal)),
            ("CHAR", Some(CellKind::Text)),
            ("VARCHAR", Some(CellKind::Text)),
            ("TEXT", Some(CellKind::Text)),
            ("ENUM", Some(CellKind::Text)),
            ("SET", Some(CellKind::Set)),
            ("BINARY", Some(CellKind::Bytes)),
            ("BLOB", Some(CellKind::Bytes)),
            ("GEOMETRY", Some(CellKind::Bytes)),
            ("DATE", Some(CellKind::Date)),
            ("DATETIME", Some(CellKind::DateTime)),
            ("TIMESTAMP", Some(CellKind::DateTime)),
            ("TIME", Some(CellKind::Time)),
            ("YEAR", Some(CellKind::Year)),
            ("JSON", Some(CellKind::Json)),
            ("GEOMETRYCOLLECTION", None),
        ];
        for (name, expected) in cases {
            assert_eq!(cell_kind(name), expected, "type: {name}");
        }
    }

    /// A server-side error: the code and message survive into the executor's text.
    #[derive(Debug)]
    struct FakeDbError {
        code: &'static str,
        message: &'static str,
    }

    impl std::fmt::Display for FakeDbError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.message)
        }
    }

    impl StdError for FakeDbError {}

    impl sqlx::error::DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            self.message
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code))
        }

        fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn database_error(code: &'static str, message: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError { code, message }))
    }

    #[test]
    fn a_missing_table_keeps_the_mysql_message_and_code() {
        let error = database_error("1146", "Table 'db.t' doesn't exist");
        let text = describe_query(&error);
        assert_eq!(text, "query: Table 'db.t' doesn't exist (MySQL 1146)");
        // `dl_api::error` turns "doesn't exist" into SOURCE_DOES_NOT_EXIST.
        assert!(text.to_ascii_lowercase().contains("doesn't exist"));
    }

    #[test]
    fn a_syntax_error_is_marked_as_one() {
        let error = database_error(
            "1064",
            "You have an error in your SQL syntax; check the manual ...",
        );
        let text = describe_query(&error);
        assert!(text.starts_with("query: syntax error: "), "{text}");
        assert!(text.contains("(MySQL 1064)"), "{text}");
    }

    #[test]
    fn a_connect_error_keeps_the_prefix_dl_api_classifies() {
        let error = sqlx::Error::Configuration("bad option".into());
        assert!(describe_connect(&error).starts_with("connect: "));
    }

    /// An unreachable server is a clean `connect:` error, not a panic or a hang.
    #[tokio::test]
    async fn an_unreachable_database_is_a_connect_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let executor = MysqlExecutor::new();
        let error = executor
            .fetch(
                "SELECT 1",
                Some(&format!("mysql://user:pass@127.0.0.1:{port}/app")),
            )
            .await
            .unwrap_err();
        assert!(error.message.starts_with("connect: "), "{}", error.message);
        assert!(
            error.message.to_ascii_lowercase().contains("refused"),
            "{}",
            error.message
        );
        assert_eq!(
            executor.len().await,
            0,
            "a failed connect must not be cached"
        );
    }

    #[tokio::test]
    async fn a_bad_dsn_is_a_dsn_error_not_a_panic() {
        let executor = MysqlExecutor::new();
        let error = executor
            .fetch("SELECT 1", Some("postgres://host/db"))
            .await
            .unwrap_err();
        assert!(
            error.message.starts_with("mysql dsn: "),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn no_target_is_a_clear_error() {
        let executor = MysqlExecutor::new();
        let error = executor.fetch("SELECT 1", None).await.unwrap_err();
        assert!(
            error.message.contains("no MySQL connection"),
            "{}",
            error.message
        );
    }
}
