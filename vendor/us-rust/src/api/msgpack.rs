//! `Accept: application/msgpack` for the private metadata endpoints.
//!
//! The stored JSON is still produced by the existing handlers; this layer converts a successful
//! `application/json` response to MessagePack after the handler ran. Error responses stay JSON
//! so a client that asked for MessagePack can still read `{"code"/"message"}`-style errors.

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::{header, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

const MSGPACK: &str = "application/msgpack";

fn wants_msgpack(request: &Request) -> bool {
    request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains(MSGPACK))
}

pub async fn negotiate(request: Request, next: Next) -> Response {
    if !wants_msgpack(&request) {
        return next.run(request).await;
    }
    let response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !is_json {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, 50 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "cannot buffer a JSON response for MessagePack");
            return Response::from_parts(parts, Body::empty());
        }
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "cannot parse a JSON response for MessagePack");
            return Response::from_parts(parts, Body::from(bytes));
        }
    };
    let encoded = match rmp_serde::to_vec_named(&value) {
        Ok(encoded) => encoded,
        Err(error) => {
            tracing::warn!(%error, "cannot encode a MessagePack response");
            return Response::from_parts(parts, Body::from(bytes));
        }
    };
    let len = encoded.len();
    let mut response = Response::from_parts(parts, Body::from(encoded));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(MSGPACK));
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).unwrap_or(HeaderValue::from_static("0")),
    );
    response
}
