//! `datalens-server` binary: the Rust data-api.
//!
//! Configuration is environment-only, so the binary stays drop-in for a container:
//!
//! | variable | meaning |
//! |---|---|
//! | `DL_DATABASE_URL` / `DATABASE_URL` | fallback PostgreSQL DSN; unset means "use each dataset's `connection`" |
//! | `DL_DIALECT` | `postgres` (default), `mysql`, `clickhouse` |
//! | `DL_DATASETS` | path to a JSON map `{"ds_id": <dataset>}` |
//! | `US_HOST` | base URL of `us`, e.g. `http://us:8081`; unset disables `us` |
//! | `US_MASTER_TOKEN` | `X-US-Master-Token` for `us` (the container's mode) |
//! | `DL_US_BRANCH` | `published` (default) or `saved` |
//! | `DL_US_CACHE_TTL_SECS` | dataset cache TTL, default `30` |
//! | `DL_CONNECTION_CACHE_TTL_SECS` | connection (DSN) cache TTL, default `300` |
//! | `DL_CRY_KEY_VAL_ID_KEY` / `CONTROL_API_CRYPTO_KEY` | fernet key of the `us` secrets |
//! | `DL_CRY_ACTUAL_KEY_ID` | key id the secrets name (`KEY` in the compose file) |
//! | `DL_DB_POOL_SIZE` | connections per database, default `8` |
//! | `DL_PORT` | listen port, default `8000` |
//! | `AUTH__TYPE` | `NONE` (default) or `NATIVE`; `ZITADEL` makes the process exit |
//! | `AUTH__JWT_ALGORITHM` | `PS256` (default) or another RS/PS algorithm |
//! | `AUTH__JWT_KEY` | PEM public key for `NATIVE` |
//!
//! `--dry-run` serves the demo dataset with a no-row executor (no database, US or auth) for
//! HTTP benchmarks. `--healthcheck [path]` probes the local service and exits 0 when it answers 200. It is
//! the container healthcheck, so the runtime image needs neither a shell nor `curl`.

use std::sync::Arc;
use std::time::Duration;

use axum::serve::ListenerExt;
use datalens_server::auth::NativeAuth;
use datalens_server::{router, AnyExecutor, AppState, DatasetStore, UsConnections};
use dl_api::Service;
use dl_query::Dialect;
use dl_us::{ConnectionSecrets, FernetKey, UsClient, UsConnectionCache, UsDatasetCache};
use tracing_subscriber::EnvFilter;

/// `datalens-server --healthcheck [/ping_ready]` — the container healthcheck without
/// `curl`, so the runtime image can be `scratch`. Exit code 0 when the endpoint answers 200.
fn run_healthcheck() -> i32 {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let path = healthcheck_path(std::env::args());
    let port: u16 = std::env::var("DL_PORT")
        .or_else(|_| std::env::var("PORT"))
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(8000);
    let address: SocketAddr = ([127, 0, 0, 1], port).into();

    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_secs(2)) else {
        return 1;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return 1;
    }

    let mut response = [0u8; 64];
    let read = stream.read(&mut response).unwrap_or(0);
    if read == 0 {
        return 1;
    }
    let status = String::from_utf8_lossy(&response[..read]);
    if status.starts_with("HTTP/1.1 200") || status.starts_with("HTTP/1.0 200") {
        0
    } else {
        1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    None,
    Native,
}

/// `NONE` and `NATIVE` are implemented; `ZITADEL` needs a full identity-provider integration
/// and is rejected instead of silently serving requests without auth.
fn auth_mode(auth_type: Option<&str>) -> Result<AuthMode, String> {
    match auth_type
        .map(|raw| raw.trim().to_ascii_uppercase())
        .as_deref()
    {
        None | Some("") | Some("NONE") => Ok(AuthMode::None),
        Some("NATIVE") => Ok(AuthMode::Native),
        Some("ZITADEL") => Err(
            "AUTH__TYPE=ZITADEL is not implemented: this build validates NATIVE JWTs only. \
             Use AUTH__TYPE=NATIVE or NONE."
                .to_string(),
        ),
        Some(other) => Err(format!(
            "AUTH__TYPE={other} is not implemented: expected NONE or NATIVE."
        )),
    }
}

fn fatal(message: String) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

/// The path argument of `--healthcheck`, defaulting to `/ping_ready`.
fn healthcheck_path(args: impl Iterator<Item = String>) -> String {
    args.skip_while(|arg| arg != "--healthcheck")
        .nth(1)
        .filter(|arg| arg.starts_with('/'))
        .unwrap_or_else(|| "/ping_ready".to_string())
}

/// v8: driven by `main.rs` on the single service runtime (`Handle::block_on` from the
/// data-api startup thread). It used to be `#[tokio::main]`, i.e. a fourth runtime.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|arg| arg == "--healthcheck") {
        std::process::exit(run_healthcheck());
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .try_init();

    // Benchmark-only mode: the demo dataset, an executor that returns no rows, and no
    // database/US/auth. It exists so two images can be compared over the same HTTP harness.
    if std::env::args().any(|arg| arg == "--dry-run") {
        let port: u16 = std::env::var("DL_PORT")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(8000);
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        tracing::info!("dry-run benchmark server on 0.0.0.0:{port}");
        let listener = listener.tap_io(|stream| {
            let _ = stream.set_nodelay(true);
        });
        let state = datalens_server::dry_run_state(dl_query::Dialect::Postgres);
        let app = router(state.clone());
        // The charts engine lives in this same OS process and reaches the dry-run
        // data-api over loopback HTTP unless the typed state is installed. The
        // production monolith installs it in `main`; installing the same state here
        // makes the `--dry-run` lab exercise the deployment chart path instead of a
        // transport the product only uses when data-api is a separate container.
        // `DL_DRY_RUN_DIRECT=0` restores loopback HTTP for A/B diagnostics.
        let direct = std::env::var("DL_DRY_RUN_DIRECT")
            .map(|value| value != "0")
            .unwrap_or(true);
        if direct {
            crate::data_direct::install(Arc::new(state));
            crate::local_http::DATA_ROUTER.set(app.clone()).ok();
            tracing::info!("dry-run data-api installed as the in-process typed chart backend");
        }
        if direct && std::env::var("MONOLITH").is_ok() {
            // v8: in the monolith every chart data call uses the typed in-process
            // `data_direct` path or the in-process `DATA_ROUTER`; an internal listener (and
            // the runtime that had to serve it) would only sit idle for the whole process.
            // The bind above is still the mock-mode probe: when an external mock owns the
            // port the bind fails and the engine keeps its loopback HTTP fallback.
            drop(listener);
            tracing::info!("dry-run data-api is in-process; not serving an internal listener in the monolith");
            return Ok(());
        }
        axum::serve(listener, app).await?;
        return Ok(());
    }

    let auth_type = std::env::var("AUTH__TYPE")
        .or_else(|_| std::env::var("AUTH_TYPE"))
        .ok();
    let auth = match auth_mode(auth_type.as_deref()).map_err(fatal)? {
        AuthMode::None => {
            match auth_type.as_deref().map(str::trim) {
                None | Some("") => {
                    tracing::warn!("AUTH__TYPE is not set: running without authentication (NONE)")
                }
                _ => tracing::info!("authentication is disabled (AUTH__TYPE=NONE)"),
            }
            None
        }
        AuthMode::Native => {
            let auth = NativeAuth::from_env().map_err(fatal)?;
            tracing::info!("NATIVE JWT authentication enabled");
            Some(auth)
        }
    };

    let dialect_name = std::env::var("DL_DIALECT").unwrap_or_else(|_| "postgres".to_string());
    let dialect = Dialect::parse(&dialect_name)
        .ok_or_else(|| format!("unknown DL_DIALECT: {dialect_name}"))?;

    let mut store = DatasetStore::with_defaults();
    if let Ok(path) = std::env::var("DL_DATASETS") {
        let raw = std::fs::read_to_string(&path)?;
        let extra = DatasetStore::from_json(&raw)?;
        for ds_id in extra.ids() {
            if let Some(dataset) = extra.get(ds_id) {
                store.insert_arc(ds_id, dataset);
            }
        }
        tracing::info!("loaded {} datasets from {path}", extra.ids().len());
    }

    let dsn = std::env::var("DL_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
        .map(|dsn| dsn.trim().to_string())
        // An empty value is what a compose file passes when the variable is not set
        // (`DL_DATABASE_URL: ${DL_DATABASE_URL:-}`), and it must mean "no DSN", not a broken one.
        .filter(|dsn| !dsn.is_empty());
    // Per-database connection pool: a dashboard's charts must not queue behind each other.
    let pool_size = std::env::var("DL_DB_POOL_SIZE")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|size| *size > 0)
        .unwrap_or(dl_postgres::DEFAULT_POOL_SIZE);
    tracing::info!("postgres pool: at most {pool_size} connections per database");
    let executor = AnyExecutor::databases(dsn.clone(), pool_size);
    if dsn.is_none() {
        tracing::info!(
            "DL_DATABASE_URL is not set: every dataset's own `connection` entry decides the database"
        );
    }

    // `us` is optional: without it only the local dataset store is served.
    // v9.0: enable the per-connection query-result cache (TTL from the connection's UI
    // setting; env override). Metadata caches above keep their own envs and are untouched.
    let service = Service::new(executor, dialect)
        .with_query_cache(dl_api::QueryCacheSettings::from_env());
    let mut state = AppState::new(Arc::new(service), Arc::new(store));
    if let Some(auth) = auth {
        state = state.with_auth(auth);
    }

    if let Ok(us_host) = std::env::var("US_HOST") {
        let branch = std::env::var("DL_US_BRANCH").unwrap_or_else(|_| "published".to_string());
        let ttl = std::env::var("DL_US_CACHE_TTL_SECS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(30));
        let connection_ttl = std::env::var("DL_CONNECTION_CACHE_TTL_SECS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(300));
        let mut client = UsClient::new(&us_host)?.with_branch(branch.clone());
        match std::env::var("US_MASTER_TOKEN") {
            Ok(token) if !token.is_empty() => client = client.with_master_token(token),
            _ => tracing::warn!("US_MASTER_TOKEN is not set: us is called without credentials"),
        }
        tracing::info!("reading datasets from {us_host} (branch {branch}, ttl {ttl:?})");

        // Connection passwords are fernet-encrypted with the key the container already has.
        let key = std::env::var("DL_CRY_KEY_VAL_ID_KEY")
            .or_else(|_| std::env::var("CONTROL_API_CRYPTO_KEY"))
            .ok()
            .and_then(|key| FernetKey::from_base64(&key));
        if key.is_none() {
            tracing::warn!(
                "DL_CRY_KEY_VAL_ID_KEY / CONTROL_API_CRYPTO_KEY is not set: \
                 encrypted connection passwords cannot be decrypted"
            );
        }
        let secrets = ConnectionSecrets::new(std::env::var("DL_CRY_ACTUAL_KEY_ID").ok(), key);

        let connections =
            Arc::new(UsConnectionCache::new(client.clone(), connection_ttl).with_secrets(secrets));
        let service = Service::new(AnyExecutor::databases(dsn.clone(), pool_size), dialect)
            .with_connection_resolver(Arc::new(UsConnections::new(connections)))
            .with_query_cache(dl_api::QueryCacheSettings::from_env());
        state = state
            .with_us(Arc::new(UsDatasetCache::new(client, ttl)))
            .with_service(Arc::new(service));
    } else {
        tracing::info!("US_HOST is not set: only the local dataset store is served");
    }

    let port: u16 = std::env::var("DL_PORT")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(8000);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(
        "datalens-server listening on 0.0.0.0:{port} (dialect: {})",
        dialect.as_str()
    );
    // `axum::serve` does not touch the socket, and a `TcpStream` without `TCP_NODELAY` waits for
    // the peer's ACK before the second write: a head plus a body is exactly that case. Measured on
    // the demo stack, this removes the few-millisecond tail on small JSON answers.
    let listener = listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::warn!(%error, "cannot set TCP_NODELAY on an accepted connection");
        }
    });
    // v5.2: chart paths call the typed data-api directly (`src/data_direct.rs`);
    // the HTTP router stays installed for the public data routes.
    crate::data_direct::install(Arc::new(state.clone()));
    let app = router(state);
    crate::local_http::DATA_ROUTER.set(app.clone()).ok();
    if std::env::var("MONOLITH").is_ok() {
        return Ok(());
    }
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{auth_mode, healthcheck_path, AuthMode};

    #[test]
    fn healthcheck_reads_the_path_after_the_flag() {
        let args = ["bin", "--healthcheck", "/ping_ready"].map(str::to_string);
        assert_eq!(healthcheck_path(args.into_iter()), "/ping_ready");
    }

    #[test]
    fn none_and_native_auth_modes_are_accepted() {
        assert_eq!(auth_mode(None), Ok(AuthMode::None));
        assert_eq!(auth_mode(Some("")), Ok(AuthMode::None));
        assert_eq!(auth_mode(Some("none")), Ok(AuthMode::None));
        assert_eq!(auth_mode(Some("NATIVE")), Ok(AuthMode::Native));
        assert!(auth_mode(Some("ZITADEL")).is_err());
        assert!(auth_mode(Some("mystery")).is_err());
    }

    #[test]
    fn healthcheck_defaults_to_ping_ready() {
        for raw in [
            vec!["bin"],
            vec!["bin", "--healthcheck"],
            vec!["bin", "--healthcheck", "not-a-path"],
        ] {
            let args = raw.into_iter().map(str::to_string);
            assert_eq!(healthcheck_path(args), "/ping_ready");
        }
    }
}
