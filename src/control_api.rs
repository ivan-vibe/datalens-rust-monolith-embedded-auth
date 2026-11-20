//! Minimal native control-api: the BI routes the DataLens UI needs from `control-api`.
//!
//! The Python service owns connector plugins, dataset loaders and US managers. Here the
//! handlers reuse the monolith's own in-process US/data-api listeners over loopback HTTP
//! (`/v1/entries`, `/private/entries`, `/api/data/v1/datasets/{id}/fields`), which keeps the
//! wire contract and needs no new transport, cache or dependency. All payload shaping is in
//! [`crate::control_schemas`] / [`crate::connections`] so it stays unit-testable.
//!
//! Wiring (Lead scope): `mod control_api;` in `main.rs` and, in `proxy.rs::forward`, route
//! `/api/v1/` (and `/api/v1/info/...`) to [`router`] before the data-api fallback. The router
//! is stateless and answers on the same public port the US/data-api routes use, so
//! `PUBLIC_PORT` is the only configuration it reads.
//!
//! Write routes (`datasets` create/update/delete/copy, `connections` create/update/delete)
//! go through the in-process US private write routes; they keep only the storage fields the
//! Python loaders own (no connector-side recomputation, no per-connector validation).
//!
//! Unsupported here (deferred, not stubbed silently):
//! * widget/dash/workbook write routes;
//! * real introspection for connectors other than PostgreSQL (`test_connection`,
//!   `/info/sources`, `/info/source/schema`); those answer Python's 4xx database-error
//!   envelope instead of a made-up result (see [`crate::connections`]);
//! * `edit` connector forms for connectors other than Postgres/ClickHouse fall back to the
//!   create form.
//! The UI reports those calls as 404/405 from the public router, exactly like a route that
//! the native monolith does not serve.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};

use crate::connections;
use crate::control_schemas as schemas;

/// The in-process US/data-api listener. `UI_PUBLIC_URL` wins when the monolith is fronted by
/// a different origin; otherwise the monolith's own `PUBLIC_PORT` (8099 in the native cluster).
fn public_base() -> String {
    if let Ok(url) = std::env::var("UI_PUBLIC_URL") {
        return url;
    }
    let port = std::env::var("PUBLIC_PORT").unwrap_or_else(|_| "8099".to_string());
    format!("http://127.0.0.1:{port}")
}

fn master_token() -> String {
    std::env::var("US_MASTER_TOKEN").unwrap_or_default()
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Headers forwarded to US so its ACL runs as the caller (the same set the old ui-api port
/// forwarded).
fn user_headers(extra: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    // The component decides whether US returns `unversionedData` (the encrypted connection
    // password) on a private read; `backend` owns connection/dataset/config scopes.
    out.insert("x-dl-component", reqwest::header::HeaderValue::from_static("backend"));
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&master_token()) {
        out.insert("x-us-master-token", value);
    }
    for name in [
        "authorization",
        "cookie",
        "x-dl-tenantid",
        "x-dl-workbookid",
        "x-dl-sudo",
        "x-dl-allow-superuser",
        "accept-language",
        "x-request-id",
    ] {
        if let Some(value) = extra.get(name).and_then(|value| value.to_str().ok()) {
            if let (Ok(value), Ok(name)) = (
                reqwest::header::HeaderValue::from_str(value),
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            ) {
                out.insert(name, value);
            }
        }
    }
    out
}

/// Headers for a US call made *as the user*: unlike `user_headers` this omits the master
/// token, so the US public routes apply their own auth + permission checks instead of treating
/// the request as a trusted service call.
fn public_user_headers(extra: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    out.insert("x-dl-component", reqwest::header::HeaderValue::from_static("backend"));
    for name in [
        "authorization",
        "cookie",
        "x-dl-tenantid",
        "x-dl-workbookid",
        "x-dl-sudo",
        "x-dl-allow-superuser",
        "accept-language",
        "x-request-id",
    ] {
        if let Some(value) = extra.get(name).and_then(|value| value.to_str().ok()) {
            if let (Ok(value), Ok(name)) = (
                reqwest::header::HeaderValue::from_str(value),
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            ) {
                out.insert(name, value);
            }
        }
    }
    out
}

fn service_headers() -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    out.insert("x-dl-component", reqwest::header::HeaderValue::from_static("backend"));
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&master_token()) {
        out.insert("x-us-master-token", value);
    }
    out
}

fn json_response(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

fn message_response(status: StatusCode, message: impl Into<String>) -> Response {
    json_response(status, json!({"message": message.into()}))
}

/// The control-api error envelope (`API.errorhandler(USReqException)` shows the US body, while
/// `USObjectNotFoundException`/validation are the `{"code","message","details","debug"}` shape).
fn us_error(status: StatusCode, code: &str, message: &str, id: &str, scope: &str) -> Response {
    if status == StatusCode::NOT_FOUND {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({
                "code": schemas::CODE_OBJ_NOT_FOUND,
                "message": "Object not found",
                "details": {"entry_id": id, "scope": scope},
                "debug": {},
            }),
        );
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return message_response(status, code_message(code, message));
    }
    if status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "code": schemas::CODE_VALIDATION_FAILED,
                "message": "Invalid input data: validation failed",
                "details": {"entry_id": id, "scope": scope},
                "debug": {},
            }),
        );
    }
    message_response(StatusCode::INTERNAL_SERVER_ERROR, code_message(code, message))
}

fn code_message(code: &str, message: &str) -> String {
    if message.is_empty() {
        code.to_string()
    } else {
        message.to_string()
    }
}

/// Who the US request runs as: the caller (ACL + permissions on the payload) or the service
/// master token (`SKIP_AUTH` export routes).
enum UsAs {
    User,
    Service,
}

async fn us_entry(
    id: &str,
    scope: &str,
    headers: &HeaderMap,
    as_: UsAs,
    rev_id: Option<&str>,
) -> Result<Value, Response> {
    let (path, extra) = match as_ {
        UsAs::User => (
            format!(
                "{}/v1/entries/{id}?includePermissionsInfo=1&includeLinks=1&includeFavorite=1",
                public_base()
            ),
            user_headers(headers),
        ),
        UsAs::Service => (
            format!(
                "{}/private/entries/{id}?includePermissionsInfo=1&includeFavorite=1",
                public_base()
            ),
            service_headers(),
        ),
    };
    let mut url = path;
    if let Some(rev_id) = rev_id {
        url.push_str("&revId=");
        url.push_str(rev_id);
    }
    let response = client()
        .get(&url)
        .headers(extra)
        .send()
        .await
        .map_err(|error| message_response(StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value.get("message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or(body);
        return Err(us_error(status, "", &detail, id, scope));
    }
    serde_json::from_str::<Value>(&body)
        .map_err(|error| message_response(StatusCode::BAD_GATEWAY, error.to_string()))
}

async fn data_api_fields(id: &str, headers: &HeaderMap) -> Result<Value, Response> {
    let url = format!("{}/api/data/v1/datasets/{id}/fields", public_base());
    let response = client()
        .get(&url)
        .headers(user_headers(headers))
        .send()
        .await
        .map_err(|error| message_response(StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status == StatusCode::NOT_FOUND {
        return Err(json_response(
            StatusCode::NOT_FOUND,
            json!({
                "code": schemas::CODE_OBJ_NOT_FOUND,
                "message": "Object not found",
                "details": {"entry_id": id, "scope": "dataset"},
                "debug": {},
            }),
        ));
    }
    if !status.is_success() {
        return Err(message_response(StatusCode::BAD_GATEWAY, body));
    }
    serde_json::from_str::<Value>(&body)
        .map_err(|error| message_response(StatusCode::BAD_GATEWAY, error.to_string()))
}

/// Apply the per-patch answer to a mapped field: a successful preflight carries the recomputed
/// type; a failed one keeps the `DEFAULT_DATA_TYPE` fallback but preserves the UI's requested
/// `type` for translation errors (the reference never recomputes it then).
fn apply_field_answer(
    mapped: &mut Value,
    answer: &Option<Value>,
    error: &Option<String>,
    patch: &Value,
) {
    match (answer, error) {
        (Some(answer), _) => {
            apply_preflight_field(mapped, answer);
            if let Some(object) = mapped.as_object_mut() {
                object.entry("valid").or_insert(json!(true));
            }
        }
        (None, Some(code)) => {
            mark_failed_field(mapped);
            if code.contains("UNKNOWN_FUNCTION") {
                if let Some(field_type) = patch.get("type").filter(|value| !value.is_null()) {
                    if let Some(object) = mapped.as_object_mut() {
                        object.insert("type".to_string(), field_type.clone());
                    }
                }
            }
        }
        _ => {}
    }
}

/// Copy the validated `data_type` / `type` / `valid` of a preflight field answer into the
/// control-api field object.
fn apply_preflight_field(field: &mut Value, answer: &Value) {
    let Some(field) = field.as_object_mut() else {
        return;
    };
    if let Some(data_type) = answer.get("data_type").filter(|value| !value.is_null()) {
        field.insert("data_type".to_string(), data_type.clone());
        field.insert("initial_data_type".to_string(), data_type.clone());
        field.insert("cast".to_string(), data_type.clone());
    }
    if let Some(field_type) = answer.get("type").filter(|value| !value.is_null()) {
        field.insert("type".to_string(), field_type.clone());
    }
    if let Some(valid) = answer.get("valid") {
        field.insert("valid".to_string(), valid.clone());
    }
}

/// The `DEFAULT_DATA_TYPE` fallback of a field the validator could not compile: Python keeps
/// the field in the response with `data_type`/`initial_data_type`/`cast` = string and
/// `valid: false`, and the structural `type` falls back to `DIMENSION`.
fn mark_failed_field(field: &mut Value) {
    let Some(field) = field.as_object_mut() else {
        return;
    };
    for key in ["data_type", "initial_data_type", "cast"] {
        field.insert(key.to_string(), json!("string"));
    }
    field.insert("type".to_string(), json!("DIMENSION"));
    field.insert("valid".to_string(), json!(false));
}

/// One `component_errors.items` entry for a field the data-api rejected.
fn field_error_item(guid: &str, preflight: &Value) -> Value {
    json!({
        "id": guid,
        "type": "field",
        "errors": [{
            "level": "error",
            "message": preflight.get("message").cloned().unwrap_or(Value::Null),
            "code": preflight.get("code").cloned().unwrap_or(json!("ERR.DS_API.VALIDATION.ERROR")),
            "details": {},
        }],
    })
}

fn json_slice(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// `POST /api/data/v2/datasets/{id}/result-preflight`: the data-api applies the editor's
/// `updates`, infers the type of a freshly added formula field and validates the result. This
/// is what gives the dataset editor's validation response its `data_type`s (Python's
/// control-api runs its own `DatasetValidator` for the same answer).
async fn data_api_preflight(
    id: &str,
    body: &Value,
    headers: &HeaderMap,
) -> Result<(StatusCode, Value), Response> {
    let url = format!("{}/api/data/v2/datasets/{id}/result-preflight", public_base());
    let response = client()
        .post(&url)
        .headers(user_headers(headers))
        .json(body)
        .send()
        .await
        .map_err(|error| message_response(StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| message_response(StatusCode::BAD_GATEWAY, error.to_string()))?;
    if !status.is_success() && !status.is_client_error() {
        return Err(message_response(StatusCode::BAD_GATEWAY, text));
    }
    Ok((status, value))
}

/// The `add_field` / `update_field` action of an editor request for `guid`, flattened to a
/// dataset field. `update_field` is what the UI sends when an existing formula is edited; its
/// patch is partial, so callers have to overlay it on the stored field.
fn update_add_field(updates: &Value, guid: &str) -> Option<Value> {
    json_slice(Some(updates))
        .iter()
        .find(|update| {
            matches!(
                update.get("action").and_then(Value::as_str),
                Some("add_field" | "update_field" | "add" | "update")
            ) && update.pointer("/field/guid").and_then(Value::as_str) == Some(guid)
        })
        .and_then(|update| update.get("field"))
        .and_then(Value::as_object)
        .map(|field| Value::Object(field.clone()))
}

/// Overlay an `update_field` patch (formula, title, aggregation, cast, ...) on the stored field
/// the response starts from. Request-only keys (`datasetId`, `local`) are not part of a dataset
/// field, and `guid` is the lookup key.
fn overlay_field_patch(field: &mut Value, patch: &Value) {
    let (Some(field), Some(patch)) = (field.as_object_mut(), patch.as_object()) else {
        return;
    };
    for (key, value) in patch {
        if matches!(key.as_str(), "guid" | "datasetId" | "local") {
            continue;
        }
        field.insert(key.clone(), value.clone());
    }
}

/// `component_errors` of a preflight answer: `{items: [{id, type, errors: [...]}]}`.
fn preflight_has_error(component_errors: &Value, guid: &str) -> bool {
    json_slice(component_errors.get("items"))
        .iter()
        .any(|item| item.get("id").and_then(Value::as_str) == Some(guid))
}

/// `connection_id -> db_type` for the sources of one dataset (the option builder and the
/// formula validator need the connector type).
async fn connection_types(ids: &[String]) -> HashMap<String, String> {
    let mut types = HashMap::with_capacity(ids.len());
    for id in ids {
        if let Ok(entry) = us_entry(id, "connection", &HeaderMap::new(), UsAs::Service, None).await {
            if let Some(db_type) = entry.get("type").and_then(Value::as_str) {
                types.insert(id.clone(), db_type.to_string());
            }
        }
    }
    types
}

/// `POST` a create body to the in-process US and return the new encoded entry id.
async fn create_us_entry(
    headers: &HeaderMap,
    body: Value,
    workbook_id: Option<&str>,
) -> Result<String, Response> {
    let url = format!("{}/v1/entries", public_base());
    let mut request = client()
        .post(&url)
        .headers(public_user_headers(headers))
        .json(&body);
    if let Some(workbook_id) = workbook_id {
        request = request.header("x-dl-workbookid", workbook_id);
    }
    let response = request
        .send()
        .await
        .map_err(|error| message_response(StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // US validates the create payload itself; a rejected payload is the caller's 400,
        // not a native-service failure.
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            let parsed: Value =
                serde_json::from_str(&body).unwrap_or_else(|_| json!({"message": body}));
            return Err(json_response(
                status,
                json!({
                    "code": parsed.get("code").cloned().unwrap_or_else(|| json!("ERR.DS_API.US.AUTH")),
                    "message": parsed.get("message").cloned().unwrap_or_else(|| json!("Auth denied")),
                }),
            ));
        }
        if status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY {
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| value.get("message").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| body.clone());
            return Err(message_response(StatusCode::BAD_REQUEST, message));
        }
        return Err(message_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            if body.is_empty() { status.to_string() } else { body },
        ));
    }
    let value: Value = serde_json::from_str(&body)
        .map_err(|error| message_response(StatusCode::BAD_GATEWAY, error.to_string()))?;
    value
        .get("entryId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| message_response(StatusCode::BAD_GATEWAY, format!("create entry: {value}")))
}

/// The caller header Python's auth middleware requires. The public UI always sends one; export
/// routes are `SKIP_AUTH` and do not call this.
fn require_user(headers: &HeaderMap) -> Result<(), Response> {
    crate::data_direct::require_authenticated_user(headers).map_err(|(status, message)| {
        message_response(status, message)
    })
}

// ---------------------------------------------------------------------------------------------
// datasets
// ---------------------------------------------------------------------------------------------

async fn dataset_body(
    id: &str,
    headers: &HeaderMap,
    rev_id: Option<&str>,
) -> Result<Value, Response> {
    require_user(headers)?;
    let entry = us_entry(id, "dataset", headers, UsAs::User, rev_id).await?;
    if entry.get("scope").and_then(Value::as_str) != Some("dataset") {
        return Err(us_error(StatusCode::NOT_FOUND, "", "", id, "dataset"));
    }
    let conn_types = connection_types(&schemas::source_connection_ids(&entry)).await;
    let content = schemas::dataset_content(&entry);
    let options = schemas::dataset_options(&content, &conn_types);
    Ok(schemas::dataset_response(&entry, options))
}

async fn dataset_get(Path(dataset_id): Path<String>, headers: HeaderMap) -> Response {
    // The Python control-api has no `GET /datasets/{id}` (only the versioned route); the BI
    // client reads datasets through `versions/draft`, so this route is the same body with the
    // default (current) revision, kept for callers that address the entry directly.
    match dataset_body(&dataset_id, &headers, None).await {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(response) => response,
    }
}

async fn dataset_version(
    Path((dataset_id, _version)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let rev_id = query.get("rev_id").map(String::as_str);
    match dataset_body(&dataset_id, &headers, rev_id).await {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(response) => response,
    }
}

async fn dataset_fields(Path(dataset_id): Path<String>, headers: HeaderMap) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    match data_api_fields(&dataset_id, &headers).await {
        Ok(body) => json_response(StatusCode::OK, schemas::fields_payload(&body)),
        Err(response) => response,
    }
}

async fn dataset_export(
    Path(dataset_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // `DatasetExportItem.REQUIRED_RESOURCES` = SKIP_AUTH + US_HEADERS_TOKEN: service manager.
    let entry = match us_entry(&dataset_id, "dataset", &headers, UsAs::Service, None).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let id_mapping = body.get("id_mapping").and_then(Value::as_object).cloned().unwrap_or_default();
    json_response(
        StatusCode::OK,
        schemas::dataset_export_payload(&entry, &id_mapping),
    )
}

async fn dataset_import(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let workbook_id = body
        .pointer("/data/workbook_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    // The mapped connection ids are the real ones; their connector types fill the stored
    // `native_type.conn_type` of the imported source columns.
    let mapped_ids: Vec<String> = body
        .get("id_mapping")
        .and_then(Value::as_object)
        .map(|mapping| {
            mapping
                .values()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let conn_types = connection_types(&mapped_ids).await;
    let create = match schemas::dataset_import_body(&body, &conn_types) {
        Ok(create) => create,
        Err(message) => {
            return json_response(
                StatusCode::OK,
                json!({"notifications": [{
                    "message": message,
                    "level": "critical",
                    "code": schemas::DS_IMPORT_CODE,
                }]}),
            );
        }
    };
    match create_us_entry(&headers, create, workbook_id.as_deref()).await {
        Ok(id) => json_response(StatusCode::OK, json!({"id": id, "notifications": []})),
        Err(response) => response,
    }
}

// ---------------------------------------------------------------------------------------------
// connections
// ---------------------------------------------------------------------------------------------

async fn connection_get(
    Path(connection_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let rev_id = query.get("rev_id").map(String::as_str);
    let entry = match us_entry(&connection_id, "connection", &headers, UsAs::User, rev_id).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    if entry.get("scope").and_then(Value::as_str) != Some("connection") {
        return us_error(StatusCode::NOT_FOUND, "", "", &connection_id, "connection");
    }
    json_response(StatusCode::OK, connections::connection_get_payload(&entry))
}

async fn connection_export(Path(connection_id): Path<String>, headers: HeaderMap) -> Response {
    let entry = match us_entry(&connection_id, "connection", &headers, UsAs::Service, None).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    json_response(StatusCode::OK, connections::connection_export_payload(&entry))
}

async fn connection_import(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let workbook_id = body
        .pointer("/data/workbook_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let create = match connections::connection_import_body(&body) {
        Ok(create) => create,
        Err(message) => {
            return json_response(
                StatusCode::OK,
                json!({"notifications": [{
                    "message": message,
                    "level": "critical",
                    "code": connections::CONNECTION_IMPORT_CODE,
                }]}),
            );
        }
    };
    match create_us_entry(&headers, create, workbook_id.as_deref()).await {
        Ok(id) => json_response(StatusCode::OK, json!({"id": id, "notifications": []})),
        Err(response) => response,
    }
}

// ---------------------------------------------------------------------------------------------
// write CRUD (US private write routes)
// ---------------------------------------------------------------------------------------------

const FEATURE_NOT_AVAILABLE_CODE: &str = "ERR.DS_API.FEATURE_NOT_AVAILABLE";
const REVISION_MISMATCH_CODE: &str = "ERR.DS_API.DATASET_REVISION_MISMATCH";

/// Python wraps a marshmallow load error as `{"message": "{'field': ['...']}"}`.
fn schema_error_response(field: &str, message: &str) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        json!({"message": format!("{{'{field}': ['{message}']}}")}),
    )
}

fn not_found_response(body: Value) -> Response {
    json_response(StatusCode::NOT_FOUND, body)
}

/// `POST /private/entries/{id}`: the US update route (`mode: publish` saves and publishes).
async fn us_update_entry(
    headers: &HeaderMap,
    entry_id: &str,
    body: Value,
) -> Result<(), Response> {
    let url = format!("{}/v1/entries/{}", public_base(), entry_id);
    let response = client()
        .post(&url)
        .headers(public_user_headers(headers))
        .json(&body)
        .send()
        .await
        .map_err(|error| message_response(StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if status.is_success() {
        return Ok(());
    }
    Err(us_write_error(status, &text, entry_id, "dataset", false))
}

/// `DELETE /private/entries/{id}`.
async fn us_delete_entry(
    headers: &HeaderMap,
    entry_id: &str,
    scope: &str,
) -> Result<(), Response> {
    let url = format!("{}/v1/entries/{}", public_base(), entry_id);
    let response = client()
        .delete(&url)
        .headers(public_user_headers(headers))
        .send()
        .await
        .map_err(|error| message_response(StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if status.is_success() {
        return Ok(());
    }
    Err(us_write_error(status, &text, entry_id, scope, true))
}

fn us_write_error(status: StatusCode, body: &str, entry_id: &str, scope: &str, details: bool) -> Response {
    if status == StatusCode::NOT_FOUND {
        let mut payload = json!({
            "code": schemas::CODE_OBJ_NOT_FOUND,
            "message": "Object not found",
            "debug": {},
        });
        payload["details"] = if details {
            json!({"entry_id": entry_id, "scope": scope})
        } else {
            json!({})
        };
        return not_found_response(payload);
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        let parsed: Value = serde_json::from_str(body).unwrap_or_else(|_| json!({"message": body}));
        return json_response(
            status,
            json!({
                "code": parsed.get("code").cloned().unwrap_or_else(|| json!("ERR.DS_API.US.AUTH")),
                "message": parsed.get("message").cloned().unwrap_or_else(|| json!("Auth denied")),
            }),
        );
    }
    if status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY {
        let message = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|value| value.get("message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| body.to_string());
        return message_response(StatusCode::BAD_REQUEST, message);
    }
    message_response(StatusCode::INTERNAL_SERVER_ERROR, body)
}

fn dataset_name(body: &Value) -> String {
    body.get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Dataset")
        .to_string()
}

/// A path-located entry (`POST /datasets` without a workbook/collection) is stored under
/// `dir_path` (`datasets` by default), like `resolve_entry_loc_from_api_req_body`.
fn location_key(body: &Value, default_dir: &str) -> String {
    let dir_path = body
        .get("dir_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .unwrap_or(default_dir);
    format!("{dir_path}/{}", dataset_name(body))
}

/// `POST /api/v1/datasets`.
async fn dataset_create(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let Some(dataset) = body.get("dataset").and_then(Value::as_object) else {
        return schema_error_response("dataset", "Missing data for required field.");
    };
    let name = dataset_name(&body);
    let description = dataset.get("description").and_then(Value::as_str).unwrap_or("");
    let source_ids: Vec<String> = dataset
        .get("sources")
        .and_then(Value::as_array)
        .map(|sources| {
            sources
                .iter()
                .filter_map(|source| source.get("connection_id"))
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let conn_types = connection_types(&source_ids).await;
    let stored = match schemas::stored_dataset_data(dataset, None, &conn_types) {
        Ok(stored) => stored,
        Err(message) => return message_response(StatusCode::BAD_REQUEST, message),
    };
    let workbook_id = body.get("workbook_id").and_then(Value::as_str);
    let mut create = Map::with_capacity(10);
    if let Some(workbook_id) = body.get("workbook_id").filter(|value| !value.is_null()) {
        create.insert("workbookId".to_string(), workbook_id.clone());
    } else if let Some(collection_id) = body.get("collection_id").filter(|value| !value.is_null()) {
        create.insert("collectionId".to_string(), collection_id.clone());
    } else {
        create.insert("key".to_string(), json!(location_key(&body, "datasets")));
    }
    create.insert("name".to_string(), json!(name));
    create.insert("scope".to_string(), json!("dataset"));
    create.insert("type".to_string(), json!(""));
    create.insert("mode".to_string(), json!("publish"));
    create.insert("links".to_string(), json!({}));
    create.insert("data".to_string(), stored.clone());
    create.insert("annotation".to_string(), json!({"description": description}));
    match create_us_entry(&headers, Value::Object(create), workbook_id).await {
        Ok(id) => json_response(
            StatusCode::OK,
            schemas::dataset_write_response(Some(&id), &stored, description, &conn_types),
        ),
        Err(response) => response,
    }
}

async fn dataset_update_impl(dataset_id: &str, headers: &HeaderMap, body: Value) -> Response {
    if let Err(response) = require_user(headers) {
        return response;
    }
    let Some(dataset) = body.get("dataset").and_then(Value::as_object) else {
        return schema_error_response("dataset", "Missing data for required field.");
    };
    let entry = match us_entry(dataset_id, "dataset", headers, UsAs::Service, None).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let current = entry.get("data").cloned().unwrap_or_else(|| json!({}));
    let current_revision = current.get("revision_id").and_then(Value::as_str).unwrap_or("");
    if let Some(client_revision) = dataset
        .get("revision_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        if client_revision != current_revision {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({
                    "code": REVISION_MISMATCH_CODE,
                    "message": "Dataset version mismatch. Refresh the page to continue.",
                    "details": {},
                    "debug": {},
                }),
            );
        }
    }
    let source_ids: Vec<String> = dataset
        .get("sources")
        .and_then(Value::as_array)
        .map(|sources| {
            sources
                .iter()
                .filter_map(|source| source.get("connection_id"))
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let conn_types = connection_types(&source_ids).await;
    let incoming = match schemas::stored_dataset_data(dataset, None, &conn_types) {
        Ok(incoming) => incoming,
        Err(message) => return message_response(StatusCode::BAD_REQUEST, message),
    };
    let mut stored = schemas::merge_stored_dataset(&current, incoming);
    stored["revision_id"] = json!(schemas::fresh_revision_id());
    let description = dataset
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            entry
                .pointer("/annotation/description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        });
    let update = json!({
        "data": stored,
        "mode": "publish",
        "annotation": {"description": description},
    });
    if let Err(response) = us_update_entry(&headers, dataset_id, update).await {
        return response;
    }
    json_response(
        StatusCode::OK,
        schemas::dataset_write_response(None, &stored, &description, &conn_types),
    )
}

async fn dataset_update(
    Path((dataset_id, _version)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    dataset_update_impl(&dataset_id, &headers, body).await
}

async fn dataset_update_alias(
    Path(dataset_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    dataset_update_impl(&dataset_id, &headers, body).await
}

/// `DELETE /api/v1/datasets/{id}`.
async fn dataset_delete(Path(dataset_id): Path<String>, headers: HeaderMap) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    // Read first: the delete route answers a bare 404 without the scope the UI expects.
    if let Err(response) = us_entry(&dataset_id, "dataset", &headers, UsAs::Service, None).await {
        return response;
    }
    match us_delete_entry(&headers, &dataset_id, "dataset").await {
        Ok(()) => json_response(StatusCode::OK, Value::Null),
        Err(response) => response,
    }
}

/// `POST /api/v1/datasets/{id}/copy`.
async fn dataset_copy(
    Path(dataset_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let Some(new_key) = body.get("new_key").and_then(Value::as_str).filter(|key| !key.is_empty()) else {
        return schema_error_response("new_key", "Missing data for required field.");
    };
    let entry = match us_entry(&dataset_id, "dataset", &headers, UsAs::Service, None).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    if entry.get("workbookId").filter(|value| !value.is_null()).is_some() {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "code": FEATURE_NOT_AVAILABLE_CODE,
                "message": "Dataset copy in workbooks is not supported yet",
                "details": {},
                "debug": {},
            }),
        );
    }
    let stored = entry.get("data").cloned().unwrap_or_else(|| json!({}));
    let name = new_key.rsplit('/').next().unwrap_or(new_key);
    let description = entry
        .pointer("/annotation/description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let source_ids = schemas::source_connection_ids(&entry);
    let conn_types = connection_types(&source_ids).await;
    let create = json!({
        "key": new_key,
        "name": name,
        "scope": "dataset",
        "type": "",
        "mode": "publish",
        "links": {},
        "data": stored,
        "annotation": {"description": description},
    });
    match create_us_entry(&headers, create, None).await {
        Ok(id) => json_response(
            StatusCode::OK,
            schemas::dataset_write_response(Some(&id), &stored, description, &conn_types),
        ),
        Err(response) => response,
    }
}

/// `POST /api/v1/connections`.
async fn connection_create(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let Some(conn_type) = body.get("type").and_then(Value::as_str) else {
        return bad_conn_type_create_response(None);
    };
    if connections::spec(conn_type).is_none() {
        return bad_conn_type_create_response(Some(conn_type));
    }
    let workbook_id = body.get("workbook_id").and_then(Value::as_str);
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(conn_type);
    let create = match connections::connection_create_body(&body) {
        Ok(mut create) => {
            if create.get("workbookId").is_none() && create.get("collectionId").is_none() {
                create["key"] = json!(format!("connections/{name}"));
            }
            create
        }
        Err(message) => return message_response(StatusCode::BAD_REQUEST, message),
    };
    match create_us_entry(&headers, create, workbook_id).await {
        Ok(id) => json_response(StatusCode::OK, json!({"id": id})),
        Err(response) => response,
    }
}

fn bad_conn_type_create_response(conn_type: Option<&str>) -> Response {
    let value = conn_type.unwrap_or("{}");
    json_response(
        StatusCode::BAD_REQUEST,
        json!({
            "code": connections::BAD_CONN_TYPE_CODE,
            "message": format!("Invalid connection type value: {value}"),
            "details": {},
            "debug": {},
        }),
    )
}

async fn connection_update_impl(connection_id: &str, headers: &HeaderMap, body: Value) -> Response {
    if let Err(response) = require_user(headers) {
        return response;
    }
    let entry = match us_entry(connection_id, "connection", headers, UsAs::Service, None).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let update = match connections::connection_update_body(&entry, &body) {
        Ok(update) => update,
        Err(message) => return message_response(StatusCode::BAD_REQUEST, message),
    };
    match us_update_entry(&headers, connection_id, update).await {
        Ok(()) => json_response(StatusCode::OK, Value::Null),
        Err(response) => response,
    }
}

async fn connection_update(
    Path(connection_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    connection_update_impl(&connection_id, &headers, body).await
}

/// `DELETE /api/v1/connections/{id}`.
async fn connection_delete(Path(connection_id): Path<String>, headers: HeaderMap) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    if let Err(response) = us_entry(&connection_id, "connection", &headers, UsAs::Service, None).await {
        return response;
    }
    match us_delete_entry(&headers, &connection_id, "connection").await {
        Ok(()) => json_response(StatusCode::OK, Value::Null),
        Err(response) => response,
    }
}

// ---------------------------------------------------------------------------------------------
// connection editor: test / introspection
// ---------------------------------------------------------------------------------------------

/// Python's `DatabaseUnavailable` error body: a 400 with the driver message in details/debug.
fn db_error_response(message: &str) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        json!({
            "code": connections::DB_SOURCE_CONNECT_ERROR,
            "message": "Data source refused connection.",
            "details": {"db_message": message},
            "debug": {"db_message": message},
        }),
    )
}

/// Python maps a database *connect/query* failure (bad password, SQL error) to
/// `ERR.DS_API.DB`; an unreachable data source is `SOURCE_CONNECT_ERROR` instead. The Rust
/// executor's error text does not say which one it was, so Postgres failures use this envelope.
fn database_error_response(message: &str) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        json!({
            "code": "ERR.DS_API.DB",
            "message": "Database error.",
            "details": {"db_message": message},
            "debug": {"db_message": message},
        }),
    )
}

/// Resolve/connect failures shared by the two test-connection handlers.
fn connection_test_error(message: String) -> Response {
    if message.starts_with("Missing data") || message.starts_with("Unexpected request") {
        return message_response(StatusCode::BAD_REQUEST, message);
    }
    if message == connections::UNSUPPORTED_DB_MESSAGE {
        return db_error_response(&message);
    }
    database_error_response(&message)
}

fn unsupported_db_response(message: &str) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        json!({
            "code": connections::UNSUPPORTED_ENTITY_CODE,
            "message": message,
            "details": {},
            "debug": {},
        }),
    )
}

fn bad_conn_type_response(conn_type: &str) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        json!({
            "code": connections::BAD_CONN_TYPE_CODE,
            "message": format!("Not a valid connection type for this environment: {conn_type}"),
            "details": {},
            "debug": {},
        }),
    )
}

/// A US `connection` entry **with** `unversionedData`: the service route is the one that
/// returns secrets, and the editor endpoints need the password to reach the database.
async fn connection_entry_with_secrets(connection_id: &str) -> Result<Value, Response> {
    let entry = us_entry(
        connection_id,
        "connection",
        &HeaderMap::new(),
        UsAs::Service,
        None,
    )
    .await?;
    if entry.get("scope").and_then(Value::as_str) != Some("connection") {
        return Err(us_error(StatusCode::NOT_FOUND, "", "", connection_id, "connection"));
    }
    Ok(entry)
}

/// Apply the edit-form changes to a stored connection entry (the body of
/// `test_connection/{id}`). A masked password keeps the stored one.
fn merged_connection_entry(entry: &Value, changes: &Value) -> Value {
    let mut merged = entry.clone();
    let Some(changes) = changes.as_object() else {
        return merged;
    };
    if changes.is_empty() {
        return merged;
    }
    if let Some(data) = merged.get_mut("data").and_then(Value::as_object_mut) {
        for (key, value) in changes {
            if matches!(key.as_str(), "type" | "name" | "description" | "password") {
                continue;
            }
            data.insert(key.clone(), value.clone());
        }
    }
    if let Some(password) = changes.get("password").and_then(Value::as_str) {
        if password != "******" {
            merged["unversionedData"] = json!({"password": password});
        }
    }
    merged
}

async fn test_connection(
    Path(connection_id): Path<String>,
    headers: HeaderMap,
    Json(changes): Json<Value>,
) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let entry = match connection_entry_with_secrets(&connection_id).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let connection = match connections::resolved_connection(&merged_connection_entry(&entry, &changes)) {
        Ok(connection) => connection,
        Err(message) => return connection_test_error(message),
    };
    match connections::test_connection(&connection).await {
        Ok(()) => json_response(StatusCode::OK, Value::Null),
        Err(message) => connection_test_error(message),
    }
}

async fn test_connection_params(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let connection = match connections::resolved_connection_params(&body) {
        Ok(connection) => connection,
        Err(message) => return connection_test_error(message),
    };
    match connections::test_connection(&connection).await {
        Ok(()) => json_response(StatusCode::OK, Value::Null),
        Err(message) => connection_test_error(message),
    }
}

async fn connection_db_names(Path(connection_id): Path<String>, headers: HeaderMap) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    if let Err(response) = connection_entry_with_secrets(&connection_id).await {
        return response;
    }
    unsupported_db_response("DB names listing is not supported for current connection type")
}

async fn connection_metadata_sources(
    Path(connection_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let entry = match connection_entry_with_secrets(&connection_id).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let conn_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
    json_response(
        StatusCode::OK,
        connections::metadata_sources_payload(conn_type, &connection_id),
    )
}

async fn connection_sources(
    Path(connection_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let entry = match connection_entry_with_secrets(&connection_id).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let conn_type = entry.get("type").and_then(Value::as_str).unwrap_or("").to_string();
    let connection = match connections::resolved_connection(&entry) {
        Ok(connection) => connection,
        Err(message) => return db_error_response(&message),
    };
    let limit = query.get("limit").and_then(|value| value.parse::<usize>().ok());
    let offset = query.get("offset").and_then(|value| value.parse::<usize>().ok());
    let search_text = query.get("search_text").map(String::as_str);
    match connections::list_sources(&connection, &connection_id, search_text, limit, offset).await {
        Ok(sources) => json_response(
            StatusCode::OK,
            connections::info_sources_payload(&conn_type, &connection_id, sources),
        ),
        Err(message) => db_error_response(&message),
    }
}

async fn connection_source_listing_options(
    Path(connection_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let entry = match connection_entry_with_secrets(&connection_id).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let conn_type = entry.get("type").and_then(Value::as_str);
    json_response(
        StatusCode::OK,
        json!({"source_listing": connections::listing_options(conn_type)}),
    )
}

async fn connection_source_schema(
    Path(connection_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let entry = match connection_entry_with_secrets(&connection_id).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let connection = match connections::resolved_connection(&entry) {
        Ok(connection) => connection,
        Err(message) => return db_error_response(&message),
    };
    let Some(source) = body.get("source").and_then(Value::as_object) else {
        return message_response(StatusCode::BAD_REQUEST, "Missing data for required field: source");
    };
    let source_type = source.get("source_type").and_then(Value::as_str).unwrap_or("");
    let parameters = source.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
    match connections::source_schema(&connection, source_type, &parameters).await {
        Ok(raw_schema) => json_response(StatusCode::OK, json!({"raw_schema": raw_schema})),
        Err(message) if message.starts_with("Missing data") => {
            message_response(StatusCode::BAD_REQUEST, message)
        }
        Err(message) => unsupported_db_response(&message),
    }
}

// ---------------------------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/info/field_types`: the static BI type/aggregation catalog.
///
/// The reference computes it from `BI_TYPE_AGGREGATIONS`; the set is fixed for the
/// opensource installation, so keep the exact golden payload instead of porting the enum
/// table (it is 925 bytes and only the dataset editor reads it).
async fn field_types() -> Json<Value> {
    Json(json!({
        "types": [
            {"name": "string", "aggregations": ["none", "count", "countunique"]},
            {"name": "integer", "aggregations": ["none", "count", "sum", "avg", "min", "max", "countunique"]},
            {"name": "float", "aggregations": ["none", "count", "sum", "avg", "min", "max", "countunique"]},
            {"name": "date", "aggregations": ["none", "count", "min", "max", "countunique", "avg"]},
            {"name": "genericdatetime", "aggregations": ["none", "count", "min", "max", "countunique"]},
            {"name": "boolean", "aggregations": ["none", "count"]},
            {"name": "geopoint", "aggregations": ["none", "count"]},
            {"name": "geopolygon", "aggregations": ["none", "count"]},
            {"name": "array_float", "aggregations": ["none", "count", "countunique"]},
            {"name": "array_int", "aggregations": ["none", "count", "countunique"]},
            {"name": "array_str", "aggregations": ["none", "count", "countunique"]},
            {"name": "tree_str", "aggregations": ["none", "count", "countunique"]}
        ]
    }))
}

async fn connectors() -> Json<Value> {
    Json(connections::connectors_payload())
}

async fn connector_icons() -> Json<Value> {
    Json(connections::icons_payload())
}

async fn connector_icon(Path(conn_type): Path<String>) -> Json<Value> {
    Json(connections::icon_payload(&conn_type))
}

async fn connector_form(Path((conn_type, form_mode)): Path<(String, String)>) -> Response {
    match connections::connector_form_payload(&conn_type, &form_mode) {
        Ok(payload) => json_response(StatusCode::OK, payload),
        Err(connections::FormLookupError::BadConnType(conn_type)) => {
            bad_conn_type_response(&conn_type)
        }
        Err(connections::FormLookupError::UnknownMode(mode)) => {
            unsupported_db_response(&format!("Unknown form mode: {mode}"))
        }
    }
}

/// `datasets_publicity_checker` / `connections_publicity_checker`: the reference deployment
/// is not a public environment, so every existing entity is allowed with no reason.
fn publicity_result(
    ids: &[Value],
    results: Vec<Value>,
) -> Value {
    let _ = ids;
    json!({"result": results})
}

async fn datasets_publicity_checker(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let mut results = Vec::new();
    for id in body.get("datasets").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = id.as_str() else { continue };
        if us_entry(id, "dataset", &headers, UsAs::User, None).await.is_ok() {
            results.push(json!({"dataset_id": id, "allowed": true, "reason": Value::Null}));
        }
    }
    json_response(StatusCode::OK, publicity_result(&[], results))
}

async fn connections_publicity_checker(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let mut results = Vec::new();
    for id in body.get("connections").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = id.as_str() else { continue };
        if us_entry(id, "connection", &headers, UsAs::User, None).await.is_ok() {
            results.push(json!({"connection_id": id, "allowed": true, "reason": Value::Null}));
        }
    }
    json_response(StatusCode::OK, publicity_result(&[], results))
}

// ---------------------------------------------------------------------------------------------
// validators
// ---------------------------------------------------------------------------------------------

fn validation_response(status: u16, payload: Value) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    json_response(status, payload)
}

fn field_of(body: &Value) -> (&str, &str, &str) {
    let field = body.get("field");
    (
        field
            .and_then(|field| field.get("title"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        field
            .and_then(|field| field.get("guid"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        field
            .and_then(|field| field.get("formula"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    )
}

async fn field_validate_root(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let (title, guid, formula) = field_of(&body);
    // An inline dataset in the body is the only source information the root route can see.
    let has_sources = body
        .get("dataset")
        .map(schemas::has_content_sources)
        .unwrap_or(false);
    let issue = schemas::formula_issue(formula, body.get("dataset"), None);
    let (status, payload) = schemas::field_validation_payload(title, guid, issue.as_ref(), has_sources);
    validation_response(status, payload)
}

async fn field_validate_version(
    Path((dataset_id, _version)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let entry = match us_entry(&dataset_id, "dataset", &headers, UsAs::User, None).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let conn_types = connection_types(&schemas::source_connection_ids(&entry)).await;
    let conn_type = schemas::connection_type_of(&entry, &conn_types);
    let content = schemas::dataset_content(&entry);
    let (title, guid, formula) = field_of(&body);
    let issue = schemas::formula_issue(formula, Some(&content), conn_type.as_deref());
    let (status, payload) =
        schemas::field_validation_payload(title, guid, issue.as_ref(), schemas::has_sources(&entry));
    validation_response(status, payload)
}

fn requested_content<'a>(body: &'a Value, stored: &'a Value) -> &'a Value {
    body.get("dataset").filter(|value| value.is_object()).unwrap_or(stored)
}

async fn dataset_validate_root(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let reported = schemas::dataset_content(&Value::Null);
    let content = requested_content(&body, &reported);
    let conn_ids: Vec<String> = content
        .get("sources")
        .and_then(Value::as_array)
        .map(|sources| {
            sources
                .iter()
                .filter_map(|source| source.get("connection_id"))
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let conn_types = connection_types(&conn_ids).await;
    let conn_type = conn_ids.first().and_then(|id| conn_types.get(id)).map(String::as_str);
    let issues = schemas::content_formula_issues(content, conn_type);
    let options = schemas::dataset_options(content, &conn_types);
    let (status, payload) = schemas::dataset_validation_payload(content, options, &issues);
    validation_response(status, payload)
}

async fn dataset_validate_version(
    Path((dataset_id, _version)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = require_user(&headers) {
        return response;
    }
    let entry = match us_entry(&dataset_id, "dataset", &headers, UsAs::User, None).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let stored = schemas::dataset_content(&entry);
    let mut content = requested_content(&body, &stored).clone();
    let updates = body.get("updates").cloned().unwrap_or_else(|| json!([]));
    // The data-api is the validator that can compile formulas: forward the same updates (plus
    // the inline dataset the editor sends on an unsaved change) and use its field list, which
    // carries the inferred `data_type`s, as the authoritative `result_schema`.
    let mut preflight_body = json!({"updates": updates});
    if let Some(dataset) = body.get("dataset").filter(|value| value.is_object()) {
        preflight_body["dataset"] = dataset.clone();
    }
    let (status, preflight) = match data_api_preflight(&dataset_id, &preflight_body, &headers).await {
        Ok(answer) => answer,
        Err(response) => return response,
    };
    let component_errors = preflight
        .pointer("/dataset/component_errors")
        .cloned()
        .unwrap_or_else(|| json!({"items": []}));
    let existing_schema = json_slice(content.get("result_schema")).to_vec();
    // The data-api rejected the request before validation could start (a formula error, a broken
    // source): Python's `DatasetValidator.apply_batch` still answers 400 with the
    // `ERR.DS_API.VALIDATION.ERROR` envelope, the patched dataset (the broken field kept with the
    // `string` fallback type, `valid: false`) and the concrete error in `component_errors`.
    let failed_early = !status.is_success();
    let mut schema: Vec<Value> = Vec::new();
    let preflight_fields = preflight.pointer("/dataset/fields").and_then(Value::as_array);
    if failed_early || preflight_fields.is_none() {
        // The whole request was rejected before the batch validation ran. Python's validator
        // applies the updates field by field, so one broken formula does not invalidate the
        // rest: validate each patch on its own (this runs only on the error path) and use the
        // per-field answers for the response.
        let mut errors: Vec<Value> = Vec::new();
        let mut answers: Vec<(&str, Option<Value>, Option<String>)> = Vec::new();
        for update in json_slice(Some(&updates)) {
            let Some(guid) = update.pointer("/field/guid").and_then(Value::as_str) else {
                continue;
            };
            let mut single = json!({"updates": [update.clone()]});
            if let Some(dataset) = body.get("dataset").filter(|value| value.is_object()) {
                single["dataset"] = dataset.clone();
            }
            let (single_status, single_body) =
                match data_api_preflight(&dataset_id, &single, &headers).await {
                    Ok(answer) => answer,
                    Err(response) => return response,
                };
            if single_status.is_success() {
                let field = single_body
                    .pointer("/dataset/fields")
                    .and_then(Value::as_array)
                    .and_then(|fields| {
                        fields.iter().find(|field| {
                            field.get("guid").and_then(Value::as_str) == Some(guid)
                        })
                    })
                    .cloned();
                answers.push((guid, field, None));
            } else {
                let code = single_body
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                answers.push((guid, None, Some(code)));
                errors.push(field_error_item(guid, &single_body));
            }
        }
        let mut schema: Vec<Value> = Vec::new();
        let mut patched_guids: Vec<&str> = Vec::new();
        for field in &existing_schema {
            let mut mapped = field.clone();
            let guid = field.get("guid").and_then(Value::as_str).unwrap_or_default();
            if let Some(patch) = update_add_field(&updates, guid) {
                overlay_field_patch(&mut mapped, &patch);
                patched_guids.push(guid);
                if let Some((_, answer, error)) = answers.iter().find(|(candidate, _, _)| *candidate == guid) { apply_field_answer(&mut mapped, answer, error, &patch) }
            }
            schema.push(mapped);
        }
        for (guid, answer, error) in &answers {
            if patched_guids.contains(guid) {
                continue;
            }
            let patch = update_add_field(&updates, guid);
            let mut mapped = patch
                .as_ref()
                .and_then(|patch| patch.as_object().cloned())
                .map(|patch| schemas::normalized_dataset_field(&patch))
                .unwrap_or_else(|| json!({"guid": guid}));
            apply_field_answer(&mut mapped, answer, error, patch.as_ref().unwrap_or(&Value::Null));
            schema.push(mapped);
        }
        // An inline dataset without updates (the dataset editor's unsaved state): mark the
        // first formula field the validator could not type, like `DEFAULT_DATA_TYPE`.
        if answers.is_empty() {
            for field in &mut schema {
                let is_untyped_formula = field.get("calc_mode").and_then(Value::as_str)
                    == Some("formula")
                    && field.get("data_type").map(Value::is_null).unwrap_or(true);
                if is_untyped_formula {
                    let guid = field
                        .get("guid")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    mark_failed_field(field);
                    errors.push(field_error_item(&guid, &preflight));
                    break;
                }
            }
        }
        content["result_schema"] = Value::Array(schema);
        content["component_errors"] = json!({"items": errors});
        let conn_ids = schemas::source_connection_ids(&entry);
        let conn_types = connection_types(&conn_ids).await;
        let options = schemas::dataset_options(&content, &conn_types);
        return validation_response(
            400,
            json!({
                "dataset": content,
                "options": options,
                "code": "ERR.DS_API.VALIDATION.ERROR",
                "message": "Validation finished with errors.",
                "dataset_errors": [],
            }),
        );
    }
    if let Some(fields) = preflight_fields {
        for field in fields {
            let Some(guid) = field.get("guid").and_then(Value::as_str) else {
                continue;
            };
            let patch = update_add_field(&updates, guid);
            let existing = existing_schema
                .iter()
                .find(|existing| existing.get("guid").and_then(Value::as_str) == Some(guid))
                .cloned();
            let mut mapped = match (&existing, &patch) {
                // An edited stored field: the stored properties plus the request's patch.
                (Some(existing), Some(patch)) => {
                    let mut mapped = existing.clone();
                    overlay_field_patch(&mut mapped, patch);
                    mapped
                }
                (None, Some(patch)) => patch
                    .as_object()
                    .cloned()
                    .map(|patch| schemas::normalized_dataset_field(&patch))
                    .unwrap_or_else(|| json!({"guid": guid})),
                (Some(existing), None) => existing.clone(),
                (None, None) => json!({"guid": guid}),
            };
            if let Some(object) = mapped.as_object_mut() {
                if let Some(data_type) = field.get("data_type").filter(|value| !value.is_null()) {
                    object.insert("data_type".to_string(), data_type.clone());
                    object.insert("initial_data_type".to_string(), data_type.clone());
                    object.insert("cast".to_string(), data_type.clone());
                }
                if let Some(field_type) = field.get("type").filter(|value| !value.is_null()) {
                    object.insert("type".to_string(), field_type.clone());
                }
                object.insert(
                    "valid".to_string(),
                    json!(!preflight_has_error(&component_errors, guid)),
                );
                for key in ["title", "calc_mode", "formula", "hidden", "aggregation"] {
                    if object.get(key).is_none() {
                        if let Some(value) = field.get(key).filter(|value| !value.is_null()) {
                            object.insert(key.to_string(), value.clone());
                        }
                    }
                }
            }
            schema.push(mapped);
        }
    } else {
        schema = existing_schema;
    }
    content["result_schema"] = Value::Array(schema);
    content["component_errors"] = component_errors;
    let conn_ids = schemas::source_connection_ids(&entry);
    let conn_types = connection_types(&conn_ids).await;
    let options = schemas::dataset_options(&content, &conn_types);
    let code = preflight
        .get("code")
        .cloned()
        .unwrap_or_else(|| json!("ERR.DS_API.VALIDATION.ERROR"));
    let message = preflight.get("message").cloned().unwrap_or(Value::Null);
    validation_response(
        status.as_u16(),
        json!({
            "dataset": content,
            "options": options,
            "code": code,
            "message": message,
            "dataset_errors": [],
        }),
    )
}

// ---------------------------------------------------------------------------------------------

/// The control-api subset, mounted by the Lead under the public router's `/api/v1/` branch.
pub fn router() -> Router {
    Router::new()
        .route("/api/v1/datasets", post(dataset_create))
        .route("/api/v1/datasets/export/{dataset_id}", post(dataset_export))
        .route("/api/v1/datasets/import", post(dataset_import))
        .route("/api/v1/datasets/validators/dataset", post(dataset_validate_root))
        .route("/api/v1/datasets/validators/field", post(field_validate_root))
        .route("/api/v1/datasets/{dataset_id}", get(dataset_get).delete(dataset_delete))
        .route("/api/v1/datasets/{dataset_id}/fields", get(dataset_fields))
        .route(
            "/api/v1/datasets/{dataset_id}/versions/{version}",
            get(dataset_version).put(dataset_update),
        )
        .route("/api/v1/datasets/{dataset_id}/update", post(dataset_update_alias))
        .route("/api/v1/datasets/{dataset_id}/copy", post(dataset_copy))
        .route(
            "/api/v1/datasets/{dataset_id}/versions/{version}/validators/schema",
            post(dataset_validate_version),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/versions/{version}/validators/field",
            post(field_validate_version),
        )
        .route("/api/v1/connections", post(connection_create))
        .route("/api/v1/connections/export/{connection_id}", get(connection_export))
        .route("/api/v1/connections/import", post(connection_import))
        .route("/api/v1/connections/test_connection_params", post(test_connection_params))
        .route("/api/v1/connections/test_connection/{connection_id}", post(test_connection))
        .route(
            "/api/v1/connections/{connection_id}",
            get(connection_get)
                .put(connection_update)
                .delete(connection_delete),
        )
        .route("/api/v1/connections/{connection_id}/update", post(connection_update))
        .route("/api/v1/connections/{connection_id}/db_names", get(connection_db_names))
        .route(
            "/api/v1/connections/{connection_id}/info/metadata_sources",
            get(connection_metadata_sources),
        )
        .route(
            "/api/v1/connections/{connection_id}/info/sources",
            get(connection_sources),
        )
        .route(
            "/api/v1/connections/{connection_id}/info/source_listing_options",
            get(connection_source_listing_options),
        )
        .route(
            "/api/v1/connections/{connection_id}/info/source/schema",
            post(connection_source_schema),
        )
        .route("/api/v1/info/field_types", get(field_types).post(field_types))
        .route("/api/v1/info/connectors", get(connectors))
        .route("/api/v1/info/connectors/icons", get(connector_icons))
        .route("/api/v1/info/connectors/icons/{conn_type}", get(connector_icon))
        .route(
            "/api/v1/info/connectors/forms/{conn_type}/{form_mode}",
            get(connector_form),
        )
        .route(
            "/api/v1/info/datasets_publicity_checker",
            post(datasets_publicity_checker),
        )
        .route(
            "/api/v1/info/connections_publicity_checker",
            post(connections_publicity_checker),
        )
}
