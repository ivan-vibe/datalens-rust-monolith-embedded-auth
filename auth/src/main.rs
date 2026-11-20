//! `datalens-auth-rs` — standalone Rust replacement for the DataLens auth container.
//!
//! Same HTTP protocol, cookies, JWT claims and PostgreSQL tables as
//! `ghcr.io/datalens-tech/datalens-auth:0.27.0`, so mono-ui and the other services can use it
//! without changes. The service itself lives in `lib.rs`; this binary only wires the logger,
//! the bind address and the Ctrl-C/TERM shutdown, so the monolith can embed the same code.

use anyhow::Result;

async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}

#[tokio::main]
async fn main() -> Result<()> {
    datalens_auth_rs::init_tracing();
    datalens_auth_rs::serve("0.0.0.0", shutdown_signal()).await
}
