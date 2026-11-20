//! Health and introspection endpoints — port of `src/controllers/helpers.ts`
//! (registered directly on express in `src/index.ts`, therefore unauthenticated).

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::state::AppState;

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `GET /` — `res.send('hey')`.
pub async fn home() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], "hey").into_response()
}

/// `GET /ping`.
pub async fn ping() -> Response {
    no_store(Json(json!({"result": "pong"})).into_response())
}

/// `GET /ping-db` — health of the replica.
pub async fn ping_db(State(state): State<AppState>) -> Response {
    if state.db.ping_replica().await {
        no_store(Json(json!({"result": "pong-db"})).into_response())
    } else {
        tracing::error!("PING_FAILED");
        no_store((StatusCode::BAD_GATEWAY, Json(json!({"result": false}))).into_response())
    }
}

/// `GET /ping-db-primary`.
pub async fn ping_db_primary(State(state): State<AppState>) -> Response {
    if state.db.ping_primary().await {
        no_store(Json(json!({"result": "db primary is ok"})).into_response())
    } else {
        tracing::error!("PING_FAILED");
        no_store(
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"result": "db primary is not available"})),
            )
                .into_response(),
        )
    }
}

/// `GET /pool`.
pub async fn pool(State(state): State<AppState>) -> Response {
    let result = json!({
        "primary": state.db.pool_stats(&state.db.primary),
        "replica": state.db.pool_stats(&state.db.replica),
    });

    no_store(Json(json!({"result": result})).into_response())
}
