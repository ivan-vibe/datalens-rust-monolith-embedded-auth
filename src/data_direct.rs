//! v5.2: direct in-process path from the charts engine to the Rust `data-api`.
//!
//! Chart requests used to go `engine -> local_http::Client -> DATA_ROUTER.oneshot`,
//! i.e. a full axum HTTP round trip for every `/fields`, `/result` and `/pivot`
//! call: request construction, extractors, JWT middleware, trace middleware,
//! MessagePack encode of the answer and a MessagePack decode back into
//! `serde_json::Value` (the shape the chart/table/pivot engines consume).
//!
//! The data-api already lives in the same OS process (`data_main` owns its typed
//! `AppState`: the `dl_api::Service` plus the dataset store and the `us` cache).
//! This module installs that state once and calls the same code the HTTP handlers
//! call (`AppState::dataset_with_revision` / `dataset_rev` / `dataset` and
//! `Service::result` / `Service::pivot`), reproducing the handler semantics:
//!
//! * NATIVE JWT validation and the `dl_query` identity scope (`AUTH__TYPE`);
//! * the `x-dl-snapshot` task-local that `dl_us` uses to coalesce/cache a request;
//! * dataset revision pinning for `/result` (and the unpinned lookup `/pivot` uses);
//! * the `{"code", "message"}` error envelope with the same status code;
//! * the `dl_timing` trace line the data-api middleware emits.
//!
//! What disappears is only the transport: the axum request, extractors, the
//! MessagePack encode/decode pair and the response body copy. The public
//! behaviour of the HTTP routes is untouched (the router stays installed), and
//! the `/api/run` JSON contract is the same.
//!
//! When the typed state is not installed — a standalone charts-engine container
//! that talks to a remote data-api — every function falls back to the previous
//! `local_http` transport, so the module is a transparent optimization.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use axum::http::{HeaderMap, StatusCode};
use datalens_server::AppState as DataApiState;
use dl_api::trace::{with_trace, Trace};
use dl_api::{ApiError, DistinctRequest, PivotRequest, ResultRequest};
use dl_core::{Dataset, UserDataType};
use dl_query::identity::{with_identity, Identity};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::local_http::Client;
use crate::typed_data::{DataBody, ResultBody};

/// `x-dl-snapshot` is the request-scoped snapshot key the public middlewares use.
const SNAPSHOT_HEADER: &str = "x-dl-snapshot";

static TYPED: OnceLock<Arc<DataApiState>> = OnceLock::new();

/// Install the data-api state owned by `data_main`; called once at startup.
///
/// Returns `false` when another runtime (a test, a second data-api) installed a
/// state first; the first one wins, like the `DATA_ROUTER` `OnceLock`.
pub fn install(state: Arc<DataApiState>) -> bool {
    let installed = TYPED.set(state).is_ok();
    if installed {
        tracing::info!(
            target: "data_direct",
            "typed data-api chart path installed (fields/result/pivot bypass the local HTTP router)"
        );
    }
    installed
}

/// Whether the typed path is available in this process.
pub fn installed() -> bool {
    // Engine-only mock: force the HTTP fallback so the external mock data-api owns the
    // request/response boundary and no embedded data-api/DB is touched.
    if std::env::var("DL_MOCK_DATA").map(|v| v == "1" || v == "true").unwrap_or(false) {
        return false;
    }
    TYPED.get().is_some()
}

/// Cross-request query-result cache counters from the typed data-api state (v9.0 / task-25).
/// The monolith's `/debug/cache` merges this next to the fields cache.
pub fn query_cache_json() -> Value {
    match TYPED.get() {
        Some(state) => state.service.query_cache_json(),
        None => serde_json::json!({"enabled": false}),
    }
}

/// One typed response as the HTTP route would have answered it.
pub struct QueryOutcome {
    /// The status the route returns: `200 OK` for a query, `ApiError.status`
    /// for a rejected request (`4xx`/`5xx`, JSON error body in [`Self::body`]).
    pub status: StatusCode,
    /// The decoded response body (`ResultResponse` / `PivotResponse` as JSON).
    pub body: Value,
    /// Exact compact-JSON length of the body. The data-api measures it once when it finalizes
    /// the response; no path here serializes the body to learn its size.
    pub json_len: u64,
    /// The `x-dl-data-export` header, when the transport provided one. The
    /// Rust data-api `/result` route does not set it, so the direct path answers
    /// `None` exactly like the HTTP route; the response's `data_export` member
    /// stays inside [`Self::body`] for the table/pivot engines.
    pub data_export: Option<Value>,
}

/// The data-api error body (`{"code", "message", "details"}`) with the status the route would
/// use. `debug` is not carried: the reference processor forwards `details` (its `db_message` is
/// user-safe) but not the diagnostics object.
#[derive(Debug, Clone)]
struct ApiFailure {
    status: u16,
    code: String,
    message: String,
    details: Value,
}

impl ApiFailure {
    fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: json!({}),
        }
    }

    fn from_api(error: &ApiError) -> Self {
        Self {
            status: error.status,
            code: error.code.clone(),
            message: error.message.clone(),
            details: error.details.clone(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "ERR.DS_API", message)
    }

    /// `HttpError`'s body minus `debug`: `{"code", "message"}` plus `details` when non-empty.
    fn body(&self) -> Value {
        let mut body = json!({"code": self.code, "message": self.message});
        if self
            .details
            .as_object()
            .map(|details| !details.is_empty())
            .unwrap_or(false)
        {
            body["details"] = self.details.clone();
        }
        body
    }

    fn status_code(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

fn typed_state() -> Option<Arc<DataApiState>> {
    TYPED.get().cloned()
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let mut parts = value.splitn(2, ' ');
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    parts
        .next()
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// A JWT from `Authorization: Bearer …` or from the UI's `auth` cookie (`URL-encoded JSON`).
fn request_token(headers: &HeaderMap) -> Option<String> {
    if let Some(token) = bearer_token(headers) {
        return Some(token.to_string());
    }
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    let value = cookie
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("auth="))?;
    let decoded = percent_decode(value);
    let parsed: serde_json::Value = serde_json::from_str(&decoded).ok()?;
    parsed
        .get("accessToken")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// Minimal percent-decoder for the `auth` cookie value (`%7B`-style, no `+` semantics).
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Gate for the control-api routes: the request must carry a **valid** NATIVE token. The old
/// check only looked at header presence, so a `Bearer garbage` header reached the handlers,
/// which then wrote through the service (master) token and bypassed US permissions.
pub fn require_authenticated_user(headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let auth_enabled = std::env::var("AUTH_ENABLED")
        .map(|value| value == "true" || value == "1")
        .unwrap_or(true);
    let Some(state) = typed_state() else {
        return if auth_enabled {
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "control-api requires the in-process auth state".to_string(),
            ))
        } else {
            Ok(())
        };
    };
    let Some(auth) = state.auth.as_ref() else {
        return if auth_enabled {
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "control-api requires authentication to be configured".to_string(),
            ))
        } else {
            Ok(())
        };
    };
    let Some(token) = request_token(headers) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            "ERR.DS_API.REQUEST.AUTH: User access token header is missing".to_string(),
        ));
    };
    auth.validate(&token).map(|_| ()).map_err(|error| {
        (
            StatusCode::UNAUTHORIZED,
            format!("ERR.DS_API.REQUEST.AUTH: Invalid user access token: {error}"),
        )
    })
}

tokio::task_local! {
    /// The bearer token and its validated identity for the whole `/api/run` request. One chart
    /// request fans out into 2-3 in-process data-api calls (`/fields` + `/result`), and each of
    /// them used to re-run the RSA-PSS JWT verification (26% of busy CPU in the profile).
    static REQUEST_IDENTITY: (String, Identity);
}

/// Gate for the engine's `/debug/*` routes.
///
/// The public port must not expose process internals. With `AUTH_ENABLED` the caller needs a
/// valid NATIVE token carrying an admin role; without auth (local lab) the routes stay open,
/// exactly like the rest of the engine. Returns the HTTP error to answer with.
pub fn require_debug_admin(headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let Some(state) = typed_state() else {
        // Engine-only mock: there is no in-process data-api/auth state, but the debug
        // allocation/stage endpoints are exactly what the profiling harness needs.
        if std::env::var("DL_MOCK_DATA").map(|v| v == "1" || v == "true").unwrap_or(false) {
            return Ok(());
        }
        // No auth state installed (standalone engine without the data-api shared state):
        // fail closed unless the deployment explicitly disabled auth (`AUTH_ENABLED=0`, a
        // local lab profile) or asked for a mock. An unset AUTH_ENABLED used to open the
        // process internals; the default is now "auth is on".
        let auth_enabled = std::env::var("AUTH_ENABLED")
            .map(|value| value == "true" || value == "1")
            .unwrap_or(true);
        if auth_enabled {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "debug endpoints require the in-process auth state".to_string(),
            ));
        }
        return Ok(());
    };
    let Some(auth) = state.auth.as_ref() else {
        return Ok(());
    };
    let Some(token) = bearer_token(headers) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            "ERR.DS_API.REQUEST.AUTH: User access token header is missing".to_string(),
        ));
    };
    let claims = auth.validate(token).map_err(|error| {
        (
            StatusCode::UNAUTHORIZED,
            format!("ERR.DS_API.REQUEST.AUTH: Invalid user access token: {error}"),
        )
    })?;
    let allowed = claims.roles.iter().any(|role| {
        matches!(
            role.as_str(),
            "admin" | "datalens.admin" | "datalens.superuser"
        )
    });
    if allowed {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "ERR.DS_API.PERMISSION: debug endpoints require an admin role".to_string(),
        ))
    }
}

/// Validate the caller's token once and remember it for every data-api call made by `future`.
///
/// An absent, invalid or expired token is not cached: the first data-api call re-validates it
/// and returns the same 401 it returned before, so the trust boundary is unchanged.
fn per_user_us_enabled() -> bool {
    std::env::var("DL_US_PER_USER")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

pub async fn scope_identity<F, T>(headers: &HeaderMap, future: F) -> T
where
    F: Future<Output = T>,
{
    // The US access context is installed even without a token: workbook isolation and the
    // per-identity cache separation do not depend on JWT validation, exactly like the US route
    // checks `x-dl-workbookid` on every request.
    let mut access = crate::us_direct::AccessContext {
        identity: "service".to_string(),
        workbook_id: headers
            .get("x-dl-workbookid")
            .and_then(|value| value.to_str().ok())
            .and_then(crate::us_direct::decode_entry_id),
        sudo: headers.get("x-dl-sudo").is_some(),
        allow_superuser: headers
            .get("x-dl-allow-superuser")
            .and_then(|value| value.to_str().ok())
            .map(|value| matches!(value.trim(), "1" | "true" | "yes"))
            .unwrap_or(false),
    };

    if let Some(state) = typed_state() {
        if let Some(auth) = state.auth.as_ref() {
            if let Some(token) = bearer_token(headers) {
                if let Ok(claims) = auth.validate(token) {
                    access.identity = format!(
                        "{}|{}|{}",
                        claims.user_id,
                        claims.groups.join(","),
                        claims.roles.join(",")
                    );
                    let identity = Identity {
                        user_id: claims.user_id,
                        groups: claims.groups,
                        roles: claims.roles,
                    };
                    return crate::us_direct::with_access(
                        access,
                        REQUEST_IDENTITY.scope((token.to_string(), identity), future),
                    )
                    .await;
                }
            }
        }
    }
    crate::us_direct::with_access(access, future).await
}

/// Run `future` inside the same request scope the data-api middlewares install:
/// `x-dl-snapshot` outside, the NATIVE identity inside (when configured).
async fn with_scope<F, T>(
    state: &Arc<DataApiState>,
    headers: &HeaderMap,
    future: F,
) -> Result<T, ApiFailure>
where
    F: Future<Output = Result<T, ApiError>>,
{
    if let Ok((cached_token, identity)) =
        REQUEST_IDENTITY.try_with(|(token, identity)| (token.clone(), identity.clone()))
    {
        if bearer_token(headers) == Some(cached_token.as_str()) {
            // G02: make the caller's token visible to the data-api's US client so it reads
            // the dataset on behalf of the user instead of the service master token.
            if !per_user_us_enabled() {
                return with_snapshot(headers, with_identity(identity, future))
                    .await
                    .map_err(|error| ApiFailure::from_api(&error));
            }
            return datalens_server::auth::with_bearer(
                cached_token,
                with_snapshot(headers, with_identity(identity, future)),
            )
            .await
            .map_err(|error| ApiFailure::from_api(&error));
        }
    }
    match state.auth.as_ref() {
        Some(auth) => {
            let Some(token) = bearer_token(headers) else {
                return Err(ApiFailure::new(
                    401,
                    "ERR.DS_API.REQUEST.AUTH",
                    "User access token header is missing",
                ));
            };
            let token = token.to_string();
            let started = Instant::now();
            let validated = auth.validate(&token);
            dl_api::trace::record_work("auth", started.elapsed());
            match validated {
                Ok(claims) => {
                    let identity = Identity {
                        user_id: claims.user_id,
                        groups: claims.groups,
                        roles: claims.roles,
                    };
                    // G02: the data-api's `us` client reads the dataset with the caller's
                    // token (`USER_BEARER`), so ACL/tenant checks see the real user instead
                    // of the service master token.
                    if !per_user_us_enabled() {
                        return with_snapshot(headers, with_identity(identity, future))
                            .await
                            .map_err(|error| ApiFailure::from_api(&error));
                    }
                    datalens_server::auth::with_bearer(
                        token,
                        with_snapshot(headers, with_identity(identity, future)),
                    )
                    .await
                    .map_err(|error| ApiFailure::from_api(&error))
                }
                Err(error) => Err(ApiFailure::new(
                    401,
                    "ERR.DS_API.REQUEST.AUTH",
                    format!("Invalid user access token: {error}"),
                )),
            }
        }
        None => with_snapshot(headers, future)
            .await
            .map_err(|error| ApiFailure::from_api(&error)),
    }
}

async fn with_snapshot<F: Future>(headers: &HeaderMap, future: F) -> F::Output {
    let snapshot = headers
        .get(SNAPSHOT_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    match snapshot {
        Some(snapshot) => dl_us::SNAPSHOT.scope(snapshot, future).await,
        None => future.await,
    }
}

/// Decode a request body the way `ApiJson` does.
///
/// The engine already holds the body as a `serde_json::Value`, so the fast path
/// deserializes straight from the reference (no clone, no re-serialization).
/// Only a rejected body takes the byte path, which reproduces axum's exact `Json`
/// rejection text that `ApiJson` wraps into `400 ERR.DS_API.REQUEST`.
static DECODE_CALLS: AtomicU64 = AtomicU64::new(0);
static DECODE_FALLBACKS: AtomicU64 = AtomicU64::new(0);

pub fn decode_stats_json() -> serde_json::Value {
    serde_json::json!({
        "calls": DECODE_CALLS.load(Ordering::Relaxed),
        "fallbacks": DECODE_FALLBACKS.load(Ordering::Relaxed),
    })
}

fn decode_request<T: DeserializeOwned>(body: &Value) -> Result<T, ApiFailure> {
    DECODE_CALLS.fetch_add(1, Ordering::Relaxed);
    if let Ok(value) = T::deserialize(body) {
        return Ok(value);
    }
    DECODE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
    let bytes = serde_json::to_vec(body).map_err(|error| ApiFailure::internal(error.to_string()))?;
    match axum::Json::<T>::from_bytes(&bytes) {
        Ok(axum::Json(value)) => Ok(value),
        Err(rejection) => Err(ApiFailure::new(400, "ERR.DS_API.REQUEST", rejection.body_text())),
    }
}

/// The `GET /api/data/v2/datasets/{ds}/fields` payload
/// (`datalens-server`'s `DatasetFieldsResponseSchema`, kept in sync with the
/// private `dataset_fields` builder of that crate).
fn fields_payload(dataset: &Dataset, revision: Option<&str>) -> Value {
    let fields: Vec<Value> = dataset
        .fields
        .iter()
        .map(|field| {
            let dtype = field
                .data_type
                .or(field.cast)
                .or(field.initial_data_type)
                .unwrap_or(UserDataType::String);
            json!({
                "title": field.title,
                "guid": field.guid,
                "data_type": dtype,
                "cast": dtype,
                "hidden": field.hidden,
                "type": dataset.field_type(field),
                "calc_mode": field.calc_mode,
                "ui_settings": field.ui_settings,
                "description": field.description,
                "formula": field.formula,
                "aggregation": field.aggregation,
                "default_value": field.default_value,
            })
        })
        .collect();
    json!({ "fields": fields, "revision_id": revision })
}

/// Emit the `dl_timing` line the data-api `trace_request` middleware emits.
fn log_trace(method: &str, path: &str, trace: &Trace, direct: bool) {
    let summary = trace.summary();
    tracing::info!(
        target: "dl_timing",
        method = %method,
        path = %path,
        direct = direct,
        total_us = summary.total.as_secs_f64() * 1_000_000.0,
        work_us = summary.work.as_secs_f64() * 1_000_000.0,
        wait_us = summary.wait.as_secs_f64() * 1_000_000.0,
        stages = %summary.stages_json(),
        "request timing"
    );
}

async fn traced<T, F: Future<Output = T>>(method: &str, path: &str, future: F) -> T {
    // The trace exists only to emit the `dl_timing` info line. When that target is disabled
    // (the benchmark runs with `RUST_LOG=warn`), skip the per-call `Arc<Trace>`, the
    // task-local scope and the per-stage recording; the service treats a missing trace as a
    // no-op, so behaviour is identical apart from the log line.
    if !tracing::enabled!(target: "dl_timing", tracing::Level::INFO) {
        return future.await;
    }
    let trace = Arc::new(Trace::new());
    let value = with_trace(Arc::clone(&trace), future).await;
    log_trace(method, path, &trace, true);
    value
}

// ---------------------------------------------------------------------------
// `/fields`
// ---------------------------------------------------------------------------

/// `GET /api/data/v2/datasets/{ds_id}/fields` as `(fields, revision_id)`.
///
/// Same tuple the old HTTP helper returned; the cache/flight logic in
/// `pipeline::dataset_fields` stays where it is.
pub async fn fetch_fields(
    http: &Client,
    data_api: &str,
    headers: &HeaderMap,
    dataset: &str,
) -> Result<(Value, Value), (StatusCode, String)> {
    if let Some(fields) = crate::bench_mock::fields(dataset) {
        return Ok(fields);
    }
    let Some(state) = typed_state() else {
        return fetch_fields_http(http, data_api, headers, dataset).await;
    };
    let path = format!("/api/data/v2/datasets/{dataset}/fields");
    let direct = Arc::clone(&state);
    let payload = traced("GET", &path, async {
        with_scope(&state, headers, async {
            let (dataset, revision) = direct.dataset_with_revision(dataset).await?;
            Ok(fields_payload(&dataset, revision.as_deref()))
        })
        .await
    })
    .await;
    match payload {
        // Move the two members out of the payload instead of cloning them back out of it.
        Ok(Value::Object(mut body)) => Ok((
            body.remove("fields").unwrap_or_else(|| json!([])),
            body.remove("revision_id").unwrap_or(Value::Null),
        )),
        Ok(_) => Ok((json!([]), Value::Null)),
        Err(failure) => Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "data-api fields {dataset}: {} {}",
                failure.status_code(),
                failure.body()
            ),
        )),
    }
}

async fn fetch_fields_http(
    http: &Client,
    data_api: &str,
    headers: &HeaderMap,
    dataset: &str,
) -> Result<(Value, Value), (StatusCode, String)> {
    let response = crate::msgpack::request(
        http.get(format!(
            "{data_api}/api/data/v2/datasets/{dataset}/fields"
        ))
        .headers(headers.clone()),
    )
    .send()
    .await
    .map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            format!("data-api fields: {error}"),
        )
    })?;
    let status = response.status();
    let body: Value = crate::msgpack::decode(response).await?;
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("data-api fields {dataset}: {status} {body}"),
        ));
    }
    Ok((
        body.get("fields").cloned().unwrap_or_else(|| json!([])),
        body.get("revision_id").cloned().unwrap_or(Value::Null),
    ))
}

// ---------------------------------------------------------------------------
// `/result`
// ---------------------------------------------------------------------------

/// `POST /api/data/v2/datasets/{ds_id}/result`.
///
/// Success and data-api errors both come back as [`QueryOutcome`] status+body,
/// exactly like the HTTP response does (the call sites keep their
/// `if !status.is_success()` handling). Only transport/decode failures are
/// `Err`, with the same message shape as the old helper.
pub async fn result(
    http: &Client,
    data_api: &str,
    headers: &HeaderMap,
    dataset: &str,
    request: &Value,
) -> Result<QueryOutcome, (StatusCode, String)> {
    if let Some(body) = crate::bench_mock::result(dataset) {
        let json_len = dl_api::json_len(&body);
        let data_export = body.get("data_export").cloned();
        return Ok(QueryOutcome { status: StatusCode::OK, body, json_len, data_export });
    }
    let Some(state) = typed_state() else {
        return result_http(http, data_api, headers, dataset, request).await;
    };
    let request = match decode_request::<ResultRequest>(request) {
        Ok(request) => request,
        Err(failure) => {
            return Ok(QueryOutcome {
                status: failure.status_code(),
                body: failure.body(),
                json_len: 0,
                data_export: None,
            })
        }
    };
    let path = format!("/api/data/v2/datasets/{dataset}/result");
    let direct = Arc::clone(&state);
    let response = traced("POST", &path, async move {
        with_scope(&state, headers, async move {
            // The handler takes the dataset out of the request and pins the
            // stored one to `revision_id`; an inline dataset wins.
            let mut request = request;
            let dataset = match request.dataset.take() {
                Some(dataset) => Arc::new(dataset),
                None => {
                    direct
                        .dataset_rev(dataset, request.revision_id.as_deref())
                        .await?
                }
            };
            direct.service.result(&dataset, &request).await
        })
        .await
    })
    .await;
    match response {
        Ok(response) => {
            let json_len = response.json_len;
            let body = serde_json::to_value(&response)
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            // `result_http` falls back to the response's `data_export` member when the route
            // sets no header, and the graph path reads this field. Keep both transports
            // content-identical.
            let data_export = body.get("data_export").cloned();
            Ok(QueryOutcome {
                status: StatusCode::OK,
                body,
                json_len,
                data_export,
            })
        }
        Err(failure) => Ok(QueryOutcome {
            status: failure.status_code(),
            body: failure.body(),
            json_len: 0,
            data_export: None,
        }),
    }
}

/// One typed `/result` outcome for chart builders that do not need a `Value` body.
pub struct TypedOutcome {
    /// `200 OK` — a rejected request is returned as [`TypedResult::Failed`].
    pub status: StatusCode,
    /// The typed `/result` body (no `serde_json::Value` hop).
    pub body: DataBody,
    /// Compact-JSON length of `body` (the value the envelope's `source.size` reports),
    /// computed once into a bounded per-worker scratch instead of walking a `Value`.
    pub json_len: u64,
    /// The response's `data_export` member as JSON (small, needed by the envelope).
    pub data_export: Value,
}

/// Result of [`result_typed`]: a typed success body, or the same `{code,message}` error body
/// the HTTP transport would return.
pub enum TypedResult {
    Ok(TypedOutcome),
    Failed { status: StatusCode, body: Value },
}

/// `POST /result` without the typed→`Value` conversion, for the v8.1 chart path.
///
/// Falls back to an error when the in-process typed data-api state is not installed (the
/// mock/standalone transports keep using [`result`] and its JSON body).
pub async fn result_typed(
    _http: &Client,
    _data_api: &str,
    headers: &HeaderMap,
    dataset: &str,
    request: &Value,
) -> Result<TypedResult, (StatusCode, String)> {
    let Some(state) = typed_state() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "typed data-api state is not installed".to_string(),
        ));
    };
    let request = match decode_request::<ResultRequest>(request) {
        Ok(request) => request,
        Err(failure) => {
            return Ok(TypedResult::Failed {
                status: failure.status_code(),
                body: failure.body(),
            })
        }
    };
    let path = format!("/api/data/v2/datasets/{dataset}/result");
    let direct = Arc::clone(&state);
    let response = traced("POST", &path, async move {
        with_scope(&state, headers, async move {
            let mut request = request;
            let dataset = match request.dataset.take() {
                Some(dataset) => Arc::new(dataset),
                None => {
                    direct
                        .dataset_rev(dataset, request.revision_id.as_deref())
                        .await?
                }
            };
            direct.service.result(&dataset, &request).await
        })
        .await
    })
    .await;
    match response {
        Ok(response) => {
            // The data-api measured the body's compact-JSON length when it finalized the
            // response (`ResultResponse::json_len`), so no serialization happens here.
            let json_len = response.json_len;
            let data_export = serde_json::to_value(&response.data_export)
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            let body = DataBody::Result(ResultBody::new(response));
            Ok(TypedResult::Ok(TypedOutcome {
                status: StatusCode::OK,
                body,
                json_len,
                data_export,
            }))
        }
        Err(failure) => Ok(TypedResult::Failed {
            status: failure.status_code(),
            body: failure.body(),
        }),
    }
}

/// `POST /api/data/v2/datasets/{ds_id}/values/distinct` on the typed in-process path.
///
/// Same shape as [`result_typed`]: the data-api returns the typed `ResultResponse` and the
/// compact-JSON length it measured, so a control source can report `sources[].size` without
/// serializing the body again. The HTTP/msgpack fallback is [`distinct`].
pub async fn distinct_typed(
    _http: &Client,
    _data_api: &str,
    headers: &HeaderMap,
    dataset: &str,
    request: &Value,
    revision: Option<&str>,
) -> Result<TypedResult, (StatusCode, String)> {
    let Some(state) = typed_state() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "typed data-api state is not installed".to_string(),
        ));
    };
    let mut request = match decode_request::<DistinctRequest>(request) {
        Ok(request) => request,
        Err(failure) => {
            return Ok(TypedResult::Failed {
                status: failure.status_code(),
                body: failure.body(),
            })
        }
    };
    // The revision pins the data-api query-cache key; it is passed out of band so the
    // `sources.distincts.data` request body echoed to the client stays byte-identical to
    // what the reference client sends.
    if request.revision_id.is_none() {
        request.revision_id = revision.map(str::to_string);
    }
    let path = format!("/api/data/v2/datasets/{dataset}/values/distinct");
    let direct = Arc::clone(&state);
    let response = traced("POST", &path, async move {
        with_scope(&state, headers, async move {
            let revision = request.revision_id.clone();
            let dataset = direct.dataset_rev(dataset, revision.as_deref()).await?;
            direct.service.distinct(&dataset, &request).await
        })
        .await
    })
    .await;
    match response {
        Ok(response) => {
            // The data-api measured the body's compact-JSON length when it finalized it.
            let json_len = response.json_len;
            let data_export = serde_json::to_value(&response.data_export)
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            let body = DataBody::Result(ResultBody::new(response));
            Ok(TypedResult::Ok(TypedOutcome {
                status: StatusCode::OK,
                body,
                json_len,
                data_export,
            }))
        }
        Err(failure) => Ok(TypedResult::Failed {
            status: failure.status_code(),
            body: failure.body(),
        }),
    }
}

/// `POST /api/data/v2/datasets/{ds_id}/values/distinct` over the HTTP/msgpack transport.
pub async fn distinct(
    http: &Client,
    data_api: &str,
    headers: &HeaderMap,
    dataset: &str,
    request: &Value,
) -> Result<QueryOutcome, (StatusCode, String)> {
    if let Some(body) = crate::bench_mock::distinct(dataset) {
        let json_len = dl_api::json_len(&body);
        let data_export = body.get("data_export").cloned();
        return Ok(QueryOutcome { status: StatusCode::OK, body, json_len, data_export });
    }
    let response = crate::msgpack::request(
        http.post(format!(
            "{data_api}/api/data/v2/datasets/{dataset}/values/distinct"
        ))
        .headers(headers.clone())
        .json(request),
    )
    .send()
    .await
    .map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            format!("data-api distinct: {error}"),
        )
    })?;
    let status = response.status();
    let body: Value = crate::msgpack::decode(response).await?;
    let json_len = dl_api::json_len(&body);
    let data_export = body.get("data_export").cloned();
    Ok(QueryOutcome {
        status,
        body,
        json_len,
        data_export,
    })
}

async fn result_http(
    http: &Client,
    data_api: &str,
    headers: &HeaderMap,
    dataset: &str,
    request: &Value,
) -> Result<QueryOutcome, (StatusCode, String)> {
    let response = crate::msgpack::request(
        http.post(format!(
            "{data_api}/api/data/v2/datasets/{dataset}/result"
        ))
        .headers(headers.clone())
        .json(request),
    )
    .send()
    .await
    .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status = response.status();
    let mut data_export = response
        .headers()
        .get("x-dl-data-export")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| serde_json::from_str::<Value>(value).ok());
    let body: Value = crate::msgpack::decode(response).await?;
    if data_export.is_none() {
        data_export = body.get("data_export").cloned();
    }
    let json_len = dl_api::json_len(&body);
    Ok(QueryOutcome {
        status,
        body,
        json_len,
        data_export,
    })
}

// ---------------------------------------------------------------------------
// `/pivot`
// ---------------------------------------------------------------------------

/// `POST /api/data/v2/datasets/{ds_id}/pivot`.
///
/// Note the handler resolves the stored dataset *without* pinning the request
/// revision (`/pivot` has always done that); the direct path keeps it.
pub async fn pivot(
    http: &Client,
    data_api: &str,
    headers: &HeaderMap,
    dataset: &str,
    request: &Value,
) -> Result<QueryOutcome, (StatusCode, String)> {
    if let Some(body) = crate::bench_mock::pivot(dataset) {
        let json_len = dl_api::json_len(&body);
        let data_export = body.get("data_export").cloned();
        return Ok(QueryOutcome { status: StatusCode::OK, body, json_len, data_export });
    }
    let Some(state) = typed_state() else {
        return pivot_http(http, data_api, headers, dataset, request).await;
    };
    let request = match decode_request::<PivotRequest>(request) {
        Ok(request) => request,
        Err(failure) => {
            return Ok(QueryOutcome {
                status: failure.status_code(),
                body: failure.body(),
                json_len: 0,
                data_export: None,
            })
        }
    };
    let path = format!("/api/data/v2/datasets/{dataset}/pivot");
    let direct = Arc::clone(&state);
    let response = traced("POST", &path, async move {
        with_scope(&state, headers, async move {
            let mut request = request;
            let dataset = match request.dataset.take() {
                Some(dataset) => Arc::new(dataset),
                None => direct.dataset(dataset).await?,
            };
            direct.service.pivot(&dataset, &request).await
        })
        .await
    })
    .await;
    match response {
        Ok(response) => {
            let json_len = response.json_len;
            let body = serde_json::to_value(&response)
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            let data_export = body.get("data_export").cloned();
            Ok(QueryOutcome {
                status: StatusCode::OK,
                body,
                json_len,
                data_export,
            })
        }
        Err(failure) => Ok(QueryOutcome {
            status: failure.status_code(),
            body: failure.body(),
            json_len: 0,
            data_export: None,
        }),
    }
}

async fn pivot_http(
    http: &Client,
    data_api: &str,
    headers: &HeaderMap,
    dataset: &str,
    request: &Value,
) -> Result<QueryOutcome, (StatusCode, String)> {
    let response = crate::msgpack::request(
        http.post(format!(
            "{data_api}/api/data/v2/datasets/{dataset}/pivot"
        ))
        .headers(headers.clone())
        .json(request),
    )
    .send()
    .await
    .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status = response.status();
    let mut data_export = response
        .headers()
        .get("x-dl-data-export")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| serde_json::from_str::<Value>(value).ok());
    let body: Value = crate::msgpack::decode(response).await?;
    if data_export.is_none() {
        data_export = body.get("data_export").cloned();
    }
    let json_len = dl_api::json_len(&body);
    Ok(QueryOutcome {
        status,
        body,
        json_len,
        data_export,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dataset() -> Dataset {
        serde_json::from_value(json!({
            "fields": [
                {
                    "guid": "f1",
                    "title": "FirstOrder_Month",
                    "data_type": "date",
                    "cast": "date",
                    "hidden": false,
                    "initial_data_type": "date",
                    "calc_mode": "formula",
                    "formula": "DATETRUNC([FirstOrderDate],'month')",
                    "aggregation": "none",
                    "type": "DIMENSION"
                },
                {
                    "guid": "m1",
                    "title": "COUNTS",
                    "data_type": "integer",
                    "cast": "integer",
                    "hidden": false,
                    "initial_data_type": "integer",
                    "calc_mode": "direct",
                    "aggregation": "sum",
                    "type": "MEASURE"
                }
            ]
        }))
        .expect("dataset")
    }

    #[test]
    fn fields_payload_has_the_route_schema() {
        let payload = fields_payload(&dataset(), Some("rev-1"));
        assert_eq!(payload["revision_id"], json!("rev-1"));
        let fields = payload["fields"].as_array().expect("fields");
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0]["guid"], json!("f1"));
        assert_eq!(fields[0]["data_type"], json!("date"));
        assert_eq!(fields[0]["type"], json!("DIMENSION"));
        assert_eq!(fields[1]["type"], json!("MEASURE"));
        assert_eq!(fields[1]["aggregation"], json!("sum"));
        // The key set the engine reads (`fetch_fields`, table/pivot request builders).
        for key in [
            "title",
            "guid",
            "data_type",
            "cast",
            "hidden",
            "type",
            "calc_mode",
            "ui_settings",
            "description",
            "formula",
            "aggregation",
            "default_value",
        ] {
            assert!(fields[0].get(key).is_some(), "missing `{key}`");
        }
    }

    #[test]
    fn a_rejected_request_gets_the_api_json_error_text() {
        let failure = decode_request::<ResultRequest>(&json!({"fields": [{"ref": {"type": "id"}}]}))
            .expect_err("missing id must be rejected");
        assert_eq!(failure.status, 400);
        assert_eq!(failure.code, "ERR.DS_API.REQUEST");
        assert!(
            failure.message.starts_with(
                "Failed to deserialize the JSON body into the target type"
            ),
            "unexpected message: {}",
            failure.message
        );
    }

    #[test]
    fn a_valid_request_decodes_without_the_http_round_trip() {
        let request = decode_request::<ResultRequest>(&json!({
            "fields": [{"ref": {"type": "id", "id": "m1"}, "role_spec": {"role": "measure"}}],
            "filters": [],
            "order_by": [],
            "parameter_values": [],
            "ignore_nonexistent_filters": true,
            "autofill_legend": true
        }))
        .expect("request");
        assert_eq!(request.fields.len(), 1);
        assert!(request.autofill_legend);
    }

    #[test]
    fn api_failure_body_matches_the_http_error_envelope() {
        let failure = ApiFailure::from_api(&ApiError::not_found("dataset nope does not exist"));
        assert_eq!(failure.status, 404);
        assert_eq!(
            failure.body(),
            json!({"code": "ERR.DS_API.DB.DATASET_NOT_FOUND", "message": "dataset nope does not exist"})
        );
    }

    #[test]
    fn bearer_tokens_are_read_like_the_auth_middleware() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer_token(&headers), None);
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "bearer token-1".parse().expect("header"),
        );
        assert_eq!(bearer_token(&headers), Some("token-1"));
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Basic abc".parse().expect("header"),
        );
        assert_eq!(bearer_token(&headers), None);
    }

    #[tokio::test]
    async fn the_snapshot_header_stays_scoped_to_one_call() {
        let mut headers = HeaderMap::new();
        headers.insert("x-dl-snapshot", "snap-1".parse().expect("header"));
        assert_eq!(dl_us::current_snapshot(), None);
        let seen = with_snapshot(&headers, async { dl_us::current_snapshot() }).await;
        assert_eq!(seen.as_deref(), Some("snap-1"));
        assert_eq!(dl_us::current_snapshot(), None);
    }
}
