//! Library form of `datalens-auth-rs`, so the monolith can serve the auth protocol from
//! inside its own process (single-binary fork). The standalone binary in `main.rs` is a thin
//! wrapper around [`serve`] and behaves exactly like the original service.
//!
//! Protocol, cookies, JWT claims and PostgreSQL tables are the same as
//! `ghcr.io/datalens-tech/datalens-auth:0.27.0`.
#![allow(clippy::result_large_err)]

mod bootstrap;
mod config;
mod cookies;
mod db;
mod guards;
mod http;
mod ids;
mod jwt;
mod models;
mod password;
mod routes;
mod users_api;
mod validation;

use anyhow::{anyhow, Context, Result};
use std::future::Future;
use std::sync::Arc;

use config::Config;
use db::Db;
use jwt::Jwt;

pub struct AppState {
    pub config: Config,
    pub jwt: Jwt,
    pub db: Db,
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{name} is required (PEM key)"))
}

/// Initialise the service logger. `RUST_LOG` selects the filter; calling it twice is fine
/// (the second `try_init` is ignored), which matters when the monolith already installed one.
pub fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init();
}

/// Reject the service-internal `/private/*` surface for non-loopback peers.
async fn internal_only(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let private = request
        .uri()
        .path()
        .strip_prefix("/private")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
    if private {
        let loopback = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|axum::extract::ConnectInfo(address)| address.ip().is_loopback())
            .unwrap_or(false);
        if !loopback {
            return (axum::http::StatusCode::FORBIDDEN, "private API is internal").into_response();
        }
    }
    next.run(request).await
}

/// Serve the auth HTTP API on `bind_host:AUTH_PORT` (or `PORT`, default `8097`).
///
/// The caller supplies the shutdown future: the standalone binary passes Ctrl-C/TERM, the
/// embedded mode passes `std::future::pending()` and lives as long as the monolith process.
pub async fn serve<F>(bind_host: &str, shutdown: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let config = Config::from_env().context("invalid auth-rs configuration")?;
    let jwt = Jwt::new(
        &required_env("TOKEN_PRIVATE_KEY")?,
        &required_env("TOKEN_PUBLIC_KEY")?,
        config.access_ttl,
    )
    .context("invalid TOKEN_PRIVATE_KEY/TOKEN_PUBLIC_KEY")?;
    let db = Db::new(config.dsn.clone(), config.db_pool_size).await?;
    let port = config.port;
    tracing::info!(
        port,
        dsn = %config.dsn,
        access_ttl = config.access_ttl,
        refresh_ttl = config.refresh_ttl,
        session_ttl = config.session_ttl,
        signup_disabled = config.signup_disabled,
        "datalens-auth-rs listening"
    );

    let state = Arc::new(AppState { config, jwt, db });
    let app = routes::router(state).layer(axum::middleware::from_fn(internal_only));
    let listener = tokio::net::TcpListener::bind((bind_host, port))
        .await
        .with_context(|| format!("bind {bind_host}:{port}"))?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .context("serve")?;
    Ok(())
}
