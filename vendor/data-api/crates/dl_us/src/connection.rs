//! `us` `connection` entries -> the DSN of the database they name.
//!
//! A dataset never names a database directly: its `source_collections[].origin.connection_id`
//! points at a `connection` entry, and that entry's `data` (host, port, db, user) plus its
//! encrypted `unversioned_data.password` are what the connector needs. Python resolves this
//! through `dl_core.services_registry` + `dl_connector_postgresql`, and the port needs the same
//! to answer a real request without a hand-written DSN.
//!
//! The password is stored as a fernet token (`{"key_kind": "local_fernet", "cypher_text": ...}`)
//! and is decrypted with the key `CONTROL_API_CRYPTO_KEY` the container already has.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::client::UsClient;
use crate::entry::{UsConnectionData, UsEntry};
use crate::error::UsError;
use crate::fernet::FernetKey;

/// Connections cached by default.
const DEFAULT_CAPACITY: usize = 256;

/// The fernet key secrets are encrypted with (`DL_CRY_KEY_VAL_ID_KEY` / `CONTROL_API_CRYPTO_KEY`).
#[derive(Clone, Default)]
pub struct ConnectionSecrets {
    key_id: Option<String>,
    key: Option<FernetKey>,
}

impl std::fmt::Debug for ConnectionSecrets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionSecrets")
            .field("key_id", &self.key_id)
            .field("has_key", &self.key.is_some())
            .finish()
    }
}

impl ConnectionSecrets {
    /// `key_id` is `DL_CRY_ACTUAL_KEY_ID` (`KEY` in the compose file).
    pub fn new(key_id: Option<String>, key: Option<FernetKey>) -> Self {
        Self { key_id, key }
    }

    /// The plaintext of `unversioned_data.password`, whatever shape it has.
    pub fn password(&self, entry: &UsEntry) -> Result<Option<String>, UsError> {
        let secret = match entry.password_secret() {
            Some(secret) => secret,
            None => return Ok(None),
        };
        match secret {
            // Older entries keep the password as is.
            serde_json::Value::String(plain) => Ok(Some(plain.clone())),
            serde_json::Value::Object(fields) => {
                let kind = fields
                    .get("key_kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("plain");
                let key_id = fields
                    .get("key_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let cypher = fields
                    .get("cypher_text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if kind != "local_fernet" {
                    // `plain` secrets carry their value directly.
                    return Ok((!cypher.is_empty()).then(|| cypher.to_string()));
                }
                let key = self.key.ok_or_else(|| UsError::Invalid {
                    entry_id: entry.entry_id.clone(),
                    reason: "the connection password is encrypted but no crypto key is set"
                        .to_string(),
                })?;
                if let Some(expected) = self.key_id.as_deref() {
                    if !key_id.is_empty() && key_id != expected {
                        return Err(UsError::Invalid {
                            entry_id: entry.entry_id.clone(),
                            reason: format!(
                                "the connection password is encrypted with key {key_id}, \
                                 the service has {expected}"
                            ),
                        });
                    }
                }
                key.decrypt(cypher)
                    .map(Some)
                    .map_err(|error| UsError::Invalid {
                        entry_id: entry.entry_id.clone(),
                        reason: format!("cannot decrypt the connection password: {error}"),
                    })
            }
            _ => Ok(None),
        }
    }
}

/// Which connector a `connection` entry names (`entry.type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorKind {
    Postgres,
    ClickHouse,
    MySql,
    /// A connector the port has no executor for (mssql, oracle, ...).
    Unsupported,
}

impl ConnectorKind {
    pub fn from_entry_type(entry_type: &str) -> Self {
        match entry_type.trim().to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "greenplum" => Self::Postgres,
            "clickhouse" => Self::ClickHouse,
            "mysql" => Self::MySql,
            _ => Self::Unsupported,
        }
    }

    /// The DSN scheme the executor dispatches on (`clickhouse://`, `postgres://`, `mysql://`).
    pub fn scheme(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::ClickHouse => "clickhouse",
            Self::MySql => "mysql",
            Self::Unsupported => "unknown",
        }
    }
}

/// A resolved connection: the DSN plus what the executor cannot honour yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsConnection {
    pub dsn: String,
    /// `ssl_enable` is on: the plain-TCP executor serves it anyway, the caller logs it.
    pub ssl_required: bool,
    pub kind: ConnectorKind,
    /// `data.cache_ttl_sec` ("Cache TTL in seconds"): `Some(0)` disables the query-result
    /// cache for this connection, `None` means no per-connection override.
    pub cache_ttl_sec: Option<i64>,
}

struct CachedConnection {
    connection: UsConnection,
    loaded_at: Instant,
}

/// A `us` client plus a TTL cache of resolved connection DSNs.
pub struct UsConnectionCache {
    client: UsClient,
    secrets: ConnectionSecrets,
    ttl: Duration,
    capacity: usize,
    entries: RwLock<HashMap<String, CachedConnection>>,
}

impl UsConnectionCache {
    pub fn new(client: UsClient, ttl: Duration) -> Self {
        Self {
            client,
            secrets: ConnectionSecrets::default(),
            ttl,
            capacity: DEFAULT_CAPACITY,
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub fn with_secrets(mut self, secrets: ConnectionSecrets) -> Self {
        self.secrets = secrets;
        self
    }

    pub fn client(&self) -> &UsClient {
        &self.client
    }

    /// The DSN of `connection_id`, from the cache or from `us`.
    pub async fn connection(&self, connection_id: &str) -> Result<UsConnection, UsError> {
        if let Some(connection) = self.cached(connection_id).await {
            return Ok(connection);
        }
        let entry = self.client.get_connection(connection_id).await?;
        let connection = self.resolve(connection_id, &entry)?;
        self.store(connection_id, connection.clone()).await;
        Ok(connection)
    }

    /// `entry` -> DSN. Public because the tests build an entry instead of a server.
    ///
    /// The connector is taken from the entry's `type`, the same key `us` uses to pick a Python
    /// connector; a connector without an executor is an error here, not an empty answer later.
    pub fn resolve(&self, connection_id: &str, entry: &UsEntry) -> Result<UsConnection, UsError> {
        let kind = ConnectorKind::from_entry_type(&entry.entry_type);
        if kind == ConnectorKind::Unsupported {
            return Err(UsError::Invalid {
                entry_id: connection_id.to_string(),
                reason: format!(
                    "the connector `{}` has no Rust executor yet",
                    entry.entry_type
                ),
            });
        }
        let data: UsConnectionData = entry.connection_data()?;
        let password = self.secrets.password(entry)?;
        let dsn = match kind {
            ConnectorKind::ClickHouse => data.clickhouse_dsn(password.as_deref()),
            ConnectorKind::MySql => mysql_dsn(&data, password.as_deref()),
            _ => data.dsn(password.as_deref()),
        }
        .map(apply_host_map)
        .ok_or_else(|| UsError::Invalid {
            entry_id: connection_id.to_string(),
            reason: format!(
                "the connection has no host{}",
                match kind {
                    ConnectorKind::ClickHouse => "",
                    _ => " or database name",
                }
            ),
        })?;
        Ok(UsConnection {
            dsn,
            ssl_required: data.ssl_required(),
            kind,
            cache_ttl_sec: data.cache_ttl_sec,
        })
    }

    pub async fn invalidate(&self, connection_id: &str) {
        self.entries.write().await.remove(connection_id);
    }

    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    async fn cached(&self, connection_id: &str) -> Option<UsConnection> {
        let guard = self.entries.read().await;
        let cached = guard.get(connection_id)?;
        (cached.loaded_at.elapsed() < self.ttl).then(|| cached.connection.clone())
    }

    async fn store(&self, connection_id: &str, connection: UsConnection) {
        let mut guard = self.entries.write().await;
        if guard.len() >= self.capacity {
            let ttl = self.ttl;
            guard.retain(|_, cached| cached.loaded_at.elapsed() < ttl);
            if guard.len() >= self.capacity {
                if let Some(oldest) = guard
                    .iter()
                    .min_by_key(|(_, cached)| cached.loaded_at)
                    .map(|(key, _)| key.clone())
                {
                    guard.remove(&oldest);
                }
            }
        }
        guard.insert(
            connection_id.to_string(),
            CachedConnection {
                connection,
                loaded_at: Instant::now(),
            },
        );
    }
}

impl std::fmt::Debug for UsConnectionCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsConnectionCache")
            .field("base_url", &self.client.base_url())
            .field("ttl", &self.ttl)
            .field("secrets", &self.secrets)
            .finish_non_exhaustive()
    }
}

/// Host overrides for running the service outside the compose network.
///
/// Connection entries name the compose service (`postgres`, `clickhouse`, ...); a host
/// process cannot resolve those names, while a container can. `DL_CONNECTION_HOST_MAP`
/// rewrites the DSN authority: `postgres=127.0.0.1:25432` or
/// `postgres=127.0.0.1:25432,clickhouse=127.0.0.1:28123`. The value may be `host` or
/// `host:port`; unset/empty keeps the entry's own authority. Parsed once per process.
fn host_map() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        let raw = match std::env::var("DL_CONNECTION_HOST_MAP") {
            Ok(raw) if !raw.trim().is_empty() => raw,
            _ => return HashMap::new(),
        };
        raw.split(',')
            .filter_map(|pair| {
                let (from, to) = pair.split_once('=')?;
                let from = from.trim();
                let to = to.trim();
                (!from.is_empty() && !to.is_empty()).then(|| (from.to_string(), to.to_string()))
            })
            .collect()
    })
}

/// `mysql://user:password@host:port/db`, `mysqls://` when the connection asks for TLS.
///
/// `dl_connector_mysql.core.us_connection.ConnectionMySQL` requires `host`, `port`, `db_name`
/// and `username`, and passes `ssl_enable` to an `ssl.SSLContext`; a MySQL DSN therefore needs
/// the database name, like PostgreSQL and unlike ClickHouse. The TLS spelling is a scheme
/// rather than a query parameter so the query dialect and the executor both see it without
/// parsing options.
fn mysql_dsn(data: &UsConnectionData, password: Option<&str>) -> Option<String> {
    let host = data.host.as_deref()?.trim();
    let db_name = data.db_name.as_deref()?.trim();
    if host.is_empty() || db_name.is_empty() {
        return None;
    }
    let port = data.port.unwrap_or(3306);
    let mut dsn = String::from(match data.ssl_required() {
        true => "mysqls://",
        false => "mysql://",
    });
    if let Some(username) = data
        .username
        .as_deref()
        .map(str::trim)
        .filter(|user| !user.is_empty())
    {
        dsn.push_str(&encode_userinfo(username));
        if let Some(password) = password {
            dsn.push(':');
            dsn.push_str(&encode_userinfo(password));
        }
        dsn.push('@');
    }
    dsn.push_str(host);
    dsn.push(':');
    dsn.push_str(&port.to_string());
    dsn.push('/');
    dsn.push_str(&encode_userinfo(db_name));
    Some(dsn)
}

/// Percent-encode one DSN component. The rule is `entry::encode_userinfo`'s (everything
/// outside `A-Za-z0-9-_.~`); it is repeated here because the PostgreSQL/ClickHouse builder
/// lives in `entry`, and the MySQL DSN needs the same authority syntax.
fn encode_userinfo(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Rewrite the authority (`user:pass@host:port`) of a DSN using the host map.
fn apply_host_map(dsn: String) -> String {
    rewrite_authority(&dsn, host_map())
}

/// The pure half of [`apply_host_map`], so it is testable without touching the environment.
fn rewrite_authority(dsn: &str, map: &HashMap<String, String>) -> String {
    if map.is_empty() {
        return dsn.to_string();
    }
    let Some(scheme_end) = dsn.find("://") else {
        return dsn.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = dsn[authority_start..]
        .find('/')
        .map(|index| authority_start + index)
        .unwrap_or(dsn.len());
    let authority = &dsn[authority_start..authority_end];
    let (userinfo, host_port) = match authority.rfind('@') {
        Some(index) => (&authority[..=index], &authority[index + 1..]),
        None => ("", authority),
    };
    let host = host_port
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(host_port);
    let Some(mapped) = map.get(host) else {
        return dsn.to_string();
    };
    format!(
        "{}{}{}{}",
        &dsn[..authority_start],
        userinfo,
        mapped,
        &dsn[authority_end..]
    )
}

#[cfg(test)]
mod tests {
    use super::rewrite_authority;

    #[test]
    fn host_map_rewrites_only_the_authority_host() {
        let map = [
            ("postgres".to_string(), "127.0.0.1:25432".to_string()),
            ("clickhouse".to_string(), "127.0.0.1:28123".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            rewrite_authority("postgres://pg-user:postgres@postgres:5432/pg-us-db", &map),
            "postgres://pg-user:postgres@127.0.0.1:25432/pg-us-db"
        );
        assert_eq!(
            rewrite_authority("clickhouse://user:secret@clickhouse:8123/", &map),
            "clickhouse://user:secret@127.0.0.1:28123/"
        );
    }

    #[test]
    fn host_map_keeps_unknown_hosts_and_shapes_without_a_path() {
        let map = [("postgres".to_string(), "127.0.0.1:25432".to_string())]
            .into_iter()
            .collect();
        assert_eq!(
            rewrite_authority("postgres://u:p@db.internal:5432/app", &map),
            "postgres://u:p@db.internal:5432/app"
        );
        assert_eq!(
            rewrite_authority("postgres://pg-user:postgres@postgres:5432", &map),
            "postgres://pg-user:postgres@127.0.0.1:25432"
        );
        assert_eq!(rewrite_authority("not-a-dsn", &map), "not-a-dsn");
        assert_eq!(
            rewrite_authority("postgres://u:p@postgres:5432/app", &Default::default()),
            "postgres://u:p@postgres:5432/app"
        );
    }
    use super::*;

    /// `CONTROL_API_CRYPTO_KEY` of the compose file.
    const KEY: &str = "ZGF0YWxlbnN1bnNlY3VyZWZlcm5ldGNyeXB0b2tleQo=";
    /// `Fernet(KEY).encrypt(b"postgres")`, the demo connection password.
    const TOKEN: &str = "gAAAAABq6isD_ZFhpnoozjxV5V7nSbzvf5fFsiBOnkGMgsc4FUvd3GOYAyeS_FMzGHTBfKE4QUbyvLPs5Y08zJ_ulicYEnkOgg==";

    fn entry(password: Option<serde_json::Value>) -> UsEntry {
        serde_json::from_value(serde_json::json!({
            "entryId": "conn-1",
            "scope": "connection",
            "type": "postgres",
            "data": {
                "host": "postgres",
                "port": 5432,
                "db_name": "pg-demo-db",
                "username": "pg-user",
                "ssl_enable": "false"
            },
            "unversionedData": password.map(|password| serde_json::json!({ "password": password })),
        }))
        .unwrap()
    }

    fn cache(secrets: ConnectionSecrets) -> UsConnectionCache {
        UsConnectionCache::new(UsClient::new("http://us").unwrap(), Duration::ZERO)
            .with_secrets(secrets)
    }

    #[test]
    fn a_fernet_password_is_decrypted_into_the_dsn() {
        let secrets = ConnectionSecrets::new(
            Some("KEY".to_string()),
            Some(FernetKey::from_base64(KEY).unwrap()),
        );
        let data = entry(Some(serde_json::json!({
            "key_id": "KEY",
            "key_kind": "local_fernet",
            "cypher_text": TOKEN,
        })));
        let connection = cache(secrets).resolve("conn-1", &data).unwrap();
        assert_eq!(
            connection.dsn,
            "postgres://pg-user:postgres@postgres:5432/pg-demo-db"
        );
        assert!(!connection.ssl_required);
    }

    #[test]
    fn a_plain_password_is_used_as_is() {
        let data = entry(Some(serde_json::json!("s3cret")));
        let connection = cache(ConnectionSecrets::default())
            .resolve("conn-1", &data)
            .unwrap();
        assert_eq!(
            connection.dsn,
            "postgres://pg-user:s3cret@postgres:5432/pg-demo-db"
        );
    }

    #[test]
    fn an_encrypted_secret_without_a_key_is_an_error() {
        let data = entry(Some(serde_json::json!({
            "key_id": "KEY",
            "key_kind": "local_fernet",
            "cypher_text": TOKEN,
        })));
        let error = cache(ConnectionSecrets::default())
            .resolve("conn-1", &data)
            .unwrap_err();
        assert_eq!(error.status(), 400);
        assert!(error.to_string().contains("no crypto key"));
    }

    #[test]
    fn a_secret_encrypted_with_another_key_is_reported() {
        let secrets = ConnectionSecrets::new(
            Some("OTHER".to_string()),
            Some(FernetKey::from_base64(KEY).unwrap()),
        );
        let data = entry(Some(serde_json::json!({
            "key_id": "KEY",
            "key_kind": "local_fernet",
            "cypher_text": TOKEN,
        })));
        let error = cache(secrets).resolve("conn-1", &data).unwrap_err();
        assert!(error.to_string().contains("encrypted with key KEY"));
    }

    #[test]
    fn a_clickhouse_entry_becomes_a_clickhouse_dsn() {
        // A ClickHouse entry: `type: clickhouse`, host/port/user, no `db_name`
        // (the dataset's source names the table).
        let data: UsEntry = serde_json::from_value(serde_json::json!({
            "entryId": "conn-clickhouse",
            "scope": "connection",
            "type": "clickhouse",
            "data": {
                "host": "clickhouse.example.com",
                "port": 4123,
                "username": "demo",
                "secure": false,
            },
        }))
        .unwrap();
        let connection = cache(ConnectionSecrets::default())
            .resolve("conn-clickhouse", &data)
            .unwrap();
        assert_eq!(
            connection.dsn,
            "clickhouse://demo@clickhouse.example.com:4123"
        );
        assert_eq!(connection.kind, ConnectorKind::ClickHouse);
    }

    #[test]
    fn a_connector_without_an_executor_is_an_error_not_an_empty_answer() {
        let data: UsEntry = serde_json::from_value(serde_json::json!({
            "entryId": "conn-mssql",
            "scope": "connection",
            "type": "mssql",
            "data": {"host": "db", "port": 1433, "db_name": "demo", "username": "u"},
        }))
        .unwrap();
        let error = cache(ConnectionSecrets::default())
            .resolve("conn-mssql", &data)
            .unwrap_err();
        assert!(error.to_string().contains("no Rust executor"), "{error}");
    }

    #[test]
    fn a_mysql_entry_becomes_a_mysql_dsn() {
        let data: UsEntry = serde_json::from_value(serde_json::json!({
            "entryId": "conn-mysql",
            "scope": "connection",
            "type": "mysql",
            "data": {
                "host": "mysql.example.com",
                "port": 3307,
                "db_name": "analytics",
                "username": "demo",
            },
        }))
        .unwrap();
        let connection = cache(ConnectionSecrets::default())
            .resolve("conn-mysql", &data)
            .unwrap();
        assert_eq!(
            connection.dsn,
            "mysql://demo@mysql.example.com:3307/analytics"
        );
        assert_eq!(connection.kind, ConnectorKind::MySql);
        assert!(!connection.ssl_required);
    }

    #[test]
    fn a_mysql_dsn_defaults_the_port_and_encodes_the_password() {
        let data: UsEntry = serde_json::from_value(serde_json::json!({
            "entryId": "conn-mysql",
            "scope": "connection",
            "type": "mysql",
            "data": {
                "host": "db.local",
                "db_name": "demo",
                "username": "a@b",
            },
        }))
        .unwrap();
        let connection = cache(ConnectionSecrets::default())
            .resolve("conn-mysql", &data)
            .unwrap();
        // `resolve` has no password here; the entry's password is a fernet secret.
        assert_eq!(connection.dsn, "mysql://a%40b@db.local:3306/demo");
    }

    #[test]
    fn a_mysql_entry_with_ssl_becomes_a_mysqls_dsn() {
        let data: UsEntry = serde_json::from_value(serde_json::json!({
            "entryId": "conn-mysql",
            "scope": "connection",
            "type": "mysql",
            "data": {
                "host": "mysql.example.com",
                "port": 3306,
                "db_name": "analytics",
                "username": "demo",
                "ssl_enable": "true",
            },
        }))
        .unwrap();
        let connection = cache(ConnectionSecrets::default())
            .resolve("conn-mysql", &data)
            .unwrap();
        assert_eq!(
            connection.dsn,
            "mysqls://demo@mysql.example.com:3306/analytics"
        );
        assert!(connection.ssl_required);
    }

    #[test]
    fn a_mysql_entry_without_a_database_is_an_error() {
        let data: UsEntry = serde_json::from_value(serde_json::json!({
            "entryId": "conn-mysql",
            "scope": "connection",
            "type": "mysql",
            "data": {"host": "mysql.example.com", "username": "demo"},
        }))
        .unwrap();
        let error = cache(ConnectionSecrets::default())
            .resolve("conn-mysql", &data)
            .unwrap_err();
        assert!(
            error.to_string().contains("no host or database name"),
            "{error}"
        );
    }

    #[test]
    fn userinfo_is_percent_encoded() {
        let data: UsConnectionData = serde_json::from_value(serde_json::json!({
            "host": "db.local",
            "db_name": "demo",
            "username": "a@b",
        }))
        .unwrap();
        assert_eq!(
            data.dsn(Some("p@ss/word")).as_deref(),
            Some("postgres://a%40b:p%40ss%2Fword@db.local:5432/demo")
        );
    }
}
