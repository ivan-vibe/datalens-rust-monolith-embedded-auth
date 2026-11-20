//! HTTP plumbing shared by the auth and user-management handlers: expresskit response
//! headers, JSON/text/404 bodies, request-id propagation and cookie-header helpers.

use axum::body::Bytes;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use serde::Serialize;
use serde_json::Value;

pub const CACHE_CONTROL: &str = "no-store, max-age=0, must-revalidate, proxy-revalidate";

/// Incoming `x-request-id` is preserved; otherwise a fresh UUID v4 is generated (expresskit).
pub fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

pub fn response(status: StatusCode, content_type: &'static str, body: Vec<u8>, request_id: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header("cache-control", CACHE_CONTROL)
        .header("surrogate-control", "no-store")
        .header("x-request-id", request_id)
        .body(axum::body::Body::from(body))
        .expect("static headers")
}

pub fn json_body<T: Serialize>(status: StatusCode, value: &T, request_id: &str) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    response(status, "application/json; charset=utf-8", body, request_id)
}

pub fn json_value(status: StatusCode, value: Value, request_id: &str) -> Response {
    json_body(status, &value, request_id)
}

pub fn text_response(status: StatusCode, content_type: &'static str, body: &str, request_id: &str) -> Response {
    response(status, content_type, body.as_bytes().to_vec(), request_id)
}

pub fn with_set_cookies(mut response: Response, values: &[String]) -> Response {
    for value in values {
        if let Ok(header_value) = HeaderValue::from_str(value) {
            response.headers_mut().append(header::SET_COOKIE, header_value);
        }
    }
    response
}

pub fn malformed_json(request_id: &str) -> Response {
    text_response(
        StatusCode::BAD_REQUEST,
        "text/html; charset=utf-8",
        "Invalid JSON supplied",
        request_id,
    )
}

/// express `res.status(401).send('Unauthorized access')` → text/html.
pub fn unauthorized(request_id: &str) -> Response {
    text_response(
        StatusCode::UNAUTHORIZED,
        "text/html; charset=utf-8",
        "Unauthorized access",
        request_id,
    )
}

pub fn internal_error(request_id: &str) -> Response {
    json_value(
        StatusCode::INTERNAL_SERVER_ERROR,
        serde_json::json!({"message": "Internal Server Error"}),
        request_id,
    )
}

/// The Node error presenter's default branch for `DBError` instances.
pub fn database_error(request_id: &str) -> Response {
    json_value(
        StatusCode::INTERNAL_SERVER_ERROR,
        serde_json::json!({"message": "Database error"}),
        request_id,
    )
}

pub fn validation_error(request_id: &str, details: Vec<Value>) -> Response {
    json_value(
        StatusCode::BAD_REQUEST,
        serde_json::json!({
            "code": "AUTH.VALIDATION_ERROR",
            "message": "Validation error",
            "details": details,
        }),
        request_id,
    )
}

pub fn access_denied(request_id: &str) -> Response {
    json_value(
        StatusCode::FORBIDDEN,
        serde_json::json!({
            "message": "You do not have a sufficient permission for this operation",
            "code": "AUTH.ACCESS_DENIED",
        }),
        request_id,
    )
}

pub fn doc_not_found(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'"),
    );
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    response
}

pub async fn not_found(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let body = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>Error</title>\n</head>\n<body>\n<pre>Cannot {method} {}</pre>\n</body>\n</html>\n",
        uri.path()
    );
    doc_not_found(text_response(
        StatusCode::NOT_FOUND,
        "text/html; charset=utf-8",
        &body,
        &request_id(&headers),
    ))
}

/// Node: `req.headers.authorization.slice('Bearer'.length + 1)` — the scheme is not checked,
/// the first seven characters are simply dropped.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.get(7..).unwrap_or(""))
}

pub enum ParsedBody {
    NotJson,
    Malformed,
    Value(Value),
}

pub fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let media_type = value.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    media_type == "application/json" || media_type.ends_with("+json")
}

/// express body-parser runs with `strict: true`: only objects/arrays are valid JSON bodies,
/// primitives are rejected with the same 400 as a syntax error.
pub fn parse_json_body(headers: &HeaderMap, body: &Bytes) -> ParsedBody {
    if !is_json_content_type(headers) {
        return ParsedBody::NotJson;
    }
    if body.is_empty() {
        return ParsedBody::Value(Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => ParsedBody::Value(value),
        Ok(_) | Err(_) => ParsedBody::Malformed,
    }
}
