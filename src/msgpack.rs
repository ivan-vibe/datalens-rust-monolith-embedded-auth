//! Internal data-api transport: MessagePack responses, identity encoding.
//!
//! The browser contract stays JSON. This module is only for charts-engine -> data-api: the
//! request body is still JSON (small and already available), the response is decoded according
//! to `Content-Type`, so an old data-api that answers JSON keeps working.

use axum::http::StatusCode;
use reqwest::header::CONTENT_TYPE;
use crate::local_http::{RequestBuilder, Response};
use serde_json::Value;

pub const MSGPACK: &str = "application/msgpack";

/// `DL_MSGPACK` is deployment-static; cache it instead of reading the environment on
/// every engine -> data-api call.
fn msgpack_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("DL_MSGPACK")
            .map(|value| value != "0")
            .unwrap_or(true)
    })
}

/// Ask data-api for MessagePack and disable compression on this internal hop. Compression is
/// left for the final JSON answer to the browser.
pub fn request(builder: RequestBuilder) -> RequestBuilder {
    // Static headers: no HeaderName/HeaderValue parsing on every internal data-api call.
    let builder = builder.header_static("accept-encoding", "identity");
    if msgpack_enabled() {
        builder.header_static("accept", MSGPACK)
    } else {
        builder
    }
}

pub async fn decode(response: Response) -> Result<Value, (StatusCode, String)> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, format!("data-api body: {error}")))?;
    if content_type.contains(MSGPACK) {
        rmp_serde::from_slice(&bytes).map_err(|error| {
            (
                StatusCode::BAD_GATEWAY,
                format!("data-api msgpack: {error}"),
            )
        })
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|error| (StatusCode::BAD_GATEWAY, format!("data-api json: {error}")))
    }
}

