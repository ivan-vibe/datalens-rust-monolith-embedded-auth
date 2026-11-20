//! The database boundary.
//!
//! Rows are returned as text: the column types are already known from the dataset, so the
//! service decodes them into JSON without depending on a specific driver's type mapping.
//! That keeps the executor swappable (PostgreSQL today, ClickHouse/MySQL next) and makes
//! the whole request path testable with an in-memory executor.

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// A stream of raw text cells, one item per database row.
pub type RowStream = Pin<Box<dyn Stream<Item = Result<Vec<Option<String>>, ExecutorError>> + Send>>;

/// Cursor metadata of one result set: the column names and the native type names.
///
/// Python reads it from `raw_cursor_info` (`names` / `db_types`) and turns it into the
/// `metadata` event of `dashsql` and the typed `headers` of `typed_query`. The text-only
/// boundary keeps rows as text, so the names/types are the only driver facts carried up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryMetadata {
    /// Column names, aligned with the row cells.
    pub names: Vec<String>,
    /// Native (driver) type names, aligned with `names`; empty when the driver reports none.
    pub db_types: Vec<String>,
}

impl QueryMetadata {
    pub fn new(names: Vec<String>, db_types: Vec<String>) -> Self {
        Self { names, db_types }
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.db_types.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ExecutorError {
    pub message: String,
}

impl ExecutorError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Executes one SQL statement and returns its rows as text.
///
/// `target` is the DSN resolved from the dataset's `connection` entry. An executor that is
/// bound to its own database (the `DL_DATABASE_URL` mode, and every test double) ignores it;
/// the container's pooled executor uses it to pick the right database.
#[async_trait]
pub trait Executor: Send + Sync {
    async fn fetch(
        &self,
        sql: &str,
        target: Option<&str>,
    ) -> Result<Vec<Vec<Option<String>>>, ExecutorError>;

    /// Cursor-style rows. The default keeps executors that cannot stream working through
    /// [`Executor::fetch`]; PostgreSQL overrides it with `query_raw`.
    async fn fetch_stream(
        &self,
        _sql: &str,
        _target: Option<&str>,
    ) -> Result<RowStream, ExecutorError> {
        Err(ExecutorError::new(
            "streaming is not supported by this executor",
        ))
    }

    /// Rows plus the cursor metadata `dashsql`/`typed_query` serialize.
    ///
    /// The default answers [`Executor::fetch`] with no metadata, which is what a driver that
    /// reports none (and every test double) does. ClickHouse overrides it from its
    /// `JSONCompact` `meta` array, PostgreSQL from the simple-query `RowDescription`.
    async fn fetch_with_meta(
        &self,
        sql: &str,
        target: Option<&str>,
    ) -> Result<(Vec<Vec<Option<String>>>, QueryMetadata), ExecutorError> {
        Ok((self.fetch(sql, target).await?, QueryMetadata::default()))
    }
}

/// Resolves a `us` `connection` entry into a database DSN.
///
/// Kept as a trait so `dl_api` does not depend on `dl_us`: the server wires the `us` cache in
/// (`UsConnectionCache`), and the tests wire in a fixed table.
/// The DSN of a `us` connection plus the settings the data-api policy needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionTarget {
    pub dsn: String,
    /// `us` connection `data.cache_ttl_sec` ("Cache TTL in seconds"): `Some(0)` disables the
    /// query-result cache for this connection, `None` leaves the service fallback in charge.
    pub cache_ttl_sec: Option<i64>,
    /// `us` connection `data.raw_sql_level` (`dl_constants.enums.RawSQLLevel`). Missing means
    /// `off`, the storage default; a Custom SQL source is refused below that level.
    pub raw_sql_level: dl_core::RawSqlLevel,
    /// `us` connection `data.data_export_forbidden`: the connection forbids export.
    pub data_export_forbidden: bool,
    /// `allow_background_data_export_for_conn_type`: the connector supports background export.
    pub data_export_allowed_in_conn_type: bool,
}

impl ConnectionTarget {
    /// A target whose policy settings are the permissive defaults (`raw_sql_level=off`).
    pub fn new(dsn: impl Into<String>) -> Self {
        Self {
            dsn: dsn.into(),
            cache_ttl_sec: None,
            raw_sql_level: dl_core::RawSqlLevel::Off,
            data_export_forbidden: false,
            data_export_allowed_in_conn_type: true,
        }
    }
}

#[async_trait]
pub trait ConnectionResolver: Send + Sync {
    async fn resolve(&self, connection_id: &str) -> Option<String>;

    /// The DSN plus the per-connection cache TTL, resolved once. The default keeps resolvers
    /// that predate the TTL setting working (no per-connection override).
    async fn resolve_target(&self, connection_id: &str) -> Option<ConnectionTarget> {
        self.resolve(connection_id).await.map(ConnectionTarget::new)
    }
}

/// An executor that answers from a fixed table. Used by tests and by `--dry-run` servers.
#[derive(Debug, Clone, Default)]
pub struct StaticExecutor {
    pub rows: Vec<Vec<Option<String>>>,
}

impl StaticExecutor {
    pub fn new(rows: Vec<Vec<Option<String>>>) -> Self {
        Self { rows }
    }
}

#[async_trait]
impl Executor for StaticExecutor {
    async fn fetch(
        &self,
        _sql: &str,
        _target: Option<&str>,
    ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
        Ok(self.rows.clone())
    }

    async fn fetch_stream(
        &self,
        _sql: &str,
        _target: Option<&str>,
    ) -> Result<RowStream, ExecutorError> {
        let rows = self.rows.clone();
        Ok(Box::pin(futures::stream::iter(rows.into_iter().map(Ok))))
    }
}
