//! Per-request memo for connection resolution.
//!
//! A dashboard chart compiles two or three queries against the same connection; with
//! `DL_CONNECTION_CACHE_TTL_SECS=0` (the deterministic test profile) every query would read
//! the connection entry from `us` again. The memo is scoped to one HTTP request, so the
//! cross-request freshness policy stays exactly what the TTL says while a single request
//! resolves each connection once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;

use crate::executor::ConnectionTarget;

tokio::task_local! {
    static CONNECTIONS: RefCell<HashMap<String, ConnectionTarget>>;
}

/// Run `future` with an empty per-request connection memo.
pub async fn with_connection_memo<F: Future>(future: F) -> F::Output {
    CONNECTIONS.scope(RefCell::new(HashMap::new()), future).await
}

/// The connection already resolved during this request, if any.
pub fn get(connection_id: &str) -> Option<ConnectionTarget> {
    CONNECTIONS
        .try_with(|memo| memo.borrow().get(connection_id).cloned())
        .ok()
        .flatten()
}

/// Remember a resolved connection for the rest of this request.
pub fn insert(connection_id: &str, target: &ConnectionTarget) {
    let _ = CONNECTIONS.try_with(|memo| {
        memo.borrow_mut()
            .insert(connection_id.to_string(), target.clone());
    });
}
