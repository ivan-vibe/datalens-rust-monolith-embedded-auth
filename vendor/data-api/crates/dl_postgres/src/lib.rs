//! PostgreSQL executor: the first real database behind the Rust data-api.
//!
//! `simple_query` returns every value as text, which is exactly the boundary `dl_api`
//! expects: the dataset already knows the column types, so decoding happens once, above
//! this crate. It also keeps the executor free of per-type `FromSql` plumbing.
//!
//! [`PostgresPool`] keeps a pool per DSN: a data-api request names its database through the
//! dataset's `connection` entry, so the service opens as many pools as there are distinct
//! connections — and up to [`DEFAULT_POOL_SIZE`] connections per pool, because one connection
//! per database serialises every chart of a dashboard behind a single `Mutex`.
//!
//! Measured on the demo dashboard: with one connection per DSN,
//! four concurrent requests each waited for the others' query, so the median went from 17 ms to
//! 65 ms while the throughput stayed flat; a pool keeps both flat and rising.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dl_api::{Executor, ExecutorError, QueryMetadata, RowStream};
use futures::SinkExt;
use rustls::RootCertStore;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, SimpleQueryMessage};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Connections one DSN may use at the same time.
///
/// A dashboard opens a dozen charts at once and every chart runs two queries (its own block and
/// the totals block), so a pool of one queues them all. The number stays small on purpose: the
/// container's PostgreSQL allows 512 connections and several data-api processes talk to it.
pub const DEFAULT_POOL_SIZE: usize = 8;

/// How long a request may wait for a free connection before it fails.
///
/// The pool used to wait forever: once all permits were held by slow queries, every later
/// request queued without a bound and the queue looked like a hang, not like a saturated pool.
fn pool_wait() -> Duration {
    static WAIT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *WAIT.get_or_init(|| {
        seconds("DL_PG_POOL_WAIT_SECS", 30)
    })
}

fn connect_timeout() -> Duration {
    static TIMEOUT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| seconds("DL_PG_CONNECT_TIMEOUT_SECS", 10))
}

/// Server-side deadline for one statement, so a query abandoned by the client stops running on
/// the database as well. `0` disables it.
fn statement_timeout_ms() -> u64 {
    static TIMEOUT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        std::env::var("DL_PG_STATEMENT_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(180_000)
    })
}

fn seconds(key: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(key)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .unwrap_or(default),
    )
}

/// Takes one pool slot, but not forever: a saturated pool is an error the caller can report,
/// not a request that hangs until the client gives up.
async fn acquire_slot<'a>(
    slots: &'a Semaphore,
    wait: Duration,
) -> Result<tokio::sync::SemaphorePermit<'a>, ExecutorError> {
    match tokio::time::timeout(wait, slots.acquire()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(error)) => Err(ExecutorError::new(format!("pool closed: {error}"))),
        Err(_) => Err(ExecutorError::new(format!(
            "pool wait timed out after {}s: every connection is busy",
            wait.as_secs()
        ))),
    }
}

/// The text of a driver error, with what `Display` leaves out.
///
/// `tokio_postgres::Error`'s `Display` is just `db error` for a server-side error: the
/// SQLSTATE, the server message and its detail/hint live in the `DbError`. Without them the
/// `500` this executor produces is impossible to diagnose from a response body or a log line.
fn describe(error: &tokio_postgres::Error) -> String {
    let Some(db) = error.as_db_error() else {
        return format!("query: {error}");
    };
    let mut text = format!(
        "query: {}: {} (SQLSTATE {})",
        db.severity(),
        db.message(),
        db.code().code()
    );
    if let Some(detail) = db.detail() {
        text.push_str(&format!("; detail: {detail}"));
    }
    if let Some(hint) = db.hint() {
        text.push_str(&format!("; hint: {hint}"));
    }
    text
}

/// Open one connection and let its driver task run in the background.
///
/// `jit` is left on, but its *cost thresholds* are raised: PostgreSQL switches the LLVM JIT
/// on by estimated cost, and a chart query's estimate can be in the millions (a
/// `count(DISTINCT ...)` window over a custom-SQL source) although it runs on a few thousand
/// rows, so the stock threshold compiles expressions for hundreds of milliseconds to run them
/// once. The cohort retention chart measured 770–890 ms with JIT against 64–73 ms without it.
/// Plans that are genuinely huge still get JIT; `DL_PG_JIT=off`/`on` change the behaviour.
/// The CA bundle used for PostgreSQL TLS: `DL_PG_CA_FILE`, then the shared `DL_CA_FILE`,
/// `SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE`. When unset, only the compiled-in webpki roots are
/// used (public CAs).
fn ca_file() -> Option<String> {
    [
        "DL_PG_CA_FILE",
        "DL_CA_FILE",
        "SSL_CERT_FILE",
        "REQUESTS_CA_BUNDLE",
    ]
    .iter()
    .find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

/// A rustls connector: webpki roots plus the optional private CA bundle.
///
/// `tokio-postgres` only parses `sslmode=disable|prefer|require`; rustls performs the normal
/// chain and hostname verification, so `require` is the secure mode. A DSN with
/// `sslmode=verify-full` is normalised to `require` by [`open`].
fn tls_connector() -> Result<MakeRustlsConnect, ExecutorError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = ca_file() {
        let file = std::fs::File::open(&path).map_err(|error| {
            ExecutorError::new(format!(
                "cannot read the PostgreSQL CA bundle {path}: {error}"
            ))
        })?;
        let mut reader = std::io::BufReader::new(file);
        let certificates = rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ExecutorError::new(format!("cannot parse {path}: {error}")))?;
        if certificates.is_empty() {
            return Err(ExecutorError::new(format!(
                "the PostgreSQL CA bundle {path} contains no certificates"
            )));
        }
        for certificate in certificates {
            roots.add(certificate).map_err(|error| {
                ExecutorError::new(format!("cannot add a certificate from {path}: {error}"))
            })?;
        }
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

async fn open(dsn: &str) -> Result<Client, ExecutorError> {
    // `tokio-postgres` rejects `verify-full`/`verify-ca`; rustls verifies the chain and the
    // hostname anyway, so `require` is the equivalent supported spelling.
    let dsn = dsn
        .replace("sslmode=verify-full", "sslmode=require")
        .replace("sslmode=verify-ca", "sslmode=require");
    let mut config: tokio_postgres::Config = dsn
        .parse()
        .map_err(|error| ExecutorError::new(format!("dsn: {error}")))?;
    // The server turns JIT on at a plan cost of 100k and inlines at 500k. A chart query's
    // estimate can be inflated into the millions by nested joins over a custom-SQL source (the
    // cohort pivot estimated 4.17M for 12 031 rows), so the stock thresholds fire JIT where it
    // cannot pay for itself. Raising them keeps JIT for genuinely huge plans without the
    // false positive; `DL_PG_JIT=off` disables it outright, `DL_PG_JIT=on` restores the stock
    // thresholds.
    let mut options = String::new();
    match std::env::var("DL_PG_JIT").as_deref() {
        Ok("off") | Ok("0") => options.push_str("-c jit=off"),
        Ok("on") | Ok("1") => {}
        _ => options.push_str(
            "-c jit_above_cost=50000000 -c jit_inline_above_cost=500000000 \
             -c jit_optimize_above_cost=500000000",
        ),
    }
    // A deadline on the server, not only on the client: without it a statement keeps running
    // (and holding locks) after the request that asked for it has timed out or disconnected.
    if statement_timeout_ms() > 0 {
        options.push_str(&format!(" -c statement_timeout={}", statement_timeout_ms()));
    }
    config.options(options.trim());
    let connect = tokio::time::timeout(connect_timeout(), config.connect(tls_connector()?));
    let (client, connection) = match connect.await {
        Ok(result) => result.map_err(|error| ExecutorError::new(format!("connect: {error}")))?,
        Err(_) => {
            return Err(ExecutorError::new(format!(
                "connect timed out after {}s",
                connect_timeout().as_secs()
            )))
        }
    };
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("postgres connection error: {error}");
        }
    });
    Ok(client)
}

/// A PostgreSQL `numeric` decoded from its binary representation.
///
/// `tokio-postgres` 0.7 has no `rust_decimal` feature, so the type is decoded here and rendered
/// as the decimal string `dl_api` already expects.
struct PgNumeric(String);

impl std::fmt::Display for PgNumeric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'a> tokio_postgres::types::FromSql<'a> for PgNumeric {
    fn from_sql(
        _ty: &tokio_postgres::types::Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 8 {
            return Err("numeric: short header".into());
        }
        let ndigits = i16::from_be_bytes([raw[0], raw[1]]);
        let weight = i16::from_be_bytes([raw[2], raw[3]]);
        let sign = u16::from_be_bytes([raw[4], raw[5]]);
        let dscale = u16::from_be_bytes([raw[6], raw[7]]) as usize;
        if ndigits < 0 {
            return Err("numeric: negative digit count".into());
        }
        let ndigits = ndigits as usize;
        if raw.len() < 8 + ndigits * 2 {
            return Err("numeric: truncated digits".into());
        }
        let digits: Vec<u16> = (0..ndigits)
            .map(|i| u16::from_be_bytes([raw[8 + i * 2], raw[9 + i * 2]]))
            .collect();
        if sign == 0xC000 {
            return Ok(Self("NaN".to_string()));
        }
        let negative = sign == 0x4000;
        let mut integer = String::new();
        let mut fraction = String::new();
        for (index, digit) in digits.iter().enumerate() {
            if (index as i32) <= weight as i32 {
                if integer.is_empty() {
                    integer.push_str(&digit.to_string());
                } else {
                    integer.push_str(&format!("{digit:04}"));
                }
            } else {
                fraction.push_str(&format!("{digit:04}"));
            }
        }
        // Weight below zero means leading fractional zero groups: 0.0012 has weight -1.
        if weight < 0 {
            let zeros = (-weight as usize - 1) * 4;
            fraction = format!("{}{}", "0".repeat(zeros), fraction);
        }
        if integer.is_empty() {
            integer.push('0');
        }
        fraction.truncate(dscale);
        while fraction.len() < dscale {
            fraction.push('0');
        }
        let mut text = if negative {
            format!("-{integer}")
        } else {
            integer
        };
        if dscale > 0 {
            text.push('.');
            text.push_str(&fraction);
        }
        Ok(Self(text))
    }

    fn accepts(ty: &tokio_postgres::types::Type) -> bool {
        *ty == tokio_postgres::types::Type::NUMERIC
    }
}

/// Convert an extended-protocol binary row to the text cells `dl_api` decodes.
fn binary_row(row: &tokio_postgres::Row) -> Result<Vec<Option<String>>, String> {
    use tokio_postgres::types::Type;
    let mut values = Vec::with_capacity(row.len());
    for (index, column) in row.columns().iter().enumerate() {
        let value = match *column.type_() {
            Type::BOOL => row
                .try_get::<_, Option<bool>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_string()),
            Type::INT2 => row
                .try_get::<_, Option<i16>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_string()),
            Type::INT4 => row
                .try_get::<_, Option<i32>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_string()),
            Type::INT8 => row
                .try_get::<_, Option<i64>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_string()),
            Type::OID => row
                .try_get::<_, Option<u32>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_string()),
            Type::FLOAT4 => row
                .try_get::<_, Option<f32>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_string()),
            Type::FLOAT8 => row
                .try_get::<_, Option<f64>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_string()),
            Type::NUMERIC => row
                .try_get::<_, Option<PgNumeric>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_string()),
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => row
                .try_get::<_, Option<String>>(index)
                .map_err(|e| e.to_string())?,
            Type::DATE => row
                .try_get::<_, Option<chrono::NaiveDate>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.format("%Y-%m-%d").to_string()),
            Type::TIMESTAMP => row
                .try_get::<_, Option<chrono::NaiveDateTime>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.format("%Y-%m-%dT%H:%M:%S").to_string()),
            Type::TIMESTAMPTZ => row
                .try_get::<_, Option<chrono::DateTime<chrono::Utc>>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_rfc3339()),
            Type::JSON | Type::JSONB => row
                .try_get::<_, Option<serde_json::Value>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_string()),
            Type::UUID => row
                .try_get::<_, Option<uuid::Uuid>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.to_string()),
            Type::TEXT_ARRAY | Type::VARCHAR_ARRAY | Type::BPCHAR_ARRAY => row
                .try_get::<_, Option<Vec<Option<String>>>>(index)
                .map_err(|e| e.to_string())?
                .map(|values| {
                    let rendered: Vec<String> = values
                        .iter()
                        .map(|value| match value {
                            Some(value) => {
                                format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
                            }
                            None => "NULL".to_string(),
                        })
                        .collect();
                    format!("{{{}}}", rendered.join(","))
                }),
            Type::BYTEA => row
                .try_get::<_, Option<Vec<u8>>>(index)
                .map_err(|e| e.to_string())?
                .map(|v| v.iter().map(|b| format!("{b:02x}")).collect()),
            ref other => return Err(format!("unsupported PostgreSQL type {other}")),
        };
        values.push(value);
    }
    Ok(values)
}

async fn query_binary(client: &Client, sql: &str) -> Result<Vec<Vec<Option<String>>>, String> {
    let rows = client
        .query(sql, &[])
        .await
        .map_err(|error| describe(&error))?;
    rows.iter().map(binary_row).collect()
}

/// Whether an error means the pooled connection is gone (server restart, idle timeout).
///
/// `tokio-postgres` sets `is_closed` for some of these, but a plain `connection closed` from the
/// simple-query stream is not flagged, so the message is checked as well. The query never ran
/// when the connection was already gone, which is what makes one retry safe.
fn connection_gone(error: &tokio_postgres::Error) -> bool {
    if error.is_closed() {
        return true;
    }
    let text = error.to_string().to_ascii_lowercase();
    text.contains("connection closed") || text.contains("terminating connection")
}

/// The native type names of one statement, as `tokio_postgres::Type::name` spells them
/// (`int4`, `float8`, `text`, `timestamp`).
///
/// The simple-query protocol exposes column names but not type OIDs, so this asks the server
/// to describe the statement (`client.prepare`). It is only used by `dashsql`, which is not the
/// chart hot path; a statement `prepare` rejects (several SQL statements in one string, for
/// example) still runs, with empty types, the way the text path always did.
async fn column_types(client: &Client, sql: &str) -> Vec<String> {
    match client.prepare(sql).await {
        Ok(statement) => statement
            .columns()
            .iter()
            .map(|column| column.type_().name().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// One SQL statement over a borrowed connection.
///
/// Kept apart from [`Executor::fetch`] so the pool can tell "the database refused this query"
/// (the connection is still good) from "the connection went away" (drop it, open another one
/// next time).
///
/// `simple_query_raw` is the streaming form: `simple_query` collects every message of the result
/// first and then this would copy them into rows, so a large answer sat in memory twice. Here the
/// driver hands one row over at a time and only the rows themselves are kept.
async fn query(
    client: &Client,
    sql: &str,
) -> Result<(Vec<Vec<Option<String>>>, Vec<String>), tokio_postgres::Error> {
    use futures::TryStreamExt;

    let messages = client.simple_query_raw(sql).await?;
    let mut messages = std::pin::pin!(messages);
    let mut rows = Vec::new();
    let mut names: Vec<String> = Vec::new();
    while let Some(message) = messages.try_next().await? {
        match message {
            SimpleQueryMessage::Row(row) => {
                let mut values = Vec::with_capacity(row.len());
                for index in 0..row.len() {
                    values.push(row.get(index).map(|value| value.to_string()));
                }
                rows.push(values);
            }
            // `RowDescription` is what `cursor.description` carries in psycopg2. The
            // simple-query protocol sends the names only; the type OIDs are not exposed by
            // `tokio-postgres`' `SimpleColumn`, so `db_types` stays empty on this path.
            SimpleQueryMessage::RowDescription(columns) => {
                names = columns
                    .iter()
                    .map(|column| column.name().to_string())
                    .collect();
            }
            SimpleQueryMessage::CommandComplete(_) => {}
            _ => {}
        }
    }
    Ok((rows, names))
}

/// A bounded pool of connections to one database.
pub struct PostgresExecutor {
    dsn: String,
    /// Connections that are not in use. A permit holder pops one, or opens a new connection
    /// while the pool is still growing.
    idle: Mutex<Vec<Client>>,
    /// One permit per connection this pool may hold: the pool's actual bound.
    slots: Semaphore,
}

impl PostgresExecutor {
    /// A pool of one connection, opened now, so a bad DSN fails here and not on the first query.
    pub async fn connect(dsn: &str) -> Result<Self, ExecutorError> {
        Self::connect_with_size(dsn, 1).await
    }

    /// A pool of at most `size` connections, with the first one opened now.
    pub async fn connect_with_size(dsn: &str, size: usize) -> Result<Self, ExecutorError> {
        let executor = Self {
            dsn: dsn.to_string(),
            idle: Mutex::new(Vec::new()),
            slots: Semaphore::new(size.max(1)),
        };
        let client = open(dsn).await?;
        executor.idle.lock().await.push(client);
        Ok(executor)
    }

    /// Connections that are open and waiting for work.
    pub async fn idle(&self) -> usize {
        self.idle.lock().await.len()
    }
}

#[async_trait]
impl Executor for PostgresExecutor {
    /// `target` is ignored: this pool *is* one database.
    async fn fetch(
        &self,
        sql: &str,
        _target: Option<&str>,
    ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
        // The permit is what queues a request once every connection is busy. It is held until the
        // whole call is done, retry included, so the pool never hands out more than its bound.
        let _permit = acquire_slot(&self.slots, pool_wait()).await?;
        let mut retried = false;
        loop {
            let (client, reused) = match self.idle.lock().await.pop() {
                Some(client) => (client, true),
                // Every permit is taken and none of them is idle: the pool is still growing.
                None => (open(&self.dsn).await?, false),
            };
            let binary = std::env::var("DL_PG_BINARY")
                .as_deref()
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            if binary {
                match query_binary(&client, sql).await {
                    Ok(rows) => {
                        self.idle.lock().await.push(client);
                        return Ok(rows);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "binary PostgreSQL decode failed, falling back to text");
                    }
                }
            }
            match query(&client, sql).await {
                Ok((rows, _names)) => {
                    self.idle.lock().await.push(client);
                    return Ok(rows);
                }
                // A pooled connection can be closed while it waits (a server restart, an idle
                // timeout). That one is dropped and replaced once: the query never ran.
                Err(error) if connection_gone(&error) && reused && !retried => {
                    // A server restart closes every idle connection this pool holds. Drop the
                    // rest as well instead of handing out another corpse and failing the next
                    // request; the loop then opens a fresh connection.
                    self.idle.lock().await.clear();
                    tracing::warn!(error = %error, "pooled connections were closed, reconnecting");
                    retried = true;
                }
                Err(error) => {
                    // The database refused this query, not the connection: keep it.
                    if !connection_gone(&error) {
                        self.idle.lock().await.push(client);
                    }
                    return Err(ExecutorError::new(describe(&error)));
                }
            }
        }
    }

    /// Text rows plus the `RowDescription` column names of the last statement.
    ///
    /// Used by `dashsql` and `typed_query`, which serialize the cursor metadata. The binary
    /// path of [`Executor::fetch`] is not repeated here: those two endpoints are not the chart
    /// hot path, and `simple_query` already returns every value as text.
    async fn fetch_with_meta(
        &self,
        sql: &str,
        _target: Option<&str>,
    ) -> Result<(Vec<Vec<Option<String>>>, QueryMetadata), ExecutorError> {
        let _permit = acquire_slot(&self.slots, pool_wait()).await?;
        let mut retried = false;
        loop {
            let (client, reused) = match self.idle.lock().await.pop() {
                Some(client) => (client, true),
                None => (open(&self.dsn).await?, false),
            };
            match query(&client, sql).await {
                Ok((rows, names)) => {
                    let db_types = column_types(&client, sql).await;
                    self.idle.lock().await.push(client);
                    return Ok((rows, QueryMetadata::new(names, db_types)));
                }
                Err(error) if connection_gone(&error) && reused && !retried => {
                    self.idle.lock().await.clear();
                    tracing::warn!(error = %error, "pooled connections were closed, reconnecting");
                    retried = true;
                }
                Err(error) => {
                    if !connection_gone(&error) {
                        self.idle.lock().await.push(client);
                    }
                    return Err(ExecutorError::new(describe(&error)));
                }
            }
        }
    }

    async fn fetch_stream(
        &self,
        sql: &str,
        _target: Option<&str>,
    ) -> Result<RowStream, ExecutorError> {
        // A stream owns its connection for its whole lifetime. Opening a dedicated connection
        // here keeps the borrow of `query_raw` inside the task; a pooled connection cannot be
        // returned while the cursor still borrows it.
        let client = open(&self.dsn).await?;
        let (mut sender, receiver) = futures::channel::mpsc::channel(64);
        let sql = sql.to_string();
        tokio::spawn(async move {
            use futures::TryStreamExt;
            let params = std::iter::empty::<&(dyn ToSql + Sync)>();
            let rows = match client.query_raw(&sql, params).await {
                Ok(rows) => rows,
                Err(error) => {
                    let _ = sender.send(Err(ExecutorError::new(describe(&error)))).await;
                    return;
                }
            };
            let mut rows = std::pin::pin!(rows);
            let mut sender = sender;
            loop {
                match rows.try_next().await {
                    Ok(Some(row)) => {
                        let cells = match binary_row(&row) {
                            Ok(cells) => cells,
                            Err(error) => {
                                let _ = sender
                                    .send(Err(ExecutorError::new(format!("binary row: {error}"))))
                                    .await;
                                return;
                            }
                        };
                        if sender.send(Ok(cells)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        let _ = sender.send(Err(ExecutorError::new(describe(&error)))).await;
                        return;
                    }
                }
            }
        });
        Ok(Box::pin(receiver))
    }
}

/// One pool per DSN, created on first use.
///
/// The DSN comes from the dataset's `connection` entry (`dl_us::UsConnectionCache`), so it
/// already carries the user, the password and the database of that connection.
pub struct PostgresPool {
    clients: RwLock<HashMap<String, Arc<PostgresExecutor>>>,
    max_connections: usize,
}

impl Default for PostgresPool {
    fn default() -> Self {
        Self::with_max_connections(DEFAULT_POOL_SIZE)
    }
}

impl PostgresPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// A pool that opens at most `max_connections` connections per database
    /// (`DL_DB_POOL_SIZE` in the server).
    pub fn with_max_connections(max_connections: usize) -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
            max_connections: max_connections.max(1),
        }
    }

    pub fn max_connections(&self) -> usize {
        self.max_connections
    }

    /// The executor for `dsn`, connecting if this database has not been used yet.
    pub async fn executor(&self, dsn: &str) -> Result<Arc<PostgresExecutor>, ExecutorError> {
        if let Some(executor) = self.clients.read().await.get(dsn) {
            return Ok(executor.clone());
        }
        // The write lock is held across the first `connect` on purpose: two simultaneous
        // requests for a new database would otherwise open two pools and leak one.
        let mut clients = self.clients.write().await;
        if let Some(executor) = clients.get(dsn) {
            return Ok(executor.clone());
        }
        let executor =
            Arc::new(PostgresExecutor::connect_with_size(dsn, self.max_connections).await?);
        clients.insert(dsn.to_string(), executor.clone());
        Ok(executor)
    }

    pub async fn len(&self) -> usize {
        self.clients.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

impl std::fmt::Debug for PostgresPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostgresPool")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only when a PostgreSQL is reachable, so `cargo test` stays hermetic.
    #[tokio::test]
    async fn executes_a_trivial_query_when_a_database_is_configured() {
        let dsn = match std::env::var("DL_TEST_POSTGRES_DSN") {
            Ok(dsn) => dsn,
            Err(_) => return,
        };
        let executor = PostgresExecutor::connect(&dsn)
            .await
            .expect("connect to DL_TEST_POSTGRES_DSN");
        let rows = executor
            .fetch("SELECT 1 AS one, 'x' AS two", None)
            .await
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![Some("1".to_string()), Some("x".to_string())]]
        );
    }


    /// The server-side deadline is what stops a query after the client that asked for it is
    /// gone. Runs only when the deadline is configured short (`DL_PG_STATEMENT_TIMEOUT_MS`),
    /// because the default is deliberately generous.
    #[tokio::test]
    async fn a_configured_statement_deadline_stops_a_slow_query() {
        let configured = match std::env::var("DL_PG_STATEMENT_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(ms) if ms > 0 && ms <= 5_000 => ms,
            _ => return,
        };
        let dsn = match std::env::var("DL_TEST_POSTGRES_DSN") {
            Ok(dsn) => dsn,
            Err(_) => return,
        };
        let executor = PostgresExecutor::connect(&dsn).await.expect("connect");
        // Longer than the configured statement deadline, shorter than a CI timeout.
        let sleep = format!("SELECT pg_sleep({})", (configured as f64 / 1000.0) + 1.0);
        let error = match executor.fetch(&sleep, None).await {
            Ok(_) => panic!("a query over the statement deadline must fail"),
            Err(error) => error,
        };
        assert!(
            error.message.contains("statement timeout") || error.message.contains("canceling statement"),
            "unexpected error: {}",
            error.message
        );
    }

    /// A saturated pool must fail fast: waiting forever is what made a slow database look
    /// like a hung server.
    #[tokio::test]
    async fn a_saturated_pool_fails_instead_of_waiting_forever() {
        let slots = Semaphore::new(0);
        let error = match acquire_slot(&slots, Duration::from_millis(30)).await {
            Ok(_) => panic!("expected a pool-wait timeout"),
            Err(error) => error,
        };
        assert!(
            error.message.contains("pool wait timed out"),
            "unexpected error: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_free_slot_is_taken_and_returned() {
        let slots = Semaphore::new(1);
        let permit = acquire_slot(&slots, Duration::from_secs(1))
            .await
            .expect("a free slot");
        assert_eq!(slots.available_permits(), 0);
        drop(permit);
        assert_eq!(slots.available_permits(), 1);
    }

    /// The pool's reason to exist: two queries over it overlap instead of queueing.
    ///
    /// `pg_sleep` makes the difference impossible to miss: one connection needs 400 ms for two
    /// 200 ms queries, two connections need about 200 ms.
    #[tokio::test]
    async fn two_queries_over_a_pool_of_two_overlap() {
        let dsn = match std::env::var("DL_TEST_POSTGRES_DSN") {
            Ok(dsn) => dsn,
            Err(_) => return,
        };
        let executor = PostgresExecutor::connect_with_size(&dsn, 2)
            .await
            .expect("connect to DL_TEST_POSTGRES_DSN");
        let started = std::time::Instant::now();
        let (left, right) = tokio::join!(
            executor.fetch("SELECT pg_sleep(0.2)", None),
            executor.fetch("SELECT pg_sleep(0.2)", None),
        );
        left.unwrap();
        right.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_millis() < 350,
            "two 200 ms queries took {elapsed:?}: the pool is serialising them"
        );
        assert_eq!(executor.idle().await, 2, "both connections should be back");
    }
}
