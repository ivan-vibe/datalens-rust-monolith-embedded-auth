//! `us` connection entries wired into `dl_api`.
//!
//! `dl_api::Service` asks a [`ConnectionResolver`](dl_api::ConnectionResolver) for the DSN of
//! the dataset's `connection_id`; this is the one that answers, through `dl_us` (entry fetch,
//! fernet password, TTL cache). The adapter lives here, not in `dl_us`, so the `us` crate stays
//! independent of the API crate.
//!
//! Besides the DSN the resolver reports the connection policy the data-api enforces: the
//! `raw_sql_level` gate for Custom SQL sources and the `data_export_forbidden` flag.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dl_api::{ConnectionResolver, ConnectionTarget};
use dl_core::RawSqlLevel;
use dl_us::{UsConnectionCache, UsEntry};
use serde::Deserialize;
use tokio::sync::RwLock;

/// `ConnectionSQL.DataModel.raw_sql_level` + `data_export_forbidden`.
#[derive(Debug, Clone, Default, Deserialize)]
struct ConnectionPolicy {
    #[serde(default)]
    raw_sql_level: Option<String>,
    #[serde(default)]
    data_export_forbidden: bool,
}

#[derive(Debug, Clone, Copy)]
struct ConnectionMeta {
    raw_sql_level: RawSqlLevel,
    data_export_forbidden: bool,
}

impl Default for ConnectionMeta {
    fn default() -> Self {
        Self {
            // A connection that does not say otherwise is `off` (`SubselectConnectionDataSchemaMixin`).
            raw_sql_level: RawSqlLevel::Off,
            data_export_forbidden: false,
        }
    }
}

impl ConnectionMeta {
    fn from_entry(entry: &UsEntry) -> Self {
        let policy = entry
            .data
            .as_ref()
            .and_then(|data| serde_json::from_value::<ConnectionPolicy>(data.clone()).ok())
            .unwrap_or_default();
        Self {
            raw_sql_level: policy
                .raw_sql_level
                .as_deref()
                .and_then(RawSqlLevel::parse)
                .unwrap_or_default(),
            data_export_forbidden: policy.data_export_forbidden,
        }
    }
}

struct CachedMeta {
    meta: ConnectionMeta,
    loaded_at: Instant,
}

/// A [`ConnectionResolver`] over the `us` connection cache.
pub struct UsConnections {
    cache: Arc<UsConnectionCache>,
    /// The policy fields live in the same connection entry as the DSN; this cache keeps the
    /// extra entry read from happening on every request.
    meta: RwLock<HashMap<String, CachedMeta>>,
    meta_ttl: Duration,
}

impl UsConnections {
    pub fn new(cache: Arc<UsConnectionCache>) -> Self {
        // The metadata is the connection entry itself: reuse the DSN cache's TTL.
        let meta_ttl = std::env::var("DL_CONNECTION_CACHE_TTL_SECS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(300));
        Self {
            cache,
            meta: RwLock::new(HashMap::new()),
            meta_ttl,
        }
    }

    async fn meta(&self, connection_id: &str) -> ConnectionMeta {
        {
            let guard = self.meta.read().await;
            if let Some(cached) = guard.get(connection_id) {
                if cached.loaded_at.elapsed() < self.meta_ttl {
                    return cached.meta;
                }
            }
        }
        let meta = match self.cache.client().get_entry(connection_id).await {
            Ok(entry) => ConnectionMeta::from_entry(&entry),
            Err(error) => {
                tracing::warn!(
                    connection_id,
                    error = %error,
                    "cannot read the connection policy, using the storage defaults"
                );
                ConnectionMeta::default()
            }
        };
        if !self.meta_ttl.is_zero() {
            let mut guard = self.meta.write().await;
            if guard.len() >= 256 {
                guard.clear();
            }
            guard.insert(
                connection_id.to_string(),
                CachedMeta {
                    meta,
                    loaded_at: Instant::now(),
                },
            );
        }
        meta
    }
}

#[async_trait]
impl ConnectionResolver for UsConnections {
    async fn resolve(&self, connection_id: &str) -> Option<String> {
        self.resolve_target(connection_id)
            .await
            .map(|target| target.dsn)
    }

    async fn resolve_target(&self, connection_id: &str) -> Option<ConnectionTarget> {
        match self.cache.connection(connection_id).await {
            Ok(connection) => {
                let meta = self.meta(connection_id).await;
                Some(ConnectionTarget {
                    // PostgreSQL carries `?sslmode=require` when the connection asks for TLS, and
                    // the executor negotiates rustls; a ClickHouse DSN carries `clickhouses://`
                    // and the HTTP client does the same.
                    dsn: connection.dsn,
                    // The UI "Cache TTL in seconds" setting of this connection.
                    cache_ttl_sec: connection.cache_ttl_sec,
                    raw_sql_level: meta.raw_sql_level,
                    data_export_forbidden: meta.data_export_forbidden,
                    // `allow_background_data_export_for_conn_type` is `True` for every connector
                    // the port has an executor for (`dl_core.us_connection_base`).
                    data_export_allowed_in_conn_type: true,
                })
            }
            Err(error) => {
                tracing::warn!(
                    connection_id,
                    error = %error,
                    "cannot resolve the connection, falling back to DL_DATABASE_URL"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(data: serde_json::Value) -> UsEntry {
        serde_json::from_value(json!({
            "entryId": "conn-1",
            "scope": "connection",
            "type": "postgres",
            "data": data,
        }))
        .expect("entry")
    }

    #[test]
    fn a_connection_without_raw_sql_level_is_off() {
        let meta = ConnectionMeta::from_entry(&entry(json!({ "host": "db" })));
        assert_eq!(meta.raw_sql_level, RawSqlLevel::Off);
        assert!(!meta.data_export_forbidden);
    }

    #[test]
    fn the_connection_policy_is_read_from_the_entry() {
        let meta = ConnectionMeta::from_entry(&entry(json!({
            "raw_sql_level": "subselect",
            "data_export_forbidden": true,
        })));
        assert_eq!(meta.raw_sql_level, RawSqlLevel::Subselect);
        assert!(meta.data_export_forbidden);
    }
}
