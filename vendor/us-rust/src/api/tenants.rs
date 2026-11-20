//! `src/controllers/tenants/*` and `src/services/new/tenants/*`.
//!
//! The tenant is resolved from the request context (`resolveTenantId` sets
//! `res.locals.tenantId = config.tenantIdOverride`, i.e. `common` in this
//! build); path/query parameters only matter for the private routes that look a
//! tenant up by an entry/collection/workbook/subscription id.
//!
//! `AUTH_ENABLED=false` makes `resolvePrivateRoute` a no-op, so the
//! `x-us-master-token` header is not required here.

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::id::decode_id;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    crate::api::dual_router(vec![
        ("/v1/tenants/details", get(tenant_details)),
        ("/v1/tenants/settings", post(update_tenant_settings)),
        (
            "/v1/tenants/set-default-color-palette",
            post(set_default_palette),
        ),
        (
            "/private/tenants/{tenant_id}/details",
            get(private_tenant_details),
        ),
        (
            "/private/resolveTenantByEntryId",
            get(resolve_tenant_by_entry_id),
        ),
        ("/private/resolve-tenant", get(resolve_tenant)),
    ])
}

/// `body-parser` json limit of the expresskit config.
const JSON_BODY_LIMIT: u64 = 10 * 1024 * 1024;

/// The columns of `src/db/models/new/tenant` that the response models emit.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TenantRow {
    pub tenant_id: String,
    pub meta: Option<Value>,
    pub created_at: Option<DateTime<Utc>>,
    pub enabled: bool,
    pub deleting: bool,
    pub last_init_at: DateTime<Utc>,
    pub retries_count: i32,
    pub folders_enabled: bool,
    pub collections_enabled: bool,
    pub branding: Option<Value>,
    pub settings: Option<Value>,
    pub features: Option<Value>,
}

const TENANT_COLUMNS: &str = "tenant_id, meta, created_at, enabled, deleting, last_init_at, \
     retries_count, folders_enabled, collections_enabled, branding, settings, features";

/// Node serializes `Date` as `toISOString()`: UTC, milliseconds, `Z` suffix.
fn format_timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn timestamp_json(value: Option<DateTime<Utc>>) -> Value {
    match value {
        Some(value) => Value::String(format_timestamp(value)),
        None => Value::Null,
    }
}

fn json_or_null(value: Option<Value>) -> Value {
    value.unwrap_or(Value::Null)
}

/// `tenant-model.ts` — exact field order of the zod object.
fn tenant_model(tenant: &TenantRow) -> Value {
    json!({
        "tenantId": tenant.tenant_id,
        "meta": json_or_null(tenant.meta.clone()),
        "createdAt": timestamp_json(tenant.created_at),
        "enabled": tenant.enabled,
        "deleting": tenant.deleting,
        "collectionsEnabled": tenant.collections_enabled,
        "foldersEnabled": tenant.folders_enabled,
        "settings": json_or_null(tenant.settings.clone()),
        "features": json_or_null(tenant.features.clone()),
        "branding": json_or_null(tenant.branding.clone()),
    })
}

/// `brief-tenant-with-settings-model.ts` — `settings` comes last.
fn brief_tenant_with_settings_model(tenant: &TenantRow) -> Value {
    json!({
        "tenantId": tenant.tenant_id,
        "meta": json_or_null(tenant.meta.clone()),
        "createdAt": timestamp_json(tenant.created_at),
        "enabled": tenant.enabled,
        "deleting": tenant.deleting,
        "lastInitAt": format_timestamp(tenant.last_init_at),
        "retriesCount": tenant.retries_count,
        "collectionsEnabled": tenant.collections_enabled,
        "foldersEnabled": tenant.folders_enabled,
        "settings": json_or_null(tenant.settings.clone()),
    })
}

async fn find_tenant(pool: &sqlx::PgPool, tenant_id: &str) -> Result<Option<TenantRow>, ApiError> {
    let sql = format!("SELECT {TENANT_COLUMNS} FROM tenants WHERE tenant_id = $1 LIMIT 1");

    Ok(sqlx::query_as::<_, TenantRow>(&sql)
        .bind(tenant_id)
        .fetch_optional(pool)
        .await?)
}

async fn get_tenant(state: &AppState, tenant_id: &str) -> Result<TenantRow, ApiError> {
    find_tenant(&state.db.replica, tenant_id)
        .await?
        .ok_or_else(ApiError::not_exist_tenant)
}

/// `GET /v1/tenants/details` — tenant from `ctx.get('info').tenantId`.
async fn tenant_details(State(state): State<AppState>) -> Result<Response, ApiError> {
    let tenant = get_tenant(&state, &state.config.tenant_id_override).await?;

    Ok(Json(tenant_model(&tenant)).into_response())
}

/// `GET /private/tenants/:tenantId/details`.
async fn private_tenant_details(
    State(state): State<AppState>,
    Path(tenant_id): Path<String>,
) -> Result<Response, ApiError> {
    let tenant = get_tenant(&state, &tenant_id).await?;

    Ok(Json(tenant_model(&tenant)).into_response())
}

/// Joins `tenants` with `entries`, as `get-tenant-by-entry-id.ts` does.
async fn tenant_by_entry_id(
    state: &AppState,
    entry_id: i64,
) -> Result<Option<TenantRow>, ApiError> {
    let sql = format!(
        "SELECT {} FROM tenants t \
         JOIN entries e ON e.tenant_id = t.tenant_id \
         WHERE e.entry_id = $1 LIMIT 1",
        aliased_tenant_columns("t")
    );

    Ok(sqlx::query_as::<_, TenantRow>(&sql)
        .bind(entry_id)
        .fetch_optional(&state.db.replica)
        .await?)
}

/// `GET /private/resolveTenantByEntryId?entryId=...`.
async fn resolve_tenant_by_entry_id(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, ApiError> {
    let query = parse_query(raw_query.as_deref().unwrap_or_default());
    let values = query.get("entryId").cloned().unwrap_or_default();

    // `entryId: zc.encodedId()` is a required query parameter.
    if values.is_empty() {
        return Err(ApiError::validation_issues(vec![missing_field_issue(
            "string", "entryId",
        )]));
    }

    let mut issues = Vec::new();
    let entry_id = encoded_query_value("entryId", &values, &mut issues).value;

    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues));
    }

    let entry_id = entry_id.expect("validated above");

    let tenant = tenant_by_entry_id(&state, entry_id)
        .await?
        .ok_or_else(ApiError::not_exist_entry)?;

    Ok(Json(tenant_model(&tenant)).into_response())
}

/// One optional `zc.encodedId()` query value.
///
/// `z.string().length(13).transform(decodeId)`:
/// * a single value of the wrong length yields `too_small`/`too_big`,
/// * a 13-character value that fails `decodeId` yields the `custom` issue,
/// * a repeated query parameter is an array (`qs` parses `a=1&a=2` as an
///   array) and yields `invalid_type` + the array length issue.
fn encoded_query_value(field: &str, values: &[String], issues: &mut Vec<Value>) -> EncodedValue {
    let expected = crate::id::ENCODED_ID_LENGTH;

    match values {
        [] => EncodedValue::default(),
        [single] => {
            // `z.string().length(13)` runs before the transform: a wrong
            // length never reaches `decodeId` (and does not abort
            // `superRefine`).
            if single.chars().count() != expected {
                issues.push(ApiError::encoded_id_issue(field, single));
                return EncodedValue::default();
            }

            match decode_id(single) {
                Ok(value) => EncodedValue {
                    value: Some(value),
                    decode_failed: false,
                },
                Err(_) => {
                    issues.push(ApiError::encoded_id_issue(field, single));
                    EncodedValue {
                        value: None,
                        // `makeIdDecoder` returns `z.NEVER`; zod then skips
                        // the object's `superRefine`.
                        decode_failed: true,
                    }
                }
            }
        }
        many => {
            issues.push(json!({
                "expected": "string",
                "code": "invalid_type",
                "path": [field],
                "message": "Invalid input: expected string, received array",
            }));

            let length = many.len();
            if length < expected {
                issues.push(json!({
                    "origin": "array",
                    "code": "too_small",
                    "minimum": expected,
                    "inclusive": true,
                    "exact": true,
                    "path": [field],
                    "message": format!(
                        "Too small: expected array to have >={expected} items"
                    ),
                }));
            } else if length > expected {
                issues.push(json!({
                    "origin": "array",
                    "code": "too_big",
                    "maximum": expected,
                    "inclusive": true,
                    "exact": true,
                    "path": [field],
                    "message": format!(
                        "Too big: expected array to have <={expected} items"
                    ),
                }));
            }

            EncodedValue::default()
        }
    }
}

#[derive(Debug, Default)]
struct EncodedValue {
    value: Option<i64>,
    decode_failed: bool,
}

/// `resolve-tenant` query schema: all four ids optional, exactly one required
/// (`superRefine`). The refinement runs on the raw values after the field
/// checks; a `z.NEVER` from `makeIdDecoder` aborts the object parse and skips
/// it, while plain length issues do not.
fn resolve_tenant_query_issues(query: &QueryMap) -> Vec<Value> {
    let mut issues = Vec::new();

    let fields = ["entryId", "collectionId", "workbookId", "subscriptionId"];
    let mut decode_failed = false;

    for field in fields {
        let values = query.get(field).cloned().unwrap_or_default();
        decode_failed |= encoded_query_value(field, &values, &mut issues).decode_failed;
    }

    // A `z.NEVER` from `makeIdDecoder` aborts the object parse, so the
    // `superRefine` below is never reached.
    if decode_failed {
        return issues;
    }

    let setted = fields
        .iter()
        .filter(|field| query.get(**field).map(is_truthy).unwrap_or(false))
        .count();

    if setted == 0 {
        issues.push(json!({
            "code": "custom",
            "path": [],
            "message": "Either 'entryId', 'collectionId', 'workbookId' or 'subscriptionId' must be set",
        }));
    } else if setted > 1 {
        issues.push(json!({
            "code": "custom",
            "path": [],
            "message": "Either 'entryId', 'collectionId', 'workbookId' or 'subscriptionId' must be set, but no more than one",
        }));
    }

    issues
}

fn is_truthy(values: &Vec<String>) -> bool {
    match values.as_slice() {
        [] => false,
        [single] => !single.is_empty(),
        _ => true,
    }
}

/// `GET /private/resolve-tenant`.
async fn resolve_tenant(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, ApiError> {
    let query = parse_query(raw_query.as_deref().unwrap_or_default());

    let issues = resolve_tenant_query_issues(&query);
    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues));
    }

    // `resolveTenant` tries entryId first, then collectionId, workbookId,
    // subscriptionId. Validation guarantees exactly one of them is set.
    let tenant = if let Some(entry_id) = decode_first(&query, "entryId") {
        tenant_by_entry_id(&state, entry_id).await?
    } else if let Some(collection_id) = decode_first(&query, "collectionId") {
        tenant_by_collection_id(&state, collection_id).await?
    } else if let Some(workbook_id) = decode_first(&query, "workbookId") {
        tenant_by_workbook_id(&state, workbook_id).await?
    } else if let Some(subscription_id) = decode_first(&query, "subscriptionId") {
        tenant_by_subscription_id(&state, subscription_id).await?
    } else {
        None
    };

    let tenant = tenant.ok_or_else(ApiError::not_exist_entry)?;

    Ok(Json(tenant_model(&tenant)).into_response())
}

fn decode_first(query: &QueryMap, field: &str) -> Option<i64> {
    let values = query.get(field)?;
    let raw = values.first()?;
    decode_id(raw).ok()
}

async fn tenant_by_collection_id(
    state: &AppState,
    collection_id: i64,
) -> Result<Option<TenantRow>, ApiError> {
    let sql = format!(
        "SELECT {} FROM tenants t \
         JOIN collections c ON c.tenant_id = t.tenant_id \
         WHERE c.collection_id = $1 LIMIT 1",
        aliased_tenant_columns("t")
    );

    Ok(sqlx::query_as::<_, TenantRow>(&sql)
        .bind(collection_id)
        .fetch_optional(&state.db.replica)
        .await?)
}

async fn tenant_by_workbook_id(
    state: &AppState,
    workbook_id: i64,
) -> Result<Option<TenantRow>, ApiError> {
    let sql = format!(
        "SELECT {} FROM tenants t \
         JOIN workbooks w ON w.tenant_id = t.tenant_id \
         WHERE w.workbook_id = $1 LIMIT 1",
        aliased_tenant_columns("t")
    );

    Ok(sqlx::query_as::<_, TenantRow>(&sql)
        .bind(workbook_id)
        .fetch_optional(&state.db.replica)
        .await?)
}

async fn tenant_by_subscription_id(
    state: &AppState,
    subscription_id: i64,
) -> Result<Option<TenantRow>, ApiError> {
    let sql = format!(
        "SELECT {} FROM tenants t \
         JOIN subscriptions s ON s.tenant_id = t.tenant_id \
         WHERE s.subscription_id = $1 LIMIT 1",
        aliased_tenant_columns("t")
    );

    Ok(sqlx::query_as::<_, TenantRow>(&sql)
        .bind(subscription_id)
        .fetch_optional(&state.db.replica)
        .await?)
}

fn aliased_tenant_columns(alias: &str) -> String {
    TENANT_COLUMNS
        .split(", ")
        .map(|column| format!("{alias}.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Query string parsing (`qs` semantics for the flat keys we need)
// ---------------------------------------------------------------------------

/// Raw query values: a key may be repeated (`a=1&a=2` ⇒ two values).
pub type QueryMap = std::collections::HashMap<String, Vec<String>>;

/// `qs`-style form decoding: `+` is a space, `%XX` is a byte.
pub(crate) fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = &raw[index + 1..index + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// Parses the query string into a map of raw (decoded) values.
pub(crate) fn parse_query(raw: &str) -> QueryMap {
    let mut map: QueryMap = std::collections::HashMap::new();

    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };

        map.entry(percent_decode(key))
            .or_default()
            .push(percent_decode(value));
    }

    map
}

// ---------------------------------------------------------------------------
// Request body parsing (express `body-parser` + expresskit contract)
// ---------------------------------------------------------------------------

fn invalid_json_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "Invalid JSON supplied",
    )
        .into_response()
}

fn body_too_large_response() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "Request entity too large",
    )
        .into_response()
}

/// Reads the request body exactly as express + `body-parser` would:
///
/// * a request without a body gets `{}` (body-parser 1.20 initializes
///   `req.body` to `{}`),
/// * a body whose `content-type` is not `application/json` is rejected by
///   `withContract` with a `ValidationError` that carries no zod issues
///   (hence `details: []`),
/// * invalid JSON — and a valid JSON primitive, because `body-parser` runs in
///   strict mode (objects/arrays only) — is answered with the expresskit
///   plain-text `Invalid JSON supplied`,
/// * a valid JSON object/array is returned as-is.
pub(crate) async fn read_json_body(headers: &HeaderMap, body: Bytes) -> Result<Value, Response> {
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok());

    if let Some(length) = content_length.and_then(|length| length.parse::<u64>().ok()) {
        if length > JSON_BODY_LIMIT {
            return Err(body_too_large_response());
        }
    }

    let has_body = headers.contains_key(header::TRANSFER_ENCODING)
        || matches!(content_length, Some(length) if length != "0");

    if has_body {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();

        if !content_type.contains("application/json") {
            return Err(ApiError::validation(json!([])).into_response());
        }
    }

    if body.is_empty() {
        return Ok(json!({}));
    }

    let value: Value = serde_json::from_slice(&body).map_err(|_| invalid_json_response())?;

    match value {
        Value::Object(_) | Value::Array(_) => Ok(value),
        _ => Err(invalid_json_response()),
    }
}

// ---------------------------------------------------------------------------
// zod issue helpers (zod v4 shapes, as observed on us v1.196.0)
// ---------------------------------------------------------------------------

/// The `received` name zod uses in `invalid_type` messages.
pub(crate) fn received_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `z.string()` / `z.boolean()` / … `invalid_type` issue.
pub(crate) fn invalid_type_issue(expected: &str, path: Vec<Value>, received: &str) -> Value {
    json!({
        "expected": expected,
        "code": "invalid_type",
        "path": path,
        "message": format!("Invalid input: expected {expected}, received {received}"),
    })
}

/// `invalid_type` issue for a missing object property.
pub(crate) fn missing_field_issue(expected: &str, field: &str) -> Value {
    invalid_type_issue(expected, vec![json!(field)], "undefined")
}

/// `zc.primitive()` = `z.union([z.boolean(), z.string(), z.number(), z.null()])`.
fn primitive_union_issue(received: &str, path: Vec<Value>) -> Value {
    json!({
        "code": "invalid_union",
        "errors": [
            [invalid_type_issue("boolean", vec![], received)],
            [invalid_type_issue("string", vec![], received)],
            [invalid_type_issue("number", vec![], received)],
            [invalid_type_issue("null", vec![], received)],
        ],
        "path": path,
        "message": "Invalid input",
    })
}

fn is_primitive(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

/// The `invalid_union` issue for `value` of `update-tenant-settings`, which is
/// `zc.primitive().or(z.record(z.string(), zc.primitive()))`.
fn tenant_setting_value_issue(value: Option<&Value>) -> Option<Value> {
    let value = match value {
        None => return Some(tenant_setting_value_union_issue("undefined", None)),
        Some(value) if is_primitive(value) => return None,
        Some(value) => value,
    };

    match value {
        Value::Array(_) => Some(tenant_setting_value_union_issue("array", Some(value))),
        Value::Object(map) => {
            let mut record_issues = Vec::new();
            for (key, item) in map {
                if !is_primitive(item) {
                    record_issues
                        .push(primitive_union_issue(received_type(item), vec![json!(key)]));
                }
            }

            if record_issues.is_empty() {
                None
            } else {
                Some(tenant_setting_value_union_issue("object", Some(value)))
            }
        }
        _ => None,
    }
}

/// `errors` of the outer union: `[[primitive union], [record option issues]]`.
fn tenant_setting_value_union_issue(received: &str, value: Option<&Value>) -> Value {
    let mut options: Vec<Value> = vec![Value::Array(vec![primitive_union_issue(received, vec![])])];

    match value {
        Some(Value::Object(map)) => {
            let mut record_issues = Vec::new();
            for (key, item) in map {
                if !is_primitive(item) {
                    record_issues
                        .push(primitive_union_issue(received_type(item), vec![json!(key)]));
                }
            }
            options.push(Value::Array(record_issues));
        }
        _ => {
            options.push(Value::Array(vec![json!({
                "expected": "record",
                "code": "invalid_type",
                "path": [],
                "message": format!("Invalid input: expected record, received {received}"),
            })]));
        }
    }

    json!({
        "code": "invalid_union",
        "errors": options,
        "path": ["value"],
        "message": "Invalid input",
    })
}

/// `requestSchema.body` of `update-tenant-settings.ts`.
fn update_tenant_settings_body_issues(body: &Value) -> Vec<Value> {
    let map = match body {
        Value::Object(map) => map,
        other => {
            return vec![invalid_type_issue("object", vec![], received_type(other))];
        }
    };

    let mut issues = Vec::new();

    match map.get("key") {
        Some(Value::String(_)) => {}
        Some(other) => issues.push(invalid_type_issue(
            "string",
            vec![json!("key")],
            received_type(other),
        )),
        None => issues.push(missing_field_issue("string", "key")),
    }

    if let Some(issue) = tenant_setting_value_issue(map.get("value")) {
        issues.push(issue);
    }

    issues
}

/// `processTenantSettings` (`src/registry/plugins/common/utils/tenant/utils.ts`):
/// `z.object({key: z.literal('defaultColorPaletteId'), value: z.string()})`.
fn process_tenant_settings_issues(key: &str, value: &Value) -> Vec<Value> {
    let mut issues = Vec::new();

    if key != "defaultColorPaletteId" {
        issues.push(json!({
            "code": "invalid_value",
            "values": ["defaultColorPaletteId"],
            "path": ["key"],
            "message": "Invalid input: expected \"defaultColorPaletteId\"",
        }));
    }

    if !value.is_string() {
        issues.push(invalid_type_issue(
            "string",
            vec![json!("value")],
            received_type(value),
        ));
    }

    issues
}

/// `set-default-color-palette.ts` body: `{defaultColorPaletteId: z.string()}`.
fn set_default_palette_body_issues(body: &Value) -> Vec<Value> {
    match body {
        Value::Object(map) => match map.get("defaultColorPaletteId") {
            Some(Value::String(_)) => Vec::new(),
            Some(other) => vec![invalid_type_issue(
                "string",
                vec![json!("defaultColorPaletteId")],
                received_type(other),
            )],
            None => vec![missing_field_issue("string", "defaultColorPaletteId")],
        },
        other => vec![invalid_type_issue("object", vec![], received_type(other))],
    }
}

/// `UPDATE tenants SET settings = jsonb_set(settings, '{key}', value)`, the SQL
/// both tenant-setting services issue.
async fn patch_tenant_setting(
    state: &AppState,
    key: &str,
    value: &Value,
) -> Result<Option<TenantRow>, ApiError> {
    let sql = format!(
        "UPDATE tenants \
         SET settings = jsonb_set(settings, $2::text[], $3::jsonb) \
         WHERE tenant_id = $1 \
         RETURNING {TENANT_COLUMNS}"
    );

    let serialized = serde_json::to_string(value).map_err(|error| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, None, error.to_string())
    })?;

    Ok(sqlx::query_as::<_, TenantRow>(&sql)
        .bind(&state.config.tenant_id_override)
        .bind(format!("{{{key}}}"))
        .bind(serialized)
        .fetch_optional(&state.db.primary)
        .await?)
}

/// `POST /v1/tenants/settings`.
async fn update_tenant_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let body = match read_json_body(&headers, body).await {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };

    let issues = update_tenant_settings_body_issues(&body);
    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues));
    }

    // Request validation passed, so `key`/`value` are present.
    let key = body["key"].as_str().expect("validated above");
    let value = body.get("value").cloned().expect("validated above");

    let issues = process_tenant_settings_issues(key, &value);
    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues));
    }

    let tenant = patch_tenant_setting(&state, key, &value)
        .await?
        .ok_or_else(ApiError::not_exist_tenant)?;

    Ok(Json(brief_tenant_with_settings_model(&tenant)).into_response())
}

/// `POST /v1/tenants/set-default-color-palette`.
async fn set_default_palette(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let body = match read_json_body(&headers, body).await {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };

    let issues = set_default_palette_body_issues(&body);
    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues));
    }

    let palette_id = body["defaultColorPaletteId"]
        .as_str()
        .expect("validated above");

    let tenant = patch_tenant_setting(
        &state,
        "defaultColorPaletteId",
        &Value::String(palette_id.to_owned()),
    )
    .await?
    .ok_or_else(ApiError::not_exist_tenant)?;

    Ok(Json(brief_tenant_with_settings_model(&tenant)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_query_strings_like_qs() {
        let query = parse_query("entryId=infciq1c89je7&x=a+b&x=c%20d");
        assert_eq!(query.get("entryId").unwrap(), &["infciq1c89je7"]);
        assert_eq!(query.get("x").unwrap(), &["a b", "c d"]);
        assert!(parse_query("").is_empty());
    }

    #[test]
    fn resolve_tenant_requires_exactly_one_id() {
        let issues = resolve_tenant_query_issues(&parse_query(""));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["code"], "custom");

        let issues = resolve_tenant_query_issues(&parse_query(
            "entryId=infciq1c89je7&workbookId=z4wtz6tg5194o",
        ));
        assert_eq!(issues.len(), 1);
        assert_eq!(
            issues[0]["message"],
            "Either 'entryId', 'collectionId', 'workbookId' or 'subscriptionId' must be set, but no more than one"
        );
    }

    #[test]
    fn resolve_tenant_appends_super_refine_issue_to_decode_issue() {
        let issues = resolve_tenant_query_issues(&parse_query("entryId="));
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0]["code"], "too_small");
        assert_eq!(issues[1]["code"], "custom");
    }

    #[test]
    fn decode_failure_skips_super_refine() {
        // Both ids fail `decodeId` (`z.NEVER`): only the two field issues.
        let issues = resolve_tenant_query_issues(&parse_query(
            "entryId=zzzzzzzzzzzzz&workbookId=zzzzzzzzzzzzz",
        ));
        assert_eq!(issues.len(), 2);
        assert!(issues.iter().all(|issue| issue["code"] == "custom"));
        assert!(issues.iter().all(|issue| issue["path"] != json!([])));

        // Two plain length issues do run the refinement.
        let issues = resolve_tenant_query_issues(&parse_query("entryId=abc&workbookId=def"));
        assert_eq!(issues.len(), 3);
        assert_eq!(issues[2]["code"], "custom");
        assert_eq!(issues[2]["path"], json!([]));
    }

    #[test]
    fn tenant_setting_union_issue_shape() {
        // `zc.primitive()` accepts scalars.
        assert!(tenant_setting_value_issue(Some(&json!("x"))).is_none());
        assert!(tenant_setting_value_issue(Some(&json!(1))).is_none());
        assert!(tenant_setting_value_issue(Some(&Value::Null)).is_none());
        assert!(tenant_setting_value_issue(Some(&json!({}))).is_none());
        assert!(tenant_setting_value_issue(Some(&json!({"a": 1, "b": "x"}))).is_none());

        // Objects are only valid when every member is primitive.
        let issue = tenant_setting_value_issue(Some(&json!({"a": {"b": 1}}))).unwrap();
        assert_eq!(issue["code"], "invalid_union");
        assert_eq!(issue["path"], json!(["value"]));
        assert_eq!(issue["errors"][0][0]["code"], "invalid_union");
        assert_eq!(issue["errors"][1][0]["path"], json!(["a"]));

        // Arrays fail both union branches.
        let issue = tenant_setting_value_issue(Some(&json!([1]))).unwrap();
        assert_eq!(issue["errors"][1][0]["expected"], "record");
        assert_eq!(
            issue["errors"][1][0]["message"],
            "Invalid input: expected record, received array"
        );
    }

    #[test]
    fn process_tenant_settings_validates_literal_key_and_string_value() {
        assert!(process_tenant_settings_issues("defaultColorPaletteId", &json!("p")).is_empty());

        let issues = process_tenant_settings_issues("foo", &json!(5));
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0]["code"], "invalid_value");
        assert_eq!(issues[1]["path"], json!(["value"]));
    }
}
