//! `us-rs` server entry point.
//!
//! Route map (see `src/routes.ts` and `src/index.ts` of the TypeScript project):
//!
//! | method | path                          | handler                    |
//! |--------|-------------------------------|----------------------------|
//! | GET    | `/`                           | `helpers.home`             |
//! | GET    | `/ping`                       | `helpers.ping`             |
//! | GET    | `/ping-db`                    | `helpers.pingDb`           |
//! | GET    | `/ping-db-primary`            | `helpers.pingDbPrimary`    |
//! | GET    | `/pool`                       | `helpers.pool`             |
//! | GET    | `/v1/entries/:entryId`        | `entries.getEntry`         |
//! | GET    | `/v1/entries/:entryId/meta`   | `entries.getEntryMeta`     |
//!
//! Routes are registered both with and without a trailing slash because express
//! runs with `strict routing` disabled (`/v1/entries/<id>/` is served), and an
//! unmatched request gets the express `finalhandler` HTML page (see
//! [`api::not_found`]) instead of axum's empty 404.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::http::{Method, StatusCode};
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use tower_http::decompression::RequestDecompressionLayer;
use tower_http::timeout::TimeoutLayer;

use us_rs::api;
use us_rs::auth;
use us_rs::config::Config;
use us_rs::db::Db;
use us_rs::state::AppState;

/// How long `waitDatabase` keeps retrying before the process starts anyway.
const DB_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Builds the whole axum router. Public so integration tests can reuse it.
pub fn build_router(state: AppState) -> Router {
    let entry_routes = api::dual_router(vec![
        ("/v1/entries/{entry_id}", get(api::entries::get_entry)),
        (
            "/v1/entries/{entry_id}/meta",
            get(api::entries::get_entry_meta),
        ),
        // The private route is the same controller with `authPolicy: disabled`;
        // in this deployment the master-token check is a no-op and the payload
        // is identical to the public one (verified against the reference).
        ("/private/entries/{entry_id}", get(api::entries::get_entry)),
        (
            "/private/entries/{entry_id}/meta",
            get(api::entries::get_entry_meta),
        ),
    ]);

    let v1 = Router::new()
        .merge(entry_routes)
        .merge(api::entries_read::router())
        .merge(api::entries_write::router())
        .merge(api::entries_copy::router())
        .merge(api::locks::router())
        .merge(api::states::router())
        .merge(api::favorites::router())
        .merge(api::workbooks::router())
        .merge(api::collections::router())
        .merge(api::tenants::router())
        .merge(api::palettes::router())
        // `appAuth` is applied to the API routes, exactly like the
        // `authPolicy: disabled` endpoints in express (`/`, `/ping*`, `/pool`
        // stay public).
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let router = Router::new()
        .route("/", get(api::health::home))
        .route("/ping", get(api::health::ping))
        .route("/ping-db", get(api::health::ping_db))
        .route("/ping-db-primary", get(api::health::ping_db_primary))
        .route("/pool", get(api::health::pool))
        .route("/metrics", get(us_rs::metrics::scrape))
        .route("/api-docs", get(api::docs::redirect))
        .route("/api-docs/", get(api::docs::index))
        .route("/api-docs/swagger.json", get(api::docs::spec))
        .merge(v1)
        // express' `finalhandler`: an unmatched request is answered with the
        // default HTML 404 page (no auth middleware runs for it).
        .fallback(api::not_found::not_found)
        // express parses JSON with `limit: '50mb'`; axum's default is 2 MB and
        // would reject large dashboards/imports with 413 (Node accepts them).
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
        // body-parser inflates `Content-Encoding: gzip|deflate` requests.
        .layer(RequestDecompressionLayer::new())
        // zstd is not supported by body-parser; accept it as a Rust-side extension.
        .layer(middleware::from_fn(api::compat::decompress_zstd))
        // express has no 405: a known path with a wrong method falls through to
        // `finalhandler` and gets the HTML 404 page.
        .layer(middleware::from_fn(express_404_for_405))
        // isolation headers + READ_ONLY_MODE (express `afterAuth` middlewares)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::compat::guards,
        ))
        // express' automatic OPTIONS response (200 + Allow)
        .layer(middleware::from_fn(api::options::handle_options))
        // Outermost layers: pino-style lifecycle logs, request id and the
        // headers express sets on every response (cache-control, charset).
        .layer(middleware::from_fn_with_state(
            state.clone(),
            us_rs::logging::middleware,
        ))
        // Internal data-api traffic asks for MessagePack; the public UI keeps JSON.
        .layer(middleware::from_fn(api::msgpack::negotiate))
        .with_state(state.clone());

    let router = if state.config.request_timeout_ms > 0 {
        router.layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_millis(state.config.request_timeout_ms),
        ))
    } else {
        router
    };

    // Outermost: Prometheus counters/histogram (also covers the timeout layer).
    router.layer(middleware::from_fn(us_rs::metrics::middleware))
}

/// express answers an unknown method on a known path with the HTML 404 page
/// (`finalhandler`), not with 405 + `Allow`.
async fn express_404_for_405(
    mut request: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    // body-parser parses JSON before express routing: a method that is not
    // allowed on an existing path with a broken/primitive JSON body gets
    // `400 Invalid JSON supplied` instead of the 404 finalhandler.
    if method != Method::GET && method != Method::HEAD && method != Method::OPTIONS {
        let is_json = request
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_ascii_lowercase().starts_with("application/json"))
            .unwrap_or(false);

        if is_json {
            let allowed = us_rs::api::options::allow_for(&path)
                .map(|allow| allow.split(',').any(|item| item == method.as_str()))
                .unwrap_or(false);

            if !allowed {
                let (parts, body) = request.into_parts();
                match axum::body::to_bytes(body, 50 * 1024 * 1024).await {
                    Ok(bytes) if !bytes.is_empty() => {
                        let valid = serde_json::from_slice::<serde_json::Value>(&bytes)
                            .map(|value| value.is_object() || value.is_array())
                            .unwrap_or(false);
                        if !valid {
                            return (
                                StatusCode::BAD_REQUEST,
                                [(
                                    axum::http::header::CONTENT_TYPE,
                                    "text/plain; charset=utf-8",
                                )],
                                "Invalid JSON supplied",
                            )
                                .into_response();
                        }
                        request = axum::extract::Request::from_parts(
                            parts,
                            axum::body::Body::from(bytes),
                        );
                    }
                    Ok(_) => {
                        request =
                            axum::extract::Request::from_parts(parts, axum::body::Body::empty());
                    }
                    Err(_) => {
                        return (StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large")
                            .into_response();
                    }
                }
            }
        }
    }

    let response = next.run(request).await;

    if response.status() == StatusCode::METHOD_NOT_ALLOWED && method != Method::OPTIONS {
        let method = Method::from_bytes(method.as_str().as_bytes()).unwrap_or(Method::GET);
        return us_rs::api::not_found::response(&method, &path);
    }

    response
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};

        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => tracing::error!(error = %error, "cannot install SIGTERM handler"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT received, shutting down"),
        _ = terminate => tracing::info!("SIGTERM received, shutting down"),
    }
}

/// `us-rs --healthcheck [/ping-db]` — container healthcheck without curl, so the
/// image can be distroless/scratch. Exit code 0 when the endpoint answers 200.
fn run_healthcheck() -> i32 {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let path = std::env::args()
        .skip_while(|arg| arg != "--healthcheck")
        .nth(1)
        .filter(|arg| arg.starts_with('/'))
        .unwrap_or_else(|| "/ping-db".to_string());
    let port: u16 = std::env::var("APP_PORT")
        .or_else(|_| std::env::var("PORT"))
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(8080);
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

    if String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.1 200")
        || String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.0 200")
    {
        0
    } else {
        1
    }
}

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--healthcheck") {
        std::process::exit(run_healthcheck());
    }

    // `US_RS_WORKERS` pins the tokio worker count; by default tokio uses one
    // worker per CPU. The rest of the process (DB I/O, JSON) is fully async.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();

    if let Some(workers) = std::env::var("US_RS_WORKERS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
    {
        builder.worker_threads(workers);
    }

    builder.build()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    us_rs::logging::init_tracing();

    let config = Arc::new(Config::from_env());

    tracing::info!(
        app = %config.app_name,
        auth_enabled = config.auth_enabled,
        pool_max = config.pg_pool_max,
        upstream_version = us_rs::metrics::UPSTREAM_VERSION,
        "starting us-rs"
    );

    // Apply pending migrations when asked to (`US_RS_MIGRATE=1`); the startup
    // `schema::check_version` below applies them anyway, like the Node preflight.
    let db = Db::connect(&config).await?;
    db.wait_until_ready(DB_READY_TIMEOUT).await;

    let args: Vec<String> = std::env::args().collect();
    let migrate_only = args.iter().any(|arg| arg == "--migrate");
    let migrate_on_start = std::env::var("US_RS_MIGRATE")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);

    let seed_demo = args.iter().any(|arg| arg == "--seed-demo");

    if seed_demo {
        match us_rs::schema::seed_demo(&db).await {
            Ok(true) => tracing::info!("demo workbook created"),
            Ok(false) => tracing::info!("demo workbook already exists"),
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "demo seed failed");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if migrate_only || migrate_on_start {
        match us_rs::schema::bootstrap(&db).await {
            Ok(true) => tracing::info!("database schema created"),
            Ok(false) => tracing::info!("database schema already present"),
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "schema bootstrap failed");
                std::process::exit(1);
            }
        }

        if migrate_only {
            return Ok(());
        }
    }

    let skip_schema_check = std::env::var("US_RS_SKIP_SCHEMA_CHECK")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);

    if !skip_schema_check {
        if let Err(error) = us_rs::schema::check_version(&db).await {
            tracing::error!(error = %error, "database schema version check failed");
            std::process::exit(1);
        }
    }

    if !db.schema_ready().await {
        tracing::error!(
            "database schema is missing (no public.entries). Run the official datalens-us \
             migration step once (or restore a dump) before starting us-rs."
        );
        std::process::exit(1);
    }

    let state = AppState {
        config: config.clone(),
        db,
    };

    let listener =
        tokio::net::TcpListener::bind(format!("{}:{}", config.host, config.port)).await?;
    let address = listener.local_addr()?;

    tracing::info!(%address, "us-rs is listening");

    axum::serve(
        listener,
        build_router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    tracing::info!("us-rs stopped");

    Ok(())
}
