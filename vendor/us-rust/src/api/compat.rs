//! Request guards that express applies before the controllers:
//! * `x-dl-workbookid` / `x-dl-datasetid` isolation headers are decoded (a
//!   malformed value is a 400 with a dedicated error code);
//! * `READ_ONLY_MODE` rejects mutating routes with 451.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::CONTENT_ENCODING;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::id::decode_id;
use crate::state::AppState;

/// `DL_WORKBOOK_ID_HEADER` / `DL_DATASET_ID_HEADER`.
const DL_WORKBOOK_ID_HEADER: &str = "x-dl-workbookid";
const DL_DATASET_ID_HEADER: &str = "x-dl-datasetid";

/// POST endpoints that are reads; every other POST and every DELETE is a write
/// (`write: true` in `src/routes.ts`).
const READ_ONLY_POST_ROUTES: [&str; 10] = [
    "/v1/get-entries",
    "/private/v1/get-entries",
    "/v1/check-entries-existence",
    "/v1/get-entries-data",
    "/v1/get-entries-meta",
    "/v1/get-entries-annotation",
    "/v1/get-entries-relations",
    "/private/v1/get-entries-relations",
    "/v1/collections-get-list-by-ids",
    "/v2/workbooks-get-list-by-ids",
];

fn is_write_route(method: &str, matched_path: &str) -> bool {
    let path = matched_path.trim_end_matches('/');

    if method == "DELETE" {
        return true;
    }

    if method == "POST" {
        return !READ_ONLY_POST_ROUTES.contains(&path);
    }

    false
}

pub async fn guards(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let matched_path = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|path| path.as_str().to_string())
        .unwrap_or_default();

    // `/ping*`, `/`, `/pool` are registered outside the middleware chain in
    // the Node app, so the isolation headers are not checked there.
    let is_api_route = matched_path.starts_with("/v1")
        || matched_path.starts_with("/v2")
        || matched_path.starts_with("/private");

    if !is_api_route {
        return next.run(request).await;
    }

    // Copy the header values out: holding a borrow of the request across
    // `next.run(...).await` would make the middleware future non-Send.
    let workbook_header = request
        .headers()
        .get(DL_WORKBOOK_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let dataset_header = request
        .headers()
        .get(DL_DATASET_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    if let Some(value) = workbook_header {
        if decode_id(&value).is_err() {
            return ApiError::new(
                StatusCode::BAD_REQUEST,
                Some("INCORRECT_WORKBOOK_ID_HEADER"),
                "Workbook id header is incorrect",
            )
            .into_response();
        }
    }

    if let Some(value) = dataset_header {
        if decode_id(&value).is_err() {
            return ApiError::new(
                StatusCode::BAD_REQUEST,
                Some("INCORRECT_DATASET_ID_HEADER"),
                "Dataset id header is incorrect",
            )
            .into_response();
        }
    }

    if state.config.auth_enabled && matched_path.starts_with("/private") {
        if let Some(response) = private_route_denied(&state, &request) {
            return response;
        }
    }

    if state.config.read_only_mode && is_write_route(request.method().as_str(), &matched_path) {
        return (
            StatusCode::from_u16(451).unwrap(),
            Json(json!({"code": "READ_ONLY_MODE_ENABLED"})),
        )
            .into_response();
    }

    next.run(request).await
}

/// `Content-Encoding: zstd` request bodies. Node's `body-parser` rejects zstd
/// (500); the Rust service accepts it as an extension. Runs before the
/// gzip/deflate decompression layer, which would answer 415 for unknown codecs.
pub async fn decompress_zstd(request: Request, next: Next) -> Response {
    let is_zstd = request
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("zstd"))
        .unwrap_or(false);

    if !is_zstd {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();

    let bytes = match axum::body::to_bytes(body, 50 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };

    let decoded = match zstd::stream::decode_all(bytes.as_ref()) {
        Ok(decoded) => decoded,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "Invalid compressed body").into_response();
        }
    };

    parts.headers.remove(CONTENT_ENCODING);
    let request = Request::from_parts(parts, Body::from(decoded));
    next.run(request).await
}

fn dynamic_token_service_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("serviceId")?.as_str().map(str::to_owned)
}

fn verify_dynamic_token(token: &str, keys: &[String]) -> bool {
    keys.iter().any(|key| {
        DecodingKey::from_rsa_pem(key.as_bytes())
            .ok()
            .map(|key| {
                let mut validation = Validation::new(Algorithm::RS256);
                // jsonwebtoken requires `exp` by default; Node's `jsonwebtoken`
                // verifies it only when present.
                validation.required_spec_claims.clear();
                validation.validate_exp = false;
                validation.validate_aud = false;
                decode::<Value>(token, &key, &validation).is_ok()
            })
            .unwrap_or(false)
    })
}

/// `resolvePrivateRoute`: with auth enabled, `/private/*` requires either a
/// valid dynamic master token (`x-us-dynamic-master-token`) or the static
/// `x-us-master-token`. With auth disabled express authorises private routes
/// unconditionally, so the check is skipped.
fn private_route_denied(state: &AppState, request: &Request) -> Option<Response> {
    // `resolvePrivateRoute`: when `DynamicMasterTokenEnabled` is off the whole
    // dynamic branch (and the "required" rule) is skipped.
    let dynamic_enabled = state.config.dynamic_master_token_enabled;
    let dynamic = dynamic_enabled
        .then(|| {
            request
                .headers()
                .get("x-us-dynamic-master-token")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
        })
        .flatten();

    if let Some(token) = dynamic {
        let Some(service_id) = dynamic_token_service_id(token) else {
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "Invalid dynamic master token format"})),
                )
                    .into_response(),
            );
        };

        let keys: &[String] = match service_id.as_str() {
            "ui" => &state.config.dynamic_master_keys_ui,
            "bi" => &state.config.dynamic_master_keys_bi,
            _ => &[],
        };

        if keys.is_empty() {
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": format!("Public key not found for serviceId: {service_id}")})),
                )
                    .into_response(),
            );
        }

        if !verify_dynamic_token(token, keys) {
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "Invalid dynamic master token"})),
                )
                    .into_response(),
            );
        }

        return None;
    }

    if dynamic_enabled && state.config.dynamic_master_token_is_required {
        return Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Dynamic master token is required"})),
            )
                .into_response(),
        );
    }

    if state.config.master_tokens.is_empty() {
        return Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "No master token in config"})),
            )
                .into_response(),
        );
    }

    let request_token = request
        .headers()
        .get("x-us-master-token")
        .and_then(|value| value.to_str().ok());

    // `getEnvTokenVariable` splits MASTER_TOKEN by comma; the middleware uses
    // `masterToken.includes(requestToken)` on the resulting array (exact match).
    match request_token {
        Some(token)
            if state
                .config
                .master_tokens
                .iter()
                .any(|known| known == token) =>
        {
            None
        }
        _ => Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Private API call denied"})),
            )
                .into_response(),
        ),
    }
}
