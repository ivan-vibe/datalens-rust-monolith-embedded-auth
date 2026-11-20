//! The executor the server runs against: a pooled PostgreSQL or a dry run.

use std::sync::Arc;

use async_trait::async_trait;
use dl_api::{Executor, ExecutorError, QueryMetadata, RowStream};
use dl_clickhouse::ClickHouseExecutor;
use dl_mysql::MysqlExecutor;
use dl_postgres::PostgresPool;

/// Which database the request goes to.
///
/// The DSN is not a server-wide setting any more: each dataset names its `connection` entry,
/// the service resolves it (`ConnectionResolver`) and passes the DSN down as the executor's
/// `target`. `default_dsn` is the `DL_DATABASE_URL` fallback for datasets whose connection
/// cannot be resolved. `DryRun` compiles SQL and returns no rows; `Static` answers with a fixed
/// row set, so the HTTP contract is testable without a database.
#[derive(Clone)]
pub enum AnyExecutor {
    Postgres {
        pool: Arc<PostgresPool>,
        default_dsn: Option<String>,
    },
    /// Every database the port can reach: the resolved DSN's scheme picks the executor, so one
    /// process serves PostgreSQL, ClickHouse and MySQL datasets side by side.
    Databases {
        postgres: Arc<PostgresPool>,
        clickhouse: Arc<ClickHouseExecutor>,
        mysql: Arc<MysqlExecutor>,
        default_dsn: Option<String>,
    },
    DryRun,
    Static(Arc<Vec<Vec<Option<String>>>>),
}

/// The scheme `dl_us::ConnectorKind::ClickHouse` puts in front of a ClickHouse DSN.
fn is_clickhouse(dsn: &str) -> bool {
    dsn.starts_with("clickhouse://") || dsn.starts_with("clickhouses://")
}

/// The scheme `dl_us::ConnectorKind::MySql` puts in front of a MySQL DSN.
fn is_mysql(dsn: &str) -> bool {
    dsn.starts_with("mysql://") || dsn.starts_with("mysqls://")
}

impl AnyExecutor {
    /// The pooled executor with an optional fallback DSN (`DL_DATABASE_URL`).
    pub fn postgres(default_dsn: Option<String>) -> Self {
        Self::postgres_with_pool(default_dsn, dl_postgres::DEFAULT_POOL_SIZE)
    }

    /// The same, with at most `max_connections` connections per database (`DL_DB_POOL_SIZE`).
    ///
    /// One connection per database is what a dashboard saturates first: every chart runs two
    /// queries, and a single connection makes them queue behind each other.
    pub fn postgres_with_pool(default_dsn: Option<String>, max_connections: usize) -> Self {
        Self::Postgres {
            pool: Arc::new(PostgresPool::with_max_connections(max_connections)),
            default_dsn,
        }
    }

    /// PostgreSQL plus ClickHouse plus MySQL: the container's executor.
    pub fn databases(default_dsn: Option<String>, max_connections: usize) -> Self {
        Self::Databases {
            postgres: Arc::new(PostgresPool::with_max_connections(max_connections)),
            clickhouse: Arc::new(ClickHouseExecutor::new()),
            mysql: Arc::new(MysqlExecutor::with_max_connections(max_connections)),
            default_dsn,
        }
    }
}

#[async_trait]
impl Executor for AnyExecutor {
    async fn fetch(
        &self,
        sql: &str,
        target: Option<&str>,
    ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
        match self {
            Self::Postgres { pool, default_dsn } => match target.or(default_dsn.as_deref()) {
                Some(dsn) if is_clickhouse(dsn) => Err(ExecutorError::new(
                    "the dataset is on ClickHouse, but this service was built without the \
                     ClickHouse executor",
                )),
                Some(dsn) if is_mysql(dsn) => Err(ExecutorError::new(
                    "the dataset is on MySQL, but this service was built without the MySQL \
                     executor",
                )),
                Some(dsn) => pool.executor(dsn).await?.fetch(sql, None).await,
                None => {
                    tracing::warn!(
                        "no connection for this dataset and no DL_DATABASE_URL: \
                         answering without rows"
                    );
                    Ok(Vec::new())
                }
            },
            Self::Databases {
                postgres,
                clickhouse,
                mysql,
                default_dsn,
            } => match target.or(default_dsn.as_deref()) {
                Some(dsn) if is_clickhouse(&dsn) => clickhouse.fetch(sql, Some(&dsn)).await,
                Some(dsn) if is_mysql(&dsn) => mysql.fetch(sql, Some(&dsn)).await,
                Some(dsn) => postgres.executor(&dsn).await?.fetch(sql, None).await,
                None => {
                    tracing::warn!(
                        "no connection for this dataset and no DL_DATABASE_URL: \
                         answering without rows"
                    );
                    Ok(Vec::new())
                }
            },
            Self::DryRun => Ok(Vec::new()),
            Self::Static(rows) => Ok(rows.as_ref().clone()),
        }
    }

    async fn fetch_with_meta(
        &self,
        sql: &str,
        target: Option<&str>,
    ) -> Result<(Vec<Vec<Option<String>>>, QueryMetadata), ExecutorError> {
        match self {
            Self::Postgres { pool, default_dsn } => match target.or(default_dsn.as_deref()) {
                Some(dsn) if is_clickhouse(dsn) || is_mysql(dsn) => Err(ExecutorError::new(
                    "this service was built without the executor for that connector",
                )),
                Some(dsn) => pool.executor(dsn).await?.fetch_with_meta(sql, None).await,
                None => Ok((Vec::new(), QueryMetadata::default())),
            },
            Self::Databases {
                postgres,
                clickhouse,
                mysql,
                default_dsn,
            } => match target.or(default_dsn.as_deref()) {
                Some(dsn) if is_clickhouse(&dsn) => {
                    clickhouse.fetch_with_meta(sql, Some(&dsn)).await
                }
                Some(dsn) if is_mysql(&dsn) => mysql.fetch_with_meta(sql, Some(&dsn)).await,
                Some(dsn) => {
                    postgres
                        .executor(&dsn)
                        .await?
                        .fetch_with_meta(sql, None)
                        .await
                }
                None => Ok((Vec::new(), QueryMetadata::default())),
            },
            Self::DryRun => Ok((Vec::new(), QueryMetadata::default())),
            Self::Static(rows) => Ok((rows.as_ref().clone(), QueryMetadata::default())),
        }
    }

    async fn fetch_stream(
        &self,
        sql: &str,
        target: Option<&str>,
    ) -> Result<RowStream, ExecutorError> {
        match self {
            Self::Postgres { pool, default_dsn } => match target.or(default_dsn.as_deref()) {
                Some(dsn) if is_clickhouse(dsn) => Err(ExecutorError::new(
                    "the dataset is on ClickHouse, which has no cursor stream here",
                )),
                Some(dsn) if is_mysql(dsn) => Err(ExecutorError::new(
                    "the dataset is on MySQL, which has no cursor stream here",
                )),
                Some(dsn) => pool.executor(dsn).await?.fetch_stream(sql, None).await,
                None => Err(ExecutorError::new("no connection for the streaming query")),
            },
            Self::Databases {
                postgres,
                clickhouse: _,
                mysql: _,
                default_dsn,
            } => match target.or(default_dsn.as_deref()) {
                Some(dsn) if is_clickhouse(&dsn) => Err(ExecutorError::new(
                    "ClickHouse streaming is not implemented",
                )),
                Some(dsn) if is_mysql(&dsn) => Err(ExecutorError::new(
                    "MySQL streaming is not implemented by this executor",
                )),
                Some(dsn) => postgres.executor(&dsn).await?.fetch_stream(sql, None).await,
                None => Err(ExecutorError::new("no connection for the streaming query")),
            },
            Self::DryRun => Ok(Box::pin(futures::stream::empty())),
            Self::Static(rows) => {
                let rows = rows.as_ref().clone();
                Ok(Box::pin(futures::stream::iter(rows.into_iter().map(Ok))))
            }
        }
    }
}
