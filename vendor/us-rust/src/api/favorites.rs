//! `src/controllers/favorites/*` — list, add, delete, rename.
//!
//! Ports of `src/services/new/favorites/*` and the `FavoriteEntryPresentation`
//! model under `AUTH_ENABLED=false` / `DLS_ENABLED=false`
//! (`accessServiceEnabled === false`), where every permission check resolves to
//! "everything allowed":
//!
//! * `checkEntriesByPermission` → `makeEntryWithFullPermissions` for folder and
//!   workbook entries (`isLocked: false`, all permissions `true` when
//!   `includePermissionsInfo=true`);
//! * collection entries go through `createCollectionEntry`, whose open-source
//!   implementation throws `COLLECTION_ENTRY_UNSUPPORTED_SCOPE`;
//! * `addFavorite` still validates the entry and, for workbook entries, calls
//!   `getWorkbook` (a deleted/missing/foreign workbook yields
//!   `WORKBOOK_NOT_EXISTS` even with the access service off).
//!
//! `favorites.sort_alias` is a `bytea` column filled by the
//! `update_favorites` trigger (`naturalsort(alias)`); `pg` hands it to Node as
//! a `Buffer`, and `JSON.stringify` prints it as `{"type":"Buffer","data":[…]}`.

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::id::{decode_id, encode_id, encode_id_opt};
use crate::permissions;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    crate::api::dual_router(vec![
        ("/v1/favorites", get(list_favorites)),
        (
            "/v1/favorites/{entry_id}",
            post(add_favorite).delete(delete_favorite),
        ),
        ("/v1/favorites/{entry_id}/rename", post(rename_favorite)),
    ])
}

/// `EntryScope` (`src/db/models/new/entry/types.ts`) in declaration order.
const ENTRY_SCOPES: [&str; 10] = [
    "connection",
    "dataset",
    "widget",
    "dash",
    "folder",
    "config",
    "pdf",
    "report",
    "compute",
    "artifact",
];

/// `symbolsValidateMessage` (`src/components/validation-schema-compiler.ts`).
const SYMBOLS_VALIDATE_MESSAGE: &str = "should start and end with A-Za-zА-Яа-яЁё0-9_@()% and can contain only symbols: A-Za-zА-Яа-яЁё0-9_@()%.,:;'|-–—−$*& and spaces";

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

enum HandlerError {
    Api(ApiError),
    Raw(Response),
}

impl From<ApiError> for HandlerError {
    fn from(error: ApiError) -> Self {
        HandlerError::Api(error)
    }
}

impl From<sqlx::Error> for HandlerError {
    fn from(error: sqlx::Error) -> Self {
        HandlerError::Api(error.into())
    }
}

impl IntoResponse for HandlerError {
    fn into_response(self) -> Response {
        match self {
            HandlerError::Api(error) => error.into_response(),
            HandlerError::Raw(response) => response,
        }
    }
}

fn favorite_not_exists() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        Some("FAVORITE_NOT_EXISTS"),
        "The favorite doesn't exist",
    )
}

fn invalid_json() -> HandlerError {
    HandlerError::Raw(
        (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "Invalid JSON supplied",
        )
            .into_response(),
    )
}

// ---------------------------------------------------------------------------
// request parsing
// ---------------------------------------------------------------------------

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = &raw[index + 1..index + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        decoded.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        decoded.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn parse_query(raw: Option<&str>) -> Vec<(String, String)> {
    let mut pairs = Vec::new();

    let Some(raw) = raw else {
        return pairs;
    };

    for piece in raw.split('&') {
        if piece.is_empty() {
            continue;
        }

        let (key, value) = match piece.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(piece), String::new()),
        };

        pairs.push((key, value));
    }

    pairs
}

fn query_values<'a>(pairs: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    pairs
        .iter()
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
        .collect()
}

fn query_first<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query_values(pairs, key).into_iter().next()
}
/// `express.json({strict: true})` as wired by expresskit:
///   * a zero-length body leaves `req.body === {}` (the parser is skipped);
///   * a JSON body must be an object or an array ("Invalid JSON supplied");
///   * a non-empty body with any other content type is never parsed, so
///     `req.body === undefined` and zod rejects it with an *empty* issue list.
fn parse_body(headers: &HeaderMap, body: &[u8]) -> Result<Value, HandlerError> {
    if body.is_empty() {
        return Ok(Value::Object(Map::new()));
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let is_json = content_type.starts_with("application/json")
        || (content_type.starts_with("application/") && content_type.contains("+json"));

    if !is_json {
        return Err(ApiError::validation(json!([])).into());
    }

    let text = String::from_utf8_lossy(body).into_owned();
    let trimmed = text.trim_start();

    // body-parser's `strict` mode only accepts objects and arrays.
    match trimmed.as_bytes().first() {
        Some(b'{') | Some(b'[') => {}
        _ => return Err(invalid_json()),
    }

    serde_json::from_str::<Value>(&text).map_err(|_| invalid_json())
}

fn js_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn issue_invalid_type(path: &[&str], expected: &str, received: &str) -> Value {
    json!({
        "expected": expected,
        "code": "invalid_type",
        "path": path,
        "message": format!("Invalid input: expected {expected}, received {received}"),
    })
}

fn issue_invalid_option(path: &[&str], values: &[&str]) -> Value {
    let quoted: Vec<String> = values.iter().map(|value| format!("\"{value}\"")).collect();

    json!({
        "code": "invalid_value",
        "values": values,
        "path": path,
        "message": format!("Invalid option: expected one of {}", quoted.join("|")),
    })
}

/// `zc.stringBoolean()`: any string is accepted, only `1`/`true` (case
/// insensitive) are true.
fn is_true_arg(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "1" | "true")
}

/// `parseInt(value, 10)`.
fn js_parse_int(value: &str) -> Option<i64> {
    let trimmed = value.trim_start();
    let mut chars = trimmed.char_indices().peekable();

    let mut sign: i64 = 1;
    match chars.peek() {
        Some((_, '-')) => {
            sign = -1;
            chars.next();
        }
        Some((_, '+')) => {
            chars.next();
        }
        _ => {}
    }

    let mut digits = String::new();
    for (_, ch) in chars {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            break;
        }
    }

    if digits.is_empty() {
        return None;
    }

    digits.parse::<i64>().ok().map(|value| sign * value)
}

/// `zc.stringNumber({min, max})` for a single query value.
fn parse_string_number(
    raw: Option<&str>,
    field: &str,
    min: Option<i64>,
    max: Option<i64>,
    issues: &mut Vec<Value>,
) -> Option<i64> {
    let value = raw?;

    let Some(number) = js_parse_int(value) else {
        issues.push(issue_invalid_type(&[field], "number", "undefined"));
        return None;
    };

    if let Some(min) = min {
        if number < min {
            issues.push(json!({
                "origin": "number",
                "code": "too_small",
                "minimum": min,
                "inclusive": true,
                "path": [field],
                "message": format!("Too small: expected number to be >={min}"),
            }));
            return None;
        }
    }

    if let Some(max) = max {
        if number > max {
            issues.push(json!({
                "origin": "number",
                "code": "too_big",
                "maximum": max,
                "inclusive": true,
                "path": [field],
                "message": format!("Too big: expected number to be <={max}"),
            }));
            return None;
        }
    }

    Some(number)
}

// ---------------------------------------------------------------------------
// entity-name validation (`zc.entityName()`)
// ---------------------------------------------------------------------------

fn is_name_edge(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(ch, '_' | '@' | '(' | ')' | '%')
        || matches!(ch, 'А'..='Я' | 'а'..='я' | 'Ё' | 'ё')
}

fn is_name_middle(ch: char) -> bool {
    is_name_edge(ch)
        || matches!(
            ch,
            '.' | ',' | ':' | ';' | '\'' | '\u{00A0}' | '\u{180E}' | '\u{2000}'
                ..='\u{200B}'
                    | '\u{202F}'
                    | '\u{205F}'
                    | '\u{3000}'
                    | '\u{FEFF}'
                    | '|'
                    | ' '
                    | '-'
                    | '–'
                    | '—'
                    | '−'
                    | '$'
                    | '*'
                    | '&'
        )
}

/// `KEY_REG.test(value)`
fn matches_entity_key(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();

    let Some(first) = chars.first() else {
        return false;
    };

    if !is_name_edge(*first) {
        return false;
    }

    if chars.len() == 1 {
        return true;
    }

    if !is_name_edge(chars[chars.len() - 1]) {
        return false;
    }

    chars[1..chars.len() - 1]
        .iter()
        .all(|ch| is_name_middle(*ch))
}

// ---------------------------------------------------------------------------
// response models
// ---------------------------------------------------------------------------

/// `favoriteModel.format` — `sortAlias` is a `bytea` (`Buffer` on the wire).
#[derive(Debug, sqlx::FromRow)]
struct FavoriteRow {
    entry_id: i64,
    tenant_id: String,
    login: String,
    created_at: Option<DateTime<Utc>>,
    alias: Option<String>,
    display_alias: Option<String>,
    sort_alias: Option<Vec<u8>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FavoriteModel {
    entry_id: String,
    login: String,
    tenant_id: String,
    created_at: Option<String>,
    alias: Option<String>,
    display_alias: Option<String>,
    sort_alias: Option<Value>,
}

impl FavoriteRow {
    fn into_model(self) -> FavoriteModel {
        FavoriteModel {
            entry_id: encode_id(self.entry_id),
            login: self.login,
            tenant_id: self.tenant_id,
            created_at: self.created_at.map(format_timestamp),
            alias: self.alias,
            display_alias: self.display_alias,
            sort_alias: self.sort_alias.map(|bytes| {
                json!({
                    "type": "Buffer",
                    "data": bytes,
                })
            }),
        }
    }
}

/// `favoriteEntryModel.format`
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FavoriteEntryModel {
    entry_id: String,
    scope: String,
    #[serde(rename = "type")]
    entry_type: String,
    key: Option<String>,
    alias: Option<String>,
    display_alias: Option<String>,
    created_by: String,
    created_at: String,
    updated_at: String,
    hidden: bool,
    workbook_id: Option<String>,
    workbook_title: Option<String>,
    collection_title: Option<String>,
    collection_id: Option<String>,
    is_locked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    permissions: Option<Value>,
}

/// `favoriteEntryModelArray.format`
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FavoriteEntryModelArray {
    entries: Vec<FavoriteEntryModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_page_token: Option<String>,
}

fn format_timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ---------------------------------------------------------------------------
// database helpers
// ---------------------------------------------------------------------------

const ENTRY_LOOKUP_SQL: &str = r#"
SELECT entry_id, workbook_id, collection_id
FROM entries
WHERE entry_id = $1
  AND tenant_id = $2
  AND is_deleted = FALSE
LIMIT 1
"#;

#[derive(Debug, sqlx::FromRow)]
struct EntryLookupRow {
    entry_id: i64,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
}

/// The entry lookup shared by add/delete/rename; `addFavorite` additionally
/// validates the parent workbook.
async fn find_entry(state: &AppState, entry_id: i64) -> Result<EntryLookupRow, ApiError> {
    let row: Option<EntryLookupRow> = sqlx::query_as(ENTRY_LOOKUP_SQL)
        .bind(entry_id)
        .bind(state.config.tenant_id_override.as_str())
        .fetch_optional(&state.db.replica)
        .await?;

    row.ok_or_else(ApiError::not_exist_entry)
}

const FAVORITE_RETURNING: &str =
    "RETURNING entry_id, tenant_id, login, created_at, alias, display_alias, sort_alias";

const LIST_FAVORITES_SELECT: &str = r#"
SELECT
    e.entry_id       AS entry_id,
    f.alias          AS alias,
    f.display_alias  AS display_alias,
    e.scope::text    AS scope,
    e.type           AS entry_type,
    e.display_key    AS display_key,
    e.created_by     AS created_by,
    e.created_at     AS created_at,
    e.updated_at     AS updated_at,
    e.hidden         AS hidden,
    e.workbook_id    AS workbook_id,
    e.collection_id  AS collection_id,
    w.title          AS workbook_title,
    c.title          AS collection_title
FROM favorites AS f
JOIN entries AS e
    ON f.entity_id = e.entry_id
   AND f.entity_type = 'entry'
LEFT JOIN workbooks AS w ON e.workbook_id = w.workbook_id
LEFT JOIN collections AS c ON e.collection_id = c.collection_id
WHERE f.tenant_id = $1
  AND f.login = $2
  AND e.is_deleted = FALSE
"#;

#[derive(Debug, sqlx::FromRow)]
struct FavoriteEntryRow {
    entry_id: i64,
    alias: Option<String>,
    display_alias: Option<String>,
    scope: String,
    entry_type: String,
    display_key: Option<String>,
    created_by: String,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    hidden: bool,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    workbook_title: Option<String>,
    collection_title: Option<String>,
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// `GET /v1/favorites`
async fn list_favorites(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
    user: Option<Extension<AuthUser>>,
) -> Result<Response, HandlerError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let pairs = parse_query(raw_query.as_deref());

    let mut issues: Vec<Value> = Vec::new();

    // --- orderBy ----------------------------------------------------------
    let mut order_by: Option<(String, bool)> = None; // (field, descending)
    let order_by_subfield = query_first(&pairs, "orderBy[field]");
    let order_by_subdirection = query_first(&pairs, "orderBy[direction]");

    if query_first(&pairs, "orderBy").is_some() {
        issues.push(issue_invalid_type(&["orderBy"], "object", "string"));
    } else if order_by_subfield.is_some() || order_by_subdirection.is_some() {
        let field = order_by_subfield.unwrap_or_default();
        let direction = order_by_subdirection.unwrap_or_default();

        let field_ok = matches!(field, "name" | "createdAt");
        let direction_ok = matches!(direction, "asc" | "desc");

        if !field_ok {
            issues.push(issue_invalid_option(
                &["orderBy", "field"],
                &["name", "createdAt"],
            ));
        }
        if !direction_ok {
            issues.push(issue_invalid_option(
                &["orderBy", "direction"],
                &["asc", "desc"],
            ));
        }

        if field_ok && direction_ok {
            order_by = Some((field.to_owned(), direction == "desc"));
        }
    }

    // --- filters ----------------------------------------------------------
    let mut filters_name: Option<String> = None;

    if query_first(&pairs, "filters").is_some() {
        issues.push(issue_invalid_type(&["filters"], "object", "string"));
    } else if let Some(name) = query_first(&pairs, "filters[name]") {
        filters_name = Some(name.to_owned());
    }

    // --- page / pageSize --------------------------------------------------
    let page = parse_string_number(
        query_first(&pairs, "page"),
        "page",
        Some(0),
        None,
        &mut issues,
    )
    .unwrap_or(0);
    let page_size = parse_string_number(
        query_first(&pairs, "pageSize"),
        "pageSize",
        Some(1),
        Some(200),
        &mut issues,
    )
    .unwrap_or(100);

    // --- scope ------------------------------------------------------------
    let scope_values = query_values(&pairs, "scope");
    let mut scope_filter: Vec<String> = Vec::new();

    if !scope_values.is_empty() {
        let invalid = scope_values
            .iter()
            .find(|value| !ENTRY_SCOPES.contains(value))
            .copied();

        if let Some(invalid) = invalid {
            let quoted: Vec<String> = ENTRY_SCOPES
                .iter()
                .map(|value| format!("\"{value}\""))
                .collect();

            issues.push(json!({
                "code": "invalid_union",
                "errors": [
                    [{
                        "code": "invalid_value",
                        "values": ENTRY_SCOPES,
                        "path": [],
                        "message": format!(
                            "Invalid option: expected one of {}",
                            quoted.join("|")
                        ),
                    }],
                    [{
                        "expected": "array",
                        "code": "invalid_type",
                        "path": [],
                        "message": format!(
                            "Invalid input: expected array, received {}",
                            if scope_values.len() > 1 { "array" } else { "string" }
                        ),
                    }],
                ],
                "path": ["scope"],
                "message": "Invalid input",
            }));

            let _ = invalid;
        } else {
            // `scope.replace(/\s/g, '').split(',')` — the transform is applied
            // to a single string; arrays are used as-is.
            for value in scope_values {
                for part in value.replace(' ', "").split(',') {
                    if !part.is_empty() {
                        scope_filter.push(part.to_owned());
                    }
                }
            }
        }
    }

    // --- boolean flags ----------------------------------------------------
    let include_permissions_info = query_first(&pairs, "includePermissionsInfo")
        .map(is_true_arg)
        .unwrap_or(false);
    let ignore_workbook_entries = query_first(&pairs, "ignoreWorkbookEntries")
        .map(is_true_arg)
        .unwrap_or(false);
    let ignore_shared_entries = query_first(&pairs, "ignoreSharedEntries")
        .map(is_true_arg)
        .unwrap_or(false);

    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues).into());
    }

    // --- query ------------------------------------------------------------
    let mut sql = String::from(LIST_FAVORITES_SELECT);

    let like_pattern = filters_name.as_ref().map(|name| {
        let escaped = name
            .to_lowercase()
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("%{escaped}%")
    });

    if like_pattern.is_some() {
        sql.push_str("  AND COALESCE(f.alias, e.name) LIKE $3\n");
    }

    if !scope_filter.is_empty() {
        sql.push_str("  AND e.scope::text IN ('folder', '");
        // Values are validated against `EntryScope`, so they are safe literals.
        sql.push_str(&scope_filter.join("', '"));
        sql.push_str("')\n");
    }

    if ignore_workbook_entries {
        sql.push_str("  AND e.workbook_id IS NULL\n");
    }

    if ignore_shared_entries {
        sql.push_str("  AND e.collection_id IS NULL\n");
    }

    sql.push_str("ORDER BY CASE WHEN e.scope::text = 'folder' THEN 0 ELSE 1 END\n");

    let order_direction = match order_by.as_ref() {
        Some((_, true)) => "DESC",
        _ => "ASC",
    };

    match order_by.as_ref().map(|(field, _)| field.as_str()) {
        Some("createdAt") => {
            sql.push_str(&format!(
                ", e.created_at {order_direction}, e.entry_id {order_direction}\n"
            ));
        }
        Some("name") => {
            sql.push_str(&format!(
                ", COALESCE(f.sort_alias, e.sort_name) {order_direction}\n"
            ));
        }
        _ => {}
    }

    let param_page_size = if like_pattern.is_some() { 4 } else { 3 };
    sql.push_str(&format!(
        "LIMIT ${param_page_size} OFFSET ${}\n",
        param_page_size + 1
    ));

    let offset = page.saturating_mul(page_size);

    let mut query = sqlx::query_as::<_, FavoriteEntryRow>(&sql)
        .bind(state.config.tenant_id_override.as_str())
        .bind("unknown");

    if let Some(pattern) = like_pattern.as_ref() {
        query = query.bind(pattern);
    }

    let rows = query
        .bind(page_size)
        .bind(offset)
        .fetch_all(&state.db.replica)
        .await?;

    // `checkCollectionEntriesByPermission` calls `createCollectionEntry`, whose
    // open-source constructor throws for every collection entry.
    if rows.iter().any(|row| row.collection_id.is_some()) {
        return Err(ApiError::collection_entry_unsupported_scope().into());
    }

    let mut workbook_ids: Vec<i64> = rows.iter().filter_map(|row| row.workbook_id).collect();
    workbook_ids.sort_unstable();
    workbook_ids.dedup();
    let existing_workbooks = permissions::existing_workbook_ids(
        &state.db.replica,
        state.config.tenant_id_override.as_str(),
        &workbook_ids,
    )
    .await?;

    // `filterEntriesByPermission` (`get-favorites.js`): locked entries that
    // live in a workbook are not returned.
    let entries: Vec<FavoriteEntryModel> = rows
        .into_iter()
        .filter_map(|row| {
            let access = permissions::entry_access(
                row.workbook_id,
                row.collection_id,
                row.workbook_id
                    .is_some_and(|workbook_id| existing_workbooks.contains(&workbook_id)),
                editor,
                include_permissions_info,
            );
            if row.workbook_id.is_some() && access.is_locked {
                return None;
            }
            Some(FavoriteEntryModel {
                entry_id: encode_id(row.entry_id),
                scope: row.scope,
                entry_type: row.entry_type,
                key: row.display_key,
                alias: row.alias,
                display_alias: row.display_alias,
                created_by: row.created_by,
                created_at: row.created_at.map(format_timestamp).unwrap_or_default(),
                updated_at: row.updated_at.map(format_timestamp).unwrap_or_default(),
                hidden: row.hidden,
                workbook_id: encode_id_opt(row.workbook_id),
                workbook_title: row.workbook_title,
                collection_title: row.collection_title,
                collection_id: encode_id_opt(row.collection_id),
                is_locked: access.is_locked,
                permissions: access.permissions,
            })
        })
        .collect();

    // `Utils.getOptimisticNextPageToken`
    let next_page_token = if page >= 0 && entries.len() as i64 == page_size {
        Some((page + 1).to_string())
    } else {
        None
    };

    Ok(Json(FavoriteEntryModelArray {
        entries,
        next_page_token,
    })
    .into_response())
}

/// `POST /v1/favorites/:entryId`
async fn add_favorite(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    user: Option<Extension<AuthUser>>,
) -> Result<Response, HandlerError> {
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;

    let entry = find_entry(&state, entry_id).await?;

    if let Some(workbook_id) = entry.workbook_id {
        // `getWorkbook({ctx}, {workbookId})`: live workbook of the same tenant.
        let workbook: Option<(i64,)> = sqlx::query_as(
            r#"
SELECT workbook_id
FROM workbooks
WHERE deleted_at IS NULL
  AND workbook_id = $1
  AND tenant_id = $2
LIMIT 1
"#,
        )
        .bind(workbook_id)
        .bind(state.config.tenant_id_override.as_str())
        .fetch_optional(&state.db.replica)
        .await?;

        if workbook.is_none() {
            return Err(ApiError::workbook_not_exists().into());
        }
    } else if entry.collection_id.is_some() {
        // `checkCollectionEntryPermission` → `createCollectionEntry` throws.
        return Err(ApiError::collection_entry_unsupported_scope().into());
    }

    let login = user
        .as_ref()
        .and_then(|Extension(user)| user.user.login.clone())
        .unwrap_or_else(|| "unknown".to_owned());

    let sql = format!(
        r#"
INSERT INTO favorites (tenant_id, entry_id, entity_id, entity_type, login)
VALUES ($1, $2, $2, 'entry', $3)
{FAVORITE_RETURNING}
"#
    );

    let row: FavoriteRow = sqlx::query_as(&sql)
        .bind(state.config.tenant_id_override.as_str())
        .bind(entry.entry_id)
        .bind(login.as_str())
        .fetch_one(&state.db.primary)
        .await?;

    Ok(Json(row.into_model()).into_response())
}

/// `DELETE /v1/favorites/:entryId`
async fn delete_favorite(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    user: Option<Extension<AuthUser>>,
) -> Result<Response, HandlerError> {
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;

    let entry = find_entry(&state, entry_id).await?;

    let login = user
        .as_ref()
        .and_then(|Extension(user)| user.user.login.clone())
        .unwrap_or_else(|| "unknown".to_owned());

    let sql = format!(
        r#"
DELETE FROM favorites
WHERE entity_id = $1
  AND entity_type = 'entry'
  AND tenant_id = $2
  AND login = $3
{FAVORITE_RETURNING}
"#
    );

    let rows: Vec<FavoriteRow> = sqlx::query_as(&sql)
        .bind(entry.entry_id)
        .bind(state.config.tenant_id_override.as_str())
        .bind(login.as_str())
        .fetch_all(&state.db.primary)
        .await?;

    let models: Vec<FavoriteModel> = rows.into_iter().map(FavoriteRow::into_model).collect();

    Ok(Json(models).into_response())
}

/// `POST /v1/favorites/:entryId/rename`
async fn rename_favorite(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    headers: HeaderMap,
    user: Option<Extension<AuthUser>>,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;

    let body = parse_body(&headers, &body)?;

    let map = match &body {
        Value::Object(map) => map,
        other => {
            return Err(ApiError::validation(json!([issue_invalid_type(
                &[],
                "object",
                js_type_name(other),
            )]))
            .into())
        }
    };

    let name: Option<String> = match map.get("name") {
        None => {
            return Err(ApiError::validation(json!([issue_invalid_type(
                &["name"],
                "string",
                "undefined",
            )]))
            .into())
        }
        Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(other) => {
            return Err(ApiError::validation(json!([issue_invalid_type(
                &["name"],
                "string",
                js_type_name(other),
            )]))
            .into())
        }
    };

    if let Some(value) = name.as_deref() {
        let mut issues = Vec::new();

        if value.contains('/') {
            issues.push(json!({
                "code": "custom",
                "path": ["name"],
                "message": "String should not contain the symbol /",
            }));
        }

        if !matches_entity_key(value) {
            issues.push(json!({
                "code": "custom",
                "path": ["name"],
                "message": format!("String {SYMBOLS_VALIDATE_MESSAGE}"),
            }));
        }

        if !issues.is_empty() {
            return Err(ApiError::validation_issues(issues).into());
        }
    }

    let entry = find_entry(&state, entry_id).await?;

    let display_alias = match name {
        Some(value) if !value.is_empty() => Some(value),
        _ => None,
    };
    let alias = display_alias.as_ref().map(|value| value.to_lowercase());

    let login = user
        .as_ref()
        .and_then(|Extension(user)| user.user.login.clone())
        .unwrap_or_else(|| "unknown".to_owned());

    let sql = format!(
        r#"
UPDATE favorites
SET alias = $1,
    display_alias = $2
WHERE entity_id = $3
  AND entity_type = 'entry'
  AND tenant_id = $4
  AND login = $5
{FAVORITE_RETURNING}
"#
    );

    let row: Option<FavoriteRow> = sqlx::query_as(&sql)
        .bind(alias)
        .bind(display_alias)
        .bind(entry.entry_id)
        .bind(state.config.tenant_id_override.as_str())
        .bind(login.as_str())
        .fetch_optional(&state.db.primary)
        .await?;

    match row {
        Some(row) => Ok(Json(row.into_model()).into_response()),
        None => Err(favorite_not_exists().into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_name_matches_key_reg() {
        for valid in [
            "My Alias",
            "abc",
            "a",
            "Отчёт за 2024",
            "_x_",
            "a b-c",
            "x@y%z",
            "(test)",
            "имя (копия)",
            "a–b",
        ] {
            assert!(matches_entity_key(valid), "{valid} should match");
        }

        for invalid in ["", " ", " a", "a ", "a/b", "a!b", "a\"b", "a\nb", "a#b"] {
            assert!(!matches_entity_key(invalid), "{invalid} should not match");
        }
    }

    #[test]
    fn string_number_matches_parse_int() {
        assert_eq!(js_parse_int("0"), Some(0));
        assert_eq!(js_parse_int("12"), Some(12));
        assert_eq!(js_parse_int("12abc"), Some(12));
        assert_eq!(js_parse_int("1.9"), Some(1));
        assert_eq!(js_parse_int(" 7"), Some(7));
        assert_eq!(js_parse_int("+5"), Some(5));
        assert_eq!(js_parse_int("-3"), Some(-3));
        assert_eq!(js_parse_int("abc"), None);
        assert_eq!(js_parse_int(""), None);
        assert_eq!(js_parse_int("  "), None);
    }

    #[test]
    fn true_flags_match_string_boolean() {
        assert!(is_true_arg("1"));
        assert!(is_true_arg("TRue"));
        assert!(!is_true_arg("yes"));
        assert!(!is_true_arg("on"));
    }

    #[test]
    fn invalid_option_message_matches_zod() {
        assert_eq!(
            issue_invalid_option(&["orderBy", "field"], &["name", "createdAt"])["message"],
            json!("Invalid option: expected one of \"name\"|\"createdAt\"")
        );
    }
}
