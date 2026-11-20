//! ClickHouse over its HTTP interface: the second real database behind the Rust data-api.
//!
//! The Python `dl_connector_clickhouse` talks to ClickHouse over HTTP too
//! (`POST http://host:port/?database=...&default_format=JSONCompact`, basic auth), so the port
//! uses the same door: no native-protocol client, no new socket protocol, and the same
//! `JSONCompact` document the Python adapter parses (`clickhouse_sqlalchemy.parsers`).
//!
//! The boundary is the one [`dl_api::Executor`] already has: rows come back as text. A
//! `JSONCompact` value is therefore converted to the text PostgreSQL would have produced for
//! the same cell (`123`, `2026-05-08 00:00:00`, `"text"`), and `dl_api::decode_value` applies the
//! dataset's own type mapping exactly as it does for PostgreSQL. A `NULL` stays `None`, an array
//! stays its JSON text (the array postprocessor parses it back).
//!
//! The DSN is not a server setting: `dl_us` resolves the dataset's `connection` entry into
//! `clickhouse://user:password@host:port/database` and the executor answers on that target, so
//! one process serves PostgreSQL and ClickHouse datasets at the same time.

use std::time::Duration;

use async_trait::async_trait;
use dl_api::{Executor, ExecutorError, QueryMetadata};
use serde_json::Value;

pub mod dsn;

pub use dsn::ClickHouseConfig;

/// How long one query may take, including the connection.
///
/// A dashboard chart is not a report: ClickHouse kills its own long queries through
/// `max_execution_time` when the connection sets one, and the HTTP client needs a ceiling of its
/// own so a silent server does not hold a request forever.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Root certificates for outbound HTTPS: the compiled-in webpki roots plus the PEM bundle
/// named by `DL_CA_FILE` (or `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE`), so a private CA works
/// in the `scratch` image without a rebuild.
fn with_ca_bundle(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let path = ["DL_CA_FILE", "SSL_CERT_FILE", "REQUESTS_CA_BUNDLE"]
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });
    let Some(path) = path else {
        return builder;
    };
    let pem = match std::fs::read(&path) {
        Ok(pem) => pem,
        Err(error) => {
            tracing::warn!(path, %error, "cannot read the CA bundle");
            return builder;
        }
    };
    let certificates = match reqwest::Certificate::from_pem_bundle(&pem) {
        Ok(certificates) => certificates,
        Err(error) => {
            tracing::warn!(path, %error, "cannot parse the CA bundle");
            return builder;
        }
    };
    let mut builder = builder;
    for certificate in certificates {
        builder = builder.add_root_certificate(certificate);
    }
    builder
}

/// The ClickHouse HTTP executor.
#[derive(Debug, Clone)]
pub struct ClickHouseExecutor {
    client: reqwest::Client,
}

impl Default for ClickHouseExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ClickHouseExecutor {
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        let client = with_ca_bundle(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(timeout),
        )
        .build()
        // A client that cannot be built has no TLS backend at all; the plain-HTTP path the
        // container uses still works, so this is a warning rather than a startup failure.
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot build the ClickHouse HTTP client");
            reqwest::Client::new()
        });
        Self { client }
    }

    /// Run one statement and return its rows as text.
    pub async fn query(
        &self,
        config: &ClickHouseConfig,
        sql: &str,
    ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
        self.query_with_meta(config, sql)
            .await
            .map(|(rows, _)| rows)
    }

    /// Run one statement and return its rows plus the `meta` of the first `JSONCompact`
    /// document: the column names and their ClickHouse type spellings.
    pub async fn query_with_meta(
        &self,
        config: &ClickHouseConfig,
        sql: &str,
    ) -> Result<(Vec<Vec<Option<String>>>, QueryMetadata), ExecutorError> {
        let started = std::time::Instant::now();
        let mut request = self.client.post(config.url()).query(&config.params());
        if let Some(user) = &config.user {
            request = request.basic_auth(user, config.password.as_deref());
        }
        let response = request
            .body(sql.to_string())
            .send()
            .await
            .map_err(|error| {
                ExecutorError::new(format!(
                    "clickhouse connect {}:{}: {error}",
                    config.host, config.port
                ))
            })?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| ExecutorError::new(format!("clickhouse response body: {error}")))?;
        if !status.is_success() {
            // ClickHouse answers a rejected query with a plain-text exception and
            // `X-ClickHouse-Exception-Code`; the text is the only thing that says *why*.
            let text = String::from_utf8_lossy(&body);
            let text = text.trim();
            let text = text.chars().take(2000).collect::<String>();
            return Err(ExecutorError::new(format!(
                "clickhouse {}: {text}",
                status.as_u16()
            )));
        }
        let (rows, meta) = parse_json_compact_with_meta(&body)?;
        tracing::debug!(
            rows = rows.len(),
            ms = started.elapsed().as_secs_f64() * 1000.0,
            "clickhouse query"
        );
        Ok((rows, meta))
    }
}

#[async_trait]
impl Executor for ClickHouseExecutor {
    async fn fetch(
        &self,
        sql: &str,
        target: Option<&str>,
    ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
        let Some(dsn) = target else {
            return Err(ExecutorError::new(
                "no ClickHouse connection: the dataset resolved to no database",
            ));
        };
        let config = ClickHouseConfig::from_dsn(dsn)
            .map_err(|error| ExecutorError::new(format!("clickhouse dsn: {error}")))?;
        self.query(&config, sql).await
    }

    async fn fetch_with_meta(
        &self,
        sql: &str,
        target: Option<&str>,
    ) -> Result<(Vec<Vec<Option<String>>>, QueryMetadata), ExecutorError> {
        let Some(dsn) = target else {
            return Err(ExecutorError::new(
                "no ClickHouse connection: the dataset resolved to no database",
            ));
        };
        let config = ClickHouseConfig::from_dsn(dsn)
            .map_err(|error| ExecutorError::new(format!("clickhouse dsn: {error}")))?;
        self.query_with_meta(&config, sql).await
    }
}

/// The `JSONCompact` document of one query (or the several chunks of a streamed one).
///
/// ClickHouse writes a large result as consecutive JSON objects, each with its own `meta` and
/// `data`; a small one is a single object. Parsing with `Deserializer::into_iter` reads the
/// stream instead of requiring one JSON value.
pub fn parse_json_compact(bytes: &[u8]) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
    parse_json_compact_with_meta(bytes).map(|(rows, _)| rows)
}

/// [`parse_json_compact`] plus the `meta` of the first document that carries one.
///
/// ClickHouse repeats `meta` in every streamed document; the Python adapter reads the first
/// `meta` event and keeps it (`first_evt_data`), so the port does the same.
pub fn parse_json_compact_with_meta(
    bytes: &[u8],
) -> Result<(Vec<Vec<Option<String>>>, QueryMetadata), ExecutorError> {
    let mut rows = Vec::new();
    let mut meta = QueryMetadata::default();
    let stream = serde_json::Deserializer::from_slice(bytes).into_iter::<Value>();
    for document in stream {
        let document = document.map_err(|error| {
            ExecutorError::new(format!("clickhouse body is not JSONCompact: {error}"))
        })?;
        if meta.is_empty() {
            if let Some(columns) = document.get("meta").and_then(Value::as_array) {
                if !columns.is_empty() {
                    meta = QueryMetadata::new(
                        columns
                            .iter()
                            .map(|column| {
                                column
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string()
                            })
                            .collect(),
                        columns
                            .iter()
                            .map(|column| {
                                column
                                    .get("type")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string()
                            })
                            .collect(),
                    );
                }
            }
        }
        let data = document
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| ExecutorError::new("clickhouse body has no `data` array"))?;
        for row in data {
            let cells = row
                .as_array()
                .ok_or_else(|| ExecutorError::new("clickhouse row is not an array"))?;
            rows.push(cells.iter().map(cell_to_text).collect());
        }
    }
    Ok((rows, meta))
}

/// One `JSONCompact` cell as the text `dl_api` decodes: `null` is no value, everything else is
/// its PostgreSQL-ish spelling (`123`, `1.5`, `2026-05-08 00:00:00`, `text`, `[1,2]`).
fn cell_to_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        // `serde_json::Number` prints the value as JSON wrote it, which is the shortest
        // round-tripping form — the same rule `str(float)` follows.
        Value::Number(number) => Some(number.to_string()),
        Value::Array(_) | Value::Object(_) => Some(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_compact_document_becomes_rows_of_text() {
        let body = br#"{"meta":[{"name":"a","type":"String"},{"name":"b","type":"UInt64"}],
                       "data":[["Europe",123],["Asia",null]],"rows":2}"#;
        let rows = parse_json_compact(body).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Some("Europe".into()), Some("123".into())],
                vec![Some("Asia".into()), None],
            ]
        );
    }

    #[test]
    fn a_json_compact_document_keeps_its_column_metadata() {
        let body =
            br#"{"meta":[{"name":"a","type":"String"},{"name":"b","type":"Nullable(UInt64)"}],
                       "data":[["Europe",123]]}"#;
        let (rows, meta) = parse_json_compact_with_meta(body).unwrap();
        assert_eq!(rows, vec![vec![Some("Europe".into()), Some("123".into())]]);
        assert_eq!(meta.names, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            meta.db_types,
            vec!["String".to_string(), "Nullable(UInt64)".to_string()]
        );
    }

    #[test]
    fn the_first_streamed_document_supplies_the_metadata() {
        let body = concat!(
            r#"{"meta":[{"name":"x","type":"Int32"}],"data":[[1]]}"#,
            r#"{"meta":[],"data":[[2]]}"#,
        );
        let (rows, meta) = parse_json_compact_with_meta(body.as_bytes()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(meta.names, vec!["x".to_string()]);
        assert_eq!(meta.db_types, vec!["Int32".to_string()]);
    }

    #[test]
    fn a_streamed_document_is_read_chunk_by_chunk() {
        let body = concat!(r#"{"meta":[],"data":[[1]]}"#, r#"{"meta":[],"data":[[2]]}"#,);
        let rows = parse_json_compact(body.as_bytes()).unwrap();
        assert_eq!(rows, vec![vec![Some("1".into())], vec![Some("2".into())]]);
    }

    #[test]
    fn dates_arrays_and_decimals_keep_their_text() {
        let body = br#"{"data":[["2026-05-08 00:00:00",["a","b"],"12.50",true,3.5]]}"#;
        let rows = parse_json_compact(body).unwrap();
        assert_eq!(
            rows[0],
            vec![
                Some("2026-05-08 00:00:00".into()),
                Some("[\"a\",\"b\"]".into()),
                Some("12.50".into()),
                Some("true".into()),
                Some("3.5".into()),
            ]
        );
    }

    #[tokio::test]
    async fn the_executor_sends_the_sql_with_basic_auth_and_the_documented_params() {
        use axum::extract::{Query, State};
        use axum::http::HeaderMap;
        use axum::routing::post;
        use axum::Router;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Seen {
            params: Arc<Mutex<HashMap<String, String>>>,
            auth: Arc<Mutex<Option<String>>>,
            sql: Arc<Mutex<String>>,
        }

        async fn handle(
            State(seen): State<Seen>,
            Query(params): Query<HashMap<String, String>>,
            headers: HeaderMap,
            body: String,
        ) -> String {
            *seen.params.lock().unwrap() = params;
            *seen.auth.lock().unwrap() = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            *seen.sql.lock().unwrap() = body;
            r#"{"meta":[{"name":"x","type":"UInt8"}],"data":[[7]]}"#.to_string()
        }

        let seen = Seen {
            params: Arc::new(Mutex::new(HashMap::new())),
            auth: Arc::new(Mutex::new(None)),
            sql: Arc::new(Mutex::new(String::new())),
        };
        let app = Router::new()
            .route("/", post(handle))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let executor = ClickHouseExecutor::new();
        let dsn = format!(
            "clickhouse://user:p%40ss@127.0.0.1:{}/analytics",
            address.port()
        );
        let rows = executor
            .fetch("SELECT count() FROM t", Some(&dsn))
            .await
            .unwrap();
        assert_eq!(rows, vec![vec![Some("7".into())]]);
        let params = seen.params.lock().unwrap().clone();
        assert_eq!(
            params.get("database").map(String::as_str),
            Some("analytics")
        );
        assert_eq!(params.get("join_use_nulls").map(String::as_str), Some("1"));
        assert_eq!(params.get("readonly").map(String::as_str), Some("2"));
        assert_eq!(
            params.get("default_format").map(String::as_str),
            Some("JSONCompact")
        );
        assert_eq!(seen.sql.lock().unwrap().as_str(), "SELECT count() FROM t");
        let auth = seen.auth.lock().unwrap().clone().unwrap();
        assert!(auth.starts_with("Basic "), "{auth}");
    }

    #[tokio::test]
    async fn an_http_error_carries_the_clickhouse_exception_text() {
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::Router;

        async fn handle() -> (StatusCode, &'static str) {
            (
                StatusCode::BAD_REQUEST,
                "Code: 62. DB::Exception: Syntax error",
            )
        }

        let app = Router::new().route("/", post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let executor = ClickHouseExecutor::new();
        let dsn = format!("clickhouse://127.0.0.1:{}", address.port());
        let error = executor.fetch("SELEC 1", Some(&dsn)).await.unwrap_err();
        assert!(error.message.contains("Syntax error"), "{}", error.message);
    }
}
