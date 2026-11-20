//! Database layer: two pools (primary/replica) exactly like
//! `src/registry/...getDbInstance`, plus the knex-compatible migration runner.

use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

use crate::config::Config;

/// Matches `getReplica(trx)` semantics: reads go to the replica when it is
/// configured, writes always go to the primary.
#[derive(Clone)]
pub struct Db {
    pub primary: PgPool,
    pub replica: PgPool,
    pub max_connections: u32,
}

fn build_pool(dsn: &str, config: &Config) -> anyhow::Result<PgPool> {
    let options = PgConnectOptions::from_str(dsn)
        .with_context(|| format!("cannot parse postgres DSN for {}", config.app_name))?
        .application_name(&config.app_name);

    let statement_timeout = config.pg_query_timeout_ms;
    let max_connections = config.pg_pool_max.max(1);

    Ok(PgPoolOptions::new()
        .max_connections(max_connections)
        // Validate a pooled connection before handing it out. The reference
        // knex/tarn validates only on creation, which leaves every idle
        // connection broken after a Postgres restart; one ping per acquire is
        // cheap next to a 500 for the whole request.
        .test_before_acquire(true)
        // knex defaults the pool to 10 connections and a 60s idle timeout.
        .acquire_timeout(Duration::from_secs(10))
        // knex: idleTimeoutMillis 45000.
        .idle_timeout(Some(Duration::from_secs(45)))
        .max_lifetime(Some(Duration::from_secs(60 * 30)))
        .after_connect(move |connection, _meta| {
            Box::pin(async move {
                // The Node service applies `DEFAULT_QUERY_TIMEOUT` per query
                // (`statement_timeout`); doing it once per connection is both
                // faithful and faster.
                sqlx::query(&format!("SET statement_timeout = {statement_timeout}"))
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy_with(options))
}

impl Db {
    pub async fn connect(config: &Config) -> anyhow::Result<Self> {
        let primary = build_pool(&config.pg_primary_dsn, config)?;

        let replica = if config.pg_replica_dsn == config.pg_primary_dsn {
            primary.clone()
        } else {
            build_pool(&config.pg_replica_dsn, config)?
        };

        Ok(Self {
            primary,
            replica,
            max_connections: config.pg_pool_max.max(1),
        })
    }

    /// Port of `waitDatabase`: the Node service refuses to serve traffic until
    /// the primary answers. We retry briefly instead of hanging forever so the
    /// container reports liveness/readiness through `/ping-db*`.
    pub async fn wait_until_ready(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut attempt: u32 = 0;

        loop {
            attempt += 1;

            match sqlx::query("SELECT 1 + 1").execute(&self.primary).await {
                Ok(_) => {
                    tracing::info!(attempt, "primary database is ready");
                    return;
                }
                Err(error) => {
                    tracing::warn!(attempt, error = %error, "waiting for the primary database");
                }
            }

            if tokio::time::Instant::now() >= deadline {
                tracing::error!(
                    attempts = attempt,
                    "primary database is not available, starting anyway"
                );
                return;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// `us-rs` applies the official knex migrations on startup (see
    /// [`crate::migrations`]); this predicate only decides whether the schema
    /// exists, for the startup message when a migration was skipped.
    pub async fn schema_ready(&self) -> bool {
        sqlx::query_scalar::<_, Option<String>>("SELECT to_regclass('public.entries')::text")
            .fetch_one(&self.primary)
            .await
            .map(|value| value.is_some())
            .unwrap_or(false)
    }

    pub async fn ping_primary(&self) -> bool {
        sqlx::query("SELECT 1 + 1")
            .execute(&self.primary)
            .await
            .is_ok()
    }

    pub async fn ping_replica(&self) -> bool {
        sqlx::query("SELECT 1 + 1")
            .execute(&self.replica)
            .await
            .is_ok()
    }

    /// `numUsed()/numFree()/numPendingAcquires()/numPendingCreates()` of tarn.
    pub fn pool_stats(&self, pool: &PgPool) -> PoolStats {
        let size = pool.size();
        let idle = pool.num_idle() as u32;
        let used = size.saturating_sub(idle);

        PoolStats {
            used,
            // tarn's `numFree` is the number of *idle* connections, not the
            // remaining capacity (`get numFree() { return this.free.length; }`).
            free: idle,
            num_pending_acquires: 0,
            num_pending_creates: 0,
        }
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolStats {
    pub used: u32,
    pub free: u32,
    pub num_pending_acquires: u32,
    pub num_pending_creates: u32,
}
