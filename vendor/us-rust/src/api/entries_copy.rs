//! Copy endpoints of `src/controllers/entries/*`.
//!
//! Port targets:
//!   POST /v2/entries/{entry_id}/copy   copy-entry-to-workbook
//!   POST /v2/copy-entries              copy-entries-to-workbook
//!
//! # Node implementation
//!
//! `copyEntryToWorkbookController` (`src/controllers/entries/copy-entry-to-workbook.ts`)
//! → `copyEntryToWorkbook` (`src/services/new/entry/copy-entry-to-workbook.ts`):
//! a single transaction that reads the source entry, copies its current revision
//! (published when it exists, saved otherwise) into a new entry + revision inside
//! the target workbook, syncing the `links` table for the new entry.
//!
//! `copyEntriesToWorkbookController` (`src/controllers/entries/copy-entries-to-workbook.ts`)
//! → `copyEntriesToWorkbook` (`src/services/new/entry/copy-entries-to-workbook.ts`)
//! → `copyToWorkbook` (`src/services/entry/actions/copy-to-workbook.ts`):
//! the legacy bulk action, which additionally resolves name collisions
//! ("`name (COPY n)`"), checks both workbooks, inserts every entry in one
//! `insertGraph` and then re-writes the copied revisions after replacing the old
//! encoded entry ids with the new ones (`crossSyncCopiedJoinedEntryRevisions` in
//! `src/services/new/workbook/copy-workbook.ts`).
//!
//! # Request parsing
//!
//! Both controllers are `withContract` routes with `manualDecodeId = true`, i.e.
//! the `decodeId` middleware is skipped and the contract zod schemas decode the
//! ids (`zc.encodedId`, `zc.encodedIdArray`). The whole request is validated as a
//! single `z.object({body, params})`, so issues are reported in schema order with
//! the leading `body`/`params` path segment stripped
//! (`appValidationErrorHandler` in `src/components/api-docs/utils.ts`).
//!
//! Note that `zc.encodedIdArray` decodes ids inside an *async* transform: zod
//! appends those `custom` issues after every synchronous issue of the request
//! (observed on us v1.196.0), which is why a bad `workbookId` is reported before
//! a bad id inside `entryIds`.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Extension;
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use sqlx::postgres::Postgres;
use sqlx::{FromRow, Transaction};

use crate::api::workbooks::{
    issue_custom, issue_invalid_type, issue_too_big_array, issue_too_small_array, key_reg_ok,
    received_name,
};
use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::id::{decode_id, encode_id};
use crate::permissions;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    crate::api::dual_router(vec![
        ("/v2/entries/{entry_id}/copy", post(copy_entry)),
        ("/v2/copy-entries", post(copy_entries)),
    ])
}

/// `makeUserId(ctx.get('info').user.userId)` with the open-source defaults
/// (`resolveSpecialTokens` → `userId = "systemId"`).
const CREATED_BY: &str = "uid:systemId";

/// `EntryType.File`, `EntryType.GsheetsV2`, `EntryType.YaDocs` — the types
/// `Utils.isFileConnection` treats as file connections.
const FILE_CONNECTION_TYPES: [&str; 3] = ["file", "gsheets_v2", "yadocs"];

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Either an API error (JSON presenter) or a raw express-style response
/// (`Invalid JSON supplied` is `text/html` and is produced by body-parser, not
/// by the error presenter).
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

/// `EntryIsNotInWorkbookError`.
fn entry_is_not_in_workbook() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("ENTRY_IS_NOT_IN_WORKBOOK"),
        "The entry is not in a workbook",
    )
}

/// `WorkbookCopyFileConnectionError` (canonical message of the single-entry copy).
fn workbook_copy_file_connection() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("WORKBOOK_COPY_FILE_CONNECTION_ERROR"),
        "Copying workbooks with file connections is forbidden",
    )
}

/// `WorkbookCopyFileConnectionError({message})` (bulk copy uses a custom message).
fn workbook_copy_file_connection_with(message: String) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("WORKBOOK_COPY_FILE_CONNECTION_ERROR"),
        message,
    )
}

/// `new AppError(US_ERRORS.NOT_EXIST_REVISION, {code})` — 404.
fn not_exist_revision() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        Some("NOT_EXIST_REVISION"),
        "The revision doesn't exist",
    )
}

/// `NotExistEntryError({message: "Entries don't exist"})` — 404.
fn entries_dont_exist() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        Some("NOT_EXIST_ENTRY"),
        "Entries don't exist",
    )
}

/// `NotExistEntryError({message: \`Entry ${id} doesn't exist\`})` — 404.
fn entry_doesnt_exist(encoded_id: String) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        Some("NOT_EXIST_ENTRY"),
        format!("Entry {encoded_id} doesn't exist"),
    )
}

/// `FolderCopyDeniedError` — 403.
fn folder_copy_denied() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        Some("FOLDER_COPY_DENIED"),
        "Folders cannot be copied",
    )
}

/// `EntryWithoutWorkbookIdCopyDeniedError({message})` — 403.
fn entry_without_workbook_id(message: String) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        Some("ENTRY_WITHOUT_WORKBOOK_ID_COPY_DENIED"),
        message,
    )
}

/// `EntriesWithDifferentWorkbookIdsCopyDeniedError({message})` — 403.
fn entries_with_different_workbook_ids(message: String) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        Some("ENTRIES_WITH_DIFFERENT_WORKBOOK_IDS_COPY_DENIED"),
        message,
    )
}

/// `IncorrectLinkError({details: {invalidLinkIds}})` — 400.
fn incorrect_link(invalid_link_ids: Value) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("INCORRECT_LINK_ERROR"),
        "Some of the provided links are incorrect",
    )
    .with_details(json!({ "invalidLinkIds": invalid_link_ids }))
}

// ---------------------------------------------------------------------------
// request parsing (express body-parser + zod contract)
// ---------------------------------------------------------------------------

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

/// `express.json({strict: true, limit: '50mb'})` as wired by expresskit:
///   * a zero-length body leaves `req.body === {}`;
///   * a JSON body must be an object or an array ("Invalid JSON supplied");
///   * a non-empty body with any other content type is never parsed, so
///     `req.body === undefined` and zod rejects it with an *empty* issue list.
///
/// body-parser matches `application/json` case-insensitively (type-is) but not
/// the `+json` suffix types for the default parser type.
fn parse_body(headers: &HeaderMap, body: &[u8]) -> Result<Value, HandlerError> {
    if body.is_empty() {
        return Ok(Value::Object(Map::new()));
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let is_json = content_type.starts_with("application/json");

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

/// `withContract`'s content-type guard: only requests that carry a body are
/// checked, and the check is a plain case-sensitive `String#includes`.
fn require_json_content_type(headers: &HeaderMap) -> Result<(), HandlerError> {
    let has_body = headers.contains_key(header::TRANSFER_ENCODING)
        || headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .map(|value| value != "0")
            .unwrap_or(false);

    if !has_body {
        return Ok(());
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    if content_type.contains("application/json") {
        Ok(())
    } else {
        Err(ApiError::validation(json!([])).into())
    }
}

/// `issue_invalid_type` with a numeric array index in the issue path
/// (`path: ["entryIds", 0]`, not `["entryIds", "0"]`).
fn issue_invalid_type_at_index(field: &str, index: usize, expected: &str, received: &str) -> Value {
    json!({
        "expected": expected,
        "code": "invalid_type",
        "path": [field, index],
        "message": format!("Invalid input: expected {expected}, received {received}"),
    })
}

/// `zc.entityName()` — `!value.includes('/')` then `KEY_REG.test(value)`.
///
/// Messages of `src/components/zod/custom-types/entity-name.ts`; the only
/// difference with `workbooks::entity_name_issues` is the issue path (`name`).
fn entity_name_issues(name: &str) -> Vec<Value> {
    let mut issues = Vec::new();

    if name.contains('/') {
        issues.push(issue_custom(
            &["name"],
            "String should not contain the symbol /",
        ));
    }

    if !key_reg_ok(name) {
        issues.push(issue_custom(
            &["name"],
            "String should start and end with A-Za-zА-Яа-яЁё0-9_@()% and can contain only \
             symbols: A-Za-zА-Яа-яЁё0-9_@()%.,:;'|-–—−$*& and spaces",
        ));
    }

    issues
}

#[derive(Debug, Default)]
struct CopyEntryArgs {
    workbook_id: Option<i64>,
    name: Option<String>,
}

/// `z.object({workbookId: zc.encodedId().optional(), name: zc.entityName().optional()})`.
fn validate_copy_entry(body: &Value) -> (CopyEntryArgs, Vec<Value>) {
    let mut args = CopyEntryArgs::default();
    let mut issues = Vec::new();

    let Some(object) = body.as_object() else {
        issues.push(issue_invalid_type(&[], "object", received_name(Some(body))));
        return (args, issues);
    };

    match object.get("workbookId") {
        None => {}
        Some(Value::String(raw)) => match decode_id(raw) {
            Ok(id) => args.workbook_id = Some(id),
            Err(_) => issues.push(ApiError::encoded_id_issue("workbookId", raw)),
        },
        Some(other) => issues.push(issue_invalid_type(
            &["workbookId"],
            "string",
            received_name(Some(other)),
        )),
    }

    match object.get("name") {
        None => {}
        Some(Value::String(name)) => {
            issues.extend(entity_name_issues(name));
            args.name = Some(name.clone());
        }
        Some(other) => issues.push(issue_invalid_type(
            &["name"],
            "string",
            received_name(Some(other)),
        )),
    }

    (args, issues)
}

#[derive(Debug)]
struct CopyEntriesArgs {
    entry_ids: Vec<i64>,
    workbook_id: i64,
    /// `Utils.decodeId('')` returns `''` (no throw), which later reaches the
    /// `whereIn` and makes PostgreSQL reject the empty bigint literal
    /// (`invalid input syntax for type bigint`) — a `DBError`, i.e. 500.
    empty_id: bool,
}

/// `z.object({entryIds: zc.encodedIdArray({min: 1, max: 1000}), workbookId: zc.encodedId()})`.
fn validate_copy_entries(body: &Value) -> (Option<CopyEntriesArgs>, Vec<Value>) {
    let Some(object) = body.as_object() else {
        return (
            None,
            vec![issue_invalid_type(&[], "object", received_name(Some(body)))],
        );
    };

    // Synchronous part first (zod's shape order: `entryIds` before `workbookId`).
    let mut entry_issues: Vec<Value> = Vec::new();
    let mut raw_ids: Option<Vec<String>> = None;

    match object.get("entryIds") {
        None => entry_issues.push(issue_invalid_type(&["entryIds"], "array", "undefined")),
        Some(Value::Array(items)) => {
            if items.is_empty() {
                entry_issues.push(issue_too_small_array(&["entryIds"], 1));
            }
            if items.len() > 1000 {
                entry_issues.push(issue_too_big_array(&["entryIds"], 1000));
            }

            if entry_issues.is_empty() {
                let mut values = Vec::with_capacity(items.len());

                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(raw) => values.push(raw.clone()),
                        other => entry_issues.push(issue_invalid_type_at_index(
                            "entryIds",
                            index,
                            "string",
                            received_name(Some(other)),
                        )),
                    }
                }

                if entry_issues.is_empty() {
                    raw_ids = Some(values);
                }
            }
        }
        Some(other) => entry_issues.push(issue_invalid_type(
            &["entryIds"],
            "array",
            received_name(Some(other)),
        )),
    }

    let mut workbook_issues: Vec<Value> = Vec::new();
    let mut workbook_id = None;

    match object.get("workbookId") {
        None => workbook_issues.push(issue_invalid_type(&["workbookId"], "string", "undefined")),
        Some(Value::String(raw)) => match decode_id(raw) {
            Ok(id) => workbook_id = Some(id),
            Err(_) => workbook_issues.push(ApiError::encoded_id_issue("workbookId", raw)),
        },
        Some(other) => workbook_issues.push(issue_invalid_type(
            &["workbookId"],
            "string",
            received_name(Some(other)),
        )),
    }

    // `zc.encodedIdArray` decodes inside an async transform: its `custom`
    // issues are appended after every synchronous issue of the request.
    let mut custom_issues: Vec<Value> = Vec::new();
    let mut entry_ids = Vec::new();
    let mut empty_id = false;

    if let Some(values) = raw_ids {
        for raw in values {
            if raw.is_empty() {
                // `Utils.decodeId('')` is `''` — kept as-is for the DB query.
                empty_id = true;
                continue;
            }

            match decode_id(&raw) {
                Ok(id) => entry_ids.push(id),
                // `zc.encodedIdArraySafe` decodes items inside a transform, so
                // an undecodable item is always the `custom` format issue (no
                // `too_small`/`too_big`, unlike the scalar `zc.encodedId()`).
                Err(_) => custom_issues.push(json!({
                    "code": "custom",
                    "message": format!("id '{raw}' has incorrect format"),
                    "path": ["entryIds"],
                })),
            }
        }
    }

    let mut issues = entry_issues;
    issues.extend(workbook_issues);
    issues.extend(custom_issues);

    if issues.is_empty() {
        (
            workbook_id.map(|workbook_id| CopyEntriesArgs {
                entry_ids,
                workbook_id,
                empty_id,
            }),
            issues,
        )
    } else {
        (None, issues)
    }
}

// ---------------------------------------------------------------------------
// name helpers (`src/utils/utils.ts`)
// ---------------------------------------------------------------------------

/// JS `\s` for the copy suffix (`\s` also matches U+FEFF).
fn is_js_space(character: char) -> bool {
    character.is_whitespace() || character == '\u{FEFF}'
}

/// `Utils.getNameByKey` — `key.match(/([^/]+)\/?$/)`, i.e. the last non-empty
/// path segment with at most one trailing slash. Returns `None` where Node
/// throws `Cannot extract name`.
fn name_by_key(key: &str) -> Option<String> {
    let mut parts: Vec<&str> = key.split('/').collect();
    let mut trailing = 0;

    while parts.last() == Some(&"") {
        parts.pop();
        trailing += 1;
    }

    if trailing > 1 {
        return None;
    }

    parts
        .last()
        .filter(|segment| !segment.is_empty())
        .map(|segment| (*segment).to_owned())
}

/// Leftmost match of `/\s\(COPY\s([^)]+)\)$/`; returns `(start, capture)`.
fn copy_suffix(name: &str) -> Option<(usize, String)> {
    let chars: Vec<char> = name.chars().collect();

    if chars.last() != Some(&')') {
        return None;
    }

    let end = chars.len() - 1;

    for start in 0..end {
        if !is_js_space(chars[start]) {
            continue;
        }

        if chars.get(start + 1) != Some(&'(')
            || chars.get(start + 2) != Some(&'C')
            || chars.get(start + 3) != Some(&'O')
            || chars.get(start + 4) != Some(&'P')
            || chars.get(start + 5) != Some(&'Y')
        {
            continue;
        }

        match chars.get(start + 6) {
            Some(character) if is_js_space(*character) => {}
            _ => continue,
        }

        let capture_start = start + 7;

        if capture_start >= end {
            continue;
        }

        let capture: String = chars[capture_start..end].iter().collect();

        if capture.contains(')') {
            continue;
        }

        return Some((start, capture));
    }

    None
}

/// `Utils.getNameWithoutCopyNumber` — strips a trailing ` (COPY n)`.
fn name_without_copy_number(name: &str) -> String {
    match copy_suffix(name) {
        Some((start, _)) => name.chars().take(start).collect(),
        None => name.to_owned(),
    }
}

/// JavaScript `Number(value)` for the values `Utils.getCopyNumber` can produce;
/// `NaN` is reported as `None`.
fn js_number(value: &str) -> Option<f64> {
    let trimmed = value.trim_matches(is_js_space);

    if trimmed.is_empty() {
        // `Number('') === 0`
        return Some(0.0);
    }

    let lower = trimmed.to_ascii_lowercase();

    for (prefix, sign) in [("infinity", 1.0), ("+infinity", 1.0), ("-infinity", -1.0)] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            return rest.is_empty().then_some(sign * f64::INFINITY);
        }
    }

    if let Some(hex) = lower.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16).ok().map(|value| value as f64);
    }

    trimmed.parse::<f64>().ok()
}

/// `Utils.getCopyNumber` — `Number(match[1])`, `NaN` when there is no suffix.
fn get_copy_number(name: &str) -> Option<f64> {
    let (_, capture) = copy_suffix(name)?;
    js_number(&capture)
}

/// JavaScript `String(number)` for the counters used by `setCopyNumber`.
fn format_js_number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// `Utils.setCopyNumber` — `count === 0 ? name : \`${name} (COPY ${count})\``.
fn set_copy_number(name: &str, count: f64) -> String {
    if count == 0.0 {
        name.to_owned()
    } else {
        format!("{name} (COPY {})", format_js_number(count))
    }
}

/// `resolveEntriesNameCollisions` (`src/services/new/entry/utils/resolveNameCollisions.ts`).
///
/// `existing` is ordered by `sort_name` ascending — the order of the Node query
/// — because the counters are built with `max`/increment and therefore depend on
/// it when copy-numbered and plain names of the same base coexist.
fn resolve_entries_name_collisions(
    existing: &[(i64, Option<String>)],
    adding: &[(i64, Option<String>)],
) -> Result<HashMap<i64, String>, ApiError> {
    let mut collisions: HashMap<String, f64> = HashMap::new();

    for (_, display_key) in existing {
        let Some(display_key) = display_key else {
            continue;
        };

        let name = name_by_key(display_key).ok_or_else(ApiError::internal)?;
        let without_copy = name_without_copy_number(&name);
        let prepared = without_copy.to_lowercase();
        let copy_number = get_copy_number(&name);

        match copy_number {
            Some(copy_number) if copy_number > 0.0 => {
                let current = collisions.get(&prepared).copied().unwrap_or(0.0);
                collisions.insert(prepared, current.max(copy_number) + 1.0);
            }
            _ => {
                let current = collisions.get(&prepared).copied().unwrap_or(0.0);
                collisions.insert(prepared, current + 1.0);
            }
        }
    }

    let mut names = HashMap::new();

    for (entry_id, display_key) in adding {
        let display_key = display_key.as_deref().ok_or_else(ApiError::internal)?;
        let name = name_by_key(display_key).ok_or_else(ApiError::internal)?;
        let copy_number = get_copy_number(&name);
        let without_copy = name_without_copy_number(&name);
        let prepared = without_copy.to_lowercase();

        let current = collisions.get(&prepared).copied().unwrap_or(0.0);
        let collision_count = match copy_number {
            Some(copy_number) if copy_number > 0.0 => current.max(copy_number),
            _ => current,
        };

        let new_name = set_copy_number(&without_copy, collision_count);
        collisions.insert(prepared, collision_count + 1.0);
        names.insert(*entry_id, new_name);
    }

    Ok(names)
}

// ---------------------------------------------------------------------------
// links (`src/db/models/links`)
// ---------------------------------------------------------------------------

/// `Link.sync` (`src/db/models/links/index.ts`): decode every `toId`, replace
/// the rows of the `links` table for `entry_id`, insert the decoded ones.
async fn sync_links(
    transaction: &mut Transaction<'_, Postgres>,
    entry_id: i64,
    links: &Value,
) -> Result<(), ApiError> {
    let Some(object) = links.as_object() else {
        return Err(incorrect_link(json!({})));
    };

    let mut invalid: Map<String, Value> = Map::new();
    let mut decoded: Vec<(i64, String)> = Vec::with_capacity(object.len());

    for (name, value) in object {
        match value {
            Value::String(raw) => match decode_id(raw) {
                Ok(to_id) => decoded.push((to_id, name.clone())),
                Err(_) => {
                    invalid.insert(name.clone(), value.clone());
                }
            },
            other => {
                invalid.insert(name.clone(), other.clone());
            }
        }
    }

    if !invalid.is_empty() {
        return Err(incorrect_link(Value::Object(invalid)));
    }

    // `Links.sync` wraps every non-presentable failure as `SYNC_LINKS_FAILED`.
    sqlx::query("DELETE FROM links WHERE from_id = $1")
        .bind(entry_id)
        .execute(&mut **transaction)
        .await
        .map_err(|_| ApiError::sync_links_failed())?;

    if !decoded.is_empty() {
        let ids: Vec<i64> = decoded.iter().map(|(id, _)| *id).collect();
        let names: Vec<String> = decoded.iter().map(|(_, name)| name.clone()).collect();

        sqlx::query(
            "INSERT INTO links (from_id, to_id, name) \
             SELECT $1, l.to_id, l.name FROM UNNEST($2::bigint[], $3::text[]) AS l(to_id, name) \
             ON CONFLICT (from_id, to_id, name) DO NOTHING",
        )
        .bind(entry_id)
        .bind(&ids)
        .bind(&names)
        .execute(&mut **transaction)
        .await
        .map_err(|_| ApiError::sync_links_failed())?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// shared database helpers
// ---------------------------------------------------------------------------

async fn next_id(transaction: &mut Transaction<'_, Postgres>) -> Result<i64, ApiError> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT get_id()")
        .fetch_one(&mut **transaction)
        .await?)
}

/// Node values that are `null` in JavaScript (both a missing JSON value and the
/// JSON literal `null`) are written back as SQL `NULL`, never as `jsonb 'null'`.
fn js_null(value: Option<Value>) -> Option<Value> {
    match value {
        Some(Value::Null) | None => None,
        value => value,
    }
}

/// Batched `sync_links`: mirrors the single-entry version, but deletes and
/// inserts the links of all copied entries with two statements.
async fn sync_links_bulk(
    transaction: &mut Transaction<'_, Postgres>,
    rows: &[(i64, Value)],
) -> Result<(), ApiError> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut from_ids: Vec<i64> = Vec::with_capacity(rows.len());
    let mut to_ids: Vec<i64> = Vec::with_capacity(rows.len());
    let mut names: Vec<String> = Vec::with_capacity(rows.len());
    let mut invalid: Map<String, Value> = Map::new();

    for (from_id, links) in rows {
        let Some(object) = links.as_object() else {
            return Err(incorrect_link(json!({})));
        };

        from_ids.push(*from_id);

        for (name, value) in object {
            match value {
                Value::String(raw) => match decode_id(raw) {
                    Ok(to_id) => {
                        to_ids.push(to_id);
                        names.push(name.clone());
                    }
                    Err(_) => {
                        invalid.insert(name.clone(), value.clone());
                    }
                },
                other => {
                    invalid.insert(name.clone(), other.clone());
                }
            }
        }
    }

    if !invalid.is_empty() {
        return Err(incorrect_link(Value::Object(invalid)));
    }

    sqlx::query("DELETE FROM links WHERE from_id = ANY($1)")
        .bind(&from_ids)
        .execute(&mut **transaction)
        .await
        .map_err(|_| ApiError::sync_links_failed())?;

    if !to_ids.is_empty() {
        // `from_ids` repeats per link, so expand it with the same length.
        let mut flat_from: Vec<i64> = Vec::with_capacity(to_ids.len());
        for (from_id, links) in rows {
            let count = links.as_object().map(Map::len).unwrap_or(0);
            flat_from.extend(std::iter::repeat_n(*from_id, count));
        }

        sqlx::query(
            "INSERT INTO links (from_id, to_id, name) \
             SELECT u.from_id, u.to_id, u.name \
             FROM UNNEST($1::bigint[], $2::bigint[], $3::text[]) AS u(from_id, to_id, name) \
             ON CONFLICT (from_id, to_id, name) DO NOTHING",
        )
        .bind(&flat_from)
        .bind(&to_ids)
        .bind(&names)
        .execute(&mut **transaction)
        .await
        .map_err(|_| ApiError::sync_links_failed())?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// POST /v2/entries/{entryId}/copy
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct CopyEntrySource {
    name: Option<String>,
    scope: String,
    entry_type: String,
    workbook_id: Option<i64>,
    saved_id: Option<i64>,
    published_id: Option<i64>,
    unversioned_data: Option<Value>,
    hidden: bool,
}

#[derive(Debug, FromRow)]
struct CopyEntryRevision {
    data: Option<Value>,
    meta: Option<Value>,
    annotation: Option<Value>,
    links: Option<Value>,
}

async fn fetch_copy_entry_source(
    transaction: &mut Transaction<'_, Postgres>,
    entry_id: i64,
    tenant_id: &str,
) -> Result<Option<CopyEntrySource>, ApiError> {
    // `Entry.query(trx).where({entryId, tenantId, isDeleted: false}).first()`
    Ok(sqlx::query_as(
        "SELECT name, scope::text AS scope, type AS entry_type, workbook_id, saved_id, \
         published_id, unversioned_data, hidden \
         FROM entries WHERE entry_id = $1 AND tenant_id = $2 AND is_deleted = FALSE LIMIT 1",
    )
    .bind(entry_id)
    .bind(tenant_id)
    .fetch_optional(&mut **transaction)
    .await?)
}

async fn fetch_copy_entry_revision(
    transaction: &mut Transaction<'_, Postgres>,
    entry_id: i64,
    source: &CopyEntrySource,
) -> Result<CopyEntryRevision, ApiError> {
    // `RevisionModel.query(trx).where({entryId, revId: publishedId ?? savedId})`
    let revision_id = match source.published_id.or(source.saved_id) {
        Some(revision_id) => revision_id,
        // `revId: undefined` makes knex throw, which the presenter reports as a
        // generic 500.
        None => return Err(ApiError::internal()),
    };

    let revision: Option<CopyEntryRevision> = sqlx::query_as(
        "SELECT data, meta, annotation, links FROM revisions \
         WHERE entry_id = $1 AND rev_id = $2 LIMIT 1",
    )
    .bind(entry_id)
    .bind(revision_id)
    .fetch_optional(&mut **transaction)
    .await?;

    revision.ok_or_else(not_exist_revision)
}

async fn copy_entry(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    user: Option<Extension<AuthUser>>,
    request_log: Option<Extension<crate::logging::RequestLog>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    if !permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user)) {
        return Err(ApiError::access_service_permission_denied().into());
    }
    // express body-parser first, then `withContract`'s content-type guard, then
    // the combined zod schema (`body` before `params`).
    let body = parse_body(&headers, &body)?;
    require_json_content_type(&headers)?;

    let (args, mut issues) = validate_copy_entry(&body);

    let entry_id = match decode_id(&raw_entry_id) {
        Ok(entry_id) => Some(entry_id),
        Err(_) => {
            issues.push(ApiError::encoded_id_issue("entryId", &raw_entry_id));
            None
        }
    };

    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues).into());
    }

    let entry_id = entry_id.expect("validated above");
    let tenant_id = state.config.tenant_id_override.clone();

    let mut transaction = state.db.primary.begin().await?;

    let source = fetch_copy_entry_source(&mut transaction, entry_id, &tenant_id).await?;

    let source = match source {
        Some(source) => source,
        None => return Err(ApiError::not_exist_entry().into()),
    };

    let source_workbook_id = match source.workbook_id {
        Some(workbook_id) => workbook_id,
        None => return Err(entry_is_not_in_workbook().into()),
    };

    if source.scope == "connection" && FILE_CONNECTION_TYPES.contains(&source.entry_type.as_str()) {
        return Err(workbook_copy_file_connection().into());
    }

    let target_workbook_id = args.workbook_id.unwrap_or(source_workbook_id);

    if let Some(Extension(ref log)) = request_log {
        let mut fields = Map::new();
        fields.insert("entryId".to_string(), Value::String(raw_entry_id.clone()));
        fields.insert(
            "workbookId".to_string(),
            Value::String(encode_id(target_workbook_id)),
        );
        fields.insert(
            "copiedBy".to_string(),
            Value::String("uid:systemId".to_string()),
        );
        crate::logging::event(
            &log,
            "copyEntryToWorkbook",
            "COPY_ENTRY_TO_WORKBOOK_START",
            fields,
        );
        crate::logging::event(&log, "copyEntryToWorkbook", "GET_ENTRY_MODEL", Map::new());
        let mut fields = Map::new();
        fields.insert(
            "targetWorkbookId".to_string(),
            Value::String(encode_id(target_workbook_id)),
        );
        crate::logging::event(
            &log,
            "copyEntryToWorkbook",
            "GET_TARGET_WORKBOOK_ID",
            fields,
        );
    }

    let revision = fetch_copy_entry_revision(&mut transaction, entry_id, &source).await?;

    if let Some(Extension(ref log)) = request_log {
        if let Some(rev_id) = source.published_id.or(source.saved_id) {
            let mut fields = Map::new();
            fields.insert(
                "targetRevisionId".to_string(),
                Value::String(encode_id(rev_id)),
            );
            crate::logging::event(&log, "copyEntryToWorkbook", "GET_TARGET_REVISION", fields);
        }
    }

    let new_entry_id = next_id(&mut transaction).await?;
    let new_revision_id = next_id(&mut transaction).await?;

    let synced_links = revision.links.clone();

    // `OldEntry.syncLinks` runs before the entry is inserted.
    if let Some(Extension(ref log)) = request_log {
        crate::logging::event(&log, "copyEntryToWorkbook", "SYNC_LINKS", Map::new());
    }
    if let Some(links) = synced_links.as_ref() {
        sync_links(&mut transaction, new_entry_id, links).await?;
    }

    let name = args
        .name
        .clone()
        .or_else(|| source.name.clone())
        .unwrap_or_else(|| "null".to_owned());
    let display_key = format!("{new_entry_id}/{name}");
    let key = display_key.to_lowercase();

    sqlx::query(
        "INSERT INTO entries (entry_id, saved_id, published_id, key, display_key, tenant_id, scope, \
         type, inner_meta, unversioned_data, hidden, created_by, updated_by, workbook_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7::scope, $8, NULL, $9, $10, $11, $11, $12)",
    )
    .bind(new_entry_id)
    .bind(new_revision_id)
    .bind(source.published_id.map(|_| new_revision_id))
    .bind(&key)
    .bind(&display_key)
    .bind(&tenant_id)
    .bind(&source.scope)
    .bind(&source.entry_type)
    .bind(js_null(source.unversioned_data))
    .bind(source.hidden)
    .bind(CREATED_BY)
    .bind(target_workbook_id)
    .execute(&mut *transaction)
    .await?;

    if let Some(Extension(ref log)) = request_log {
        let mut fields = Map::new();
        fields.insert(
            "newEntryId".to_string(),
            Value::String(encode_id(new_entry_id)),
        );
        crate::logging::event(&log, "copyEntryToWorkbook", "CREATE_NEW_ENTRY", fields);
    }

    sqlx::query(
        "INSERT INTO revisions (rev_id, entry_id, meta, data, annotation, links, created_by, \
         updated_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $7)",
    )
    .bind(new_revision_id)
    .bind(new_entry_id)
    .bind(js_null(revision.meta))
    .bind(js_null(revision.data))
    .bind(js_null(revision.annotation))
    .bind(js_null(synced_links))
    .bind(CREATED_BY)
    .execute(&mut *transaction)
    .await?;

    if let Some(Extension(ref log)) = request_log {
        let mut fields = Map::new();
        fields.insert(
            "newRevisionId".to_string(),
            Value::String(encode_id(new_revision_id)),
        );
        crate::logging::event(&log, "copyEntryToWorkbook", "CREATE_NEW_REVISION", fields);
        crate::logging::event(
            &log,
            "copyEntryToWorkbook",
            "COPY_ENTRY_TO_WORKBOOK_FINISH",
            Map::new(),
        );
        let mut fields = Map::new();
        fields.insert("entryId".to_string(), Value::String(raw_entry_id.clone()));
        crate::logging::event(&log, "copyEntryToWorkbook", "COPY_ENTRY_SUCCESS", fields);
    }

    transaction.commit().await?;

    // `copy-entry-to-workbook-model.ts`: `format` reads the object returned by
    // `Entry.query().insert()`, which only carries the columns of the insert
    // payload — `createdAt`, `updatedAt` and `public` are `undefined` and are
    // therefore omitted from the JSON (no response validation is applied).
    Ok(Json(json!({
        "entryId": encode_id(new_entry_id),
        "scope": source.scope,
        "type": source.entry_type,
        "key": display_key,
        "createdBy": CREATED_BY,
        "updatedBy": CREATED_BY,
        "savedId": encode_id(new_revision_id),
        "publishedId": source.published_id.map(|_| encode_id(new_revision_id)),
        "tenantId": tenant_id,
        "workbookId": encode_id(target_workbook_id),
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// POST /v2/copy-entries
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct OriginEntry {
    entry_id: i64,
    scope: String,
    entry_type: String,
    display_key: Option<String>,
    hidden: bool,
    tenant_id: Option<String>,
    workbook_id: Option<i64>,
    published_id: Option<i64>,
    unversioned_data: Option<Value>,
    data: Option<Value>,
    meta: Option<Value>,
    links: Option<Value>,
    annotation: Option<Value>,
}

struct NewEntry {
    old_entry_id: i64,
    new_entry_id: i64,
    new_revision_id: i64,
    scope: String,
    entry_type: String,
    key: String,
    display_key: String,
    hidden: bool,
    published: bool,
    unversioned_data: Option<Value>,
    data: Option<Value>,
    meta: Option<Value>,
    annotation: Option<Value>,
    links: Option<Value>,
}

#[derive(Debug, FromRow)]
struct CopiedEntry {
    entry_id: i64,
    rev_id: i64,
    data: Option<Value>,
    meta: Option<Value>,
    links: Option<Value>,
}

/// `JoinedEntryRevision.find` on the replica: the entry joined with
/// `COALESCE(published_id, saved_id)`; note there is no tenant filter here.
async fn fetch_origin_entries(
    transaction: &mut Transaction<'_, Postgres>,
    entry_ids: &[i64],
) -> Result<Vec<OriginEntry>, ApiError> {
    let placeholders = (0..entry_ids.len())
        .map(|index| format!("${}", index + 1))
        .collect::<Vec<_>>()
        .join(", ");

    let sql = format!(
        "SELECT e.entry_id, e.scope::text AS scope, e.type AS entry_type, e.display_key, \
         e.hidden, e.tenant_id, e.workbook_id, e.published_id, e.unversioned_data, \
         r.rev_id, r.data, r.meta, r.links, r.annotation \
         FROM entries AS e \
         JOIN revisions AS r ON r.rev_id = COALESCE(e.published_id, e.saved_id) \
         WHERE e.is_deleted = FALSE AND e.entry_id IN ({placeholders})"
    );

    let mut query = sqlx::query_as::<_, OriginEntry>(&sql);

    for entry_id in entry_ids {
        query = query.bind(*entry_id);
    }

    Ok(query.fetch_all(&mut **transaction).await?)
}

/// `crossSyncCopiedJoinedEntryRevisions`: replace every old encoded id with the
/// new one, then re-sync the links and patch the copied revision.
///
/// The Node implementation round-trips the whole JSON through
/// `JSON.stringify`/`replace`/`JSON.parse`; walking the tree mutates only the
/// strings that actually contain an id, which avoids two full copies of a
/// (potentially large) dashboard payload.
fn replace_ids(value: Option<Value>, mapping: &[(String, String)]) -> Option<Value> {
    let mut value = value?;

    if !mapping.is_empty() {
        replace_ids_in_value(&mut value, mapping);
    }

    Some(value)
}

fn replace_ids_in_value(value: &mut Value, mapping: &[(String, String)]) {
    match value {
        Value::String(text) => {
            if !mapping.iter().any(|(old, _)| text.contains(old.as_str())) {
                return;
            }

            let mut replaced = std::mem::take(text);
            for (old, new) in mapping {
                if replaced.contains(old.as_str()) {
                    replaced = replaced.replace(old.as_str(), new.as_str());
                }
            }
            *text = replaced;
        }
        Value::Array(items) => {
            for item in items {
                replace_ids_in_value(item, mapping);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                replace_ids_in_value(item, mapping);
            }
        }
        _ => {}
    }
}

async fn copy_entries(
    State(state): State<AppState>,
    user: Option<Extension<AuthUser>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    if !permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user)) {
        return Err(ApiError::access_service_permission_denied().into());
    }
    let body = parse_body(&headers, &body)?;
    require_json_content_type(&headers)?;

    let (args, issues) = validate_copy_entries(&body);

    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues).into());
    }

    let args = args.expect("validated above");

    if args.empty_id {
        // `whereIn(entryId, [''])` → PostgreSQL `invalid input syntax for type
        // bigint` → `DBError` → presenter default (500, message only).
        return Err(ApiError::database(None).into());
    }

    let tenant_id = state.config.tenant_id_override.clone();
    let target_workbook_id = args.workbook_id;

    let mut transaction = state.db.primary.begin().await?;

    let origins = fetch_origin_entries(&mut transaction, &args.entry_ids).await?;

    if origins.is_empty() {
        return Err(entries_dont_exist().into());
    }

    // The validation loop of `copyToWorkbook` (the migrate branch is never used
    // by this endpoint).
    let mut origin_workbook_id: Option<i64> = None;

    for origin in &origins {
        if origin.tenant_id.as_deref() != Some(tenant_id.as_str()) {
            return Err(entry_doesnt_exist(encode_id(origin.entry_id)).into());
        }

        if origin.scope == "folder" {
            return Err(folder_copy_denied().into());
        }

        let workbook_id = match origin.workbook_id {
            Some(workbook_id) => workbook_id,
            None => {
                return Err(entry_without_workbook_id(format!(
                    "Entry {} doesn't have a workbookId and cannot be copied to a workbook.",
                    encode_id(origin.entry_id)
                ))
                .into())
            }
        };

        match origin_workbook_id {
            None => origin_workbook_id = Some(workbook_id),
            Some(current) if current != workbook_id => {
                return Err(entries_with_different_workbook_ids(format!(
                    "Copying entries from different workbooks is denied – {} and {}",
                    encode_id(current),
                    encode_id(workbook_id)
                ))
                .into())
            }
            Some(_) => {}
        }

        if origin.scope == "connection"
            && FILE_CONNECTION_TYPES.contains(&origin.entry_type.as_str())
        {
            return Err(workbook_copy_file_connection_with(format!(
                "Entry {} is a file connection and cannot be copied to a workbook.",
                encode_id(origin.entry_id)
            ))
            .into());
        }
    }

    // `resolveNameCollisions: true` — target workbook entries ordered by
    // `sortName` ascending, exactly like the Node query.
    let existing: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT entry_id, display_key FROM entries \
         WHERE workbook_id = $1 AND tenant_id = $2 AND is_deleted = FALSE \
         ORDER BY sort_name ASC",
    )
    .bind(target_workbook_id)
    .bind(&tenant_id)
    .fetch_all(&mut *transaction)
    .await?;

    let adding: Vec<(i64, Option<String>)> = origins
        .iter()
        .map(|origin| (origin.entry_id, origin.display_key.clone()))
        .collect();

    let name_overrides = resolve_entries_name_collisions(&existing, &adding)?;

    let mut new_entries: Vec<NewEntry> = Vec::with_capacity(origins.len());

    // One `get_id()` per row is a round-trip per id (2 per copied entry). The
    // sequence is volatile, so `generate_series` yields distinct values and a
    // single statement allocates everything up front.
    let mut allocated_ids =
        sqlx::query_scalar::<_, i64>("SELECT get_id() FROM generate_series(1, $1)")
            .bind((origins.len() * 2) as i64)
            .fetch_all(&mut *transaction)
            .await?
            .into_iter();

    for origin in &origins {
        let new_entry_id = allocated_ids.next().ok_or_else(ApiError::internal)?;
        let new_revision_id = allocated_ids.next().ok_or_else(ApiError::internal)?;

        let name = match name_overrides.get(&origin.entry_id) {
            Some(name) => name.clone(),
            None => {
                let display_key = origin
                    .display_key
                    .as_deref()
                    .ok_or_else(ApiError::internal)?;
                name_by_key(display_key).ok_or_else(ApiError::internal)?
            }
        };

        let display_key = format!("{new_entry_id}/{name}");
        let key = display_key.to_lowercase();

        new_entries.push(NewEntry {
            old_entry_id: origin.entry_id,
            new_entry_id,
            new_revision_id,
            scope: origin.scope.clone(),
            entry_type: origin.entry_type.clone(),
            key,
            display_key,
            hidden: origin.hidden,
            published: origin.published_id.is_some(),
            unversioned_data: origin.unversioned_data.clone(),
            data: origin.data.clone(),
            meta: origin.meta.clone(),
            annotation: origin.annotation.clone(),
            links: origin.links.clone(),
        });
    }

    // Ids and the old→new id map are known before inserting, so the (possibly
    // large) JSON payloads below can be *moved* into the INSERT arrays instead
    // of being cloned, and the later revision patch can use the map directly.
    let new_ids: Vec<i64> = new_entries.iter().map(|entry| entry.new_entry_id).collect();
    let mut mapping: Vec<(String, String)> = new_entries
        .iter()
        .map(|entry| (encode_id(entry.old_entry_id), encode_id(entry.new_entry_id)))
        .collect();

    // Workbook checks (`skipWorkbookPermissionsCheck` is false for this route).
    let destination: Option<i64> = sqlx::query_scalar(
        "SELECT workbook_id FROM workbooks WHERE workbook_id = $1 AND deleted_at IS NULL LIMIT 1",
    )
    .bind(target_workbook_id)
    .fetch_optional(&mut *transaction)
    .await?;

    if destination.is_none() {
        return Err(ApiError::workbook_not_exists().into());
    }

    if let Some(origin_workbook_id) = origin_workbook_id {
        let origin_workbook: Option<String> = sqlx::query_scalar(
            "SELECT tenant_id FROM workbooks WHERE workbook_id = $1 AND deleted_at IS NULL LIMIT 1",
        )
        .bind(origin_workbook_id)
        .fetch_optional(&mut *transaction)
        .await?;

        match origin_workbook {
            None => return Err(ApiError::workbook_not_exists().into()),
            Some(origin_tenant) if origin_tenant != tenant_id => {
                return Err(ApiError::workbook_not_exists().into())
            }
            Some(_) => {}
        }
    }

    // `Entry.query(trx).insertGraph(newEntries)` — entries and their revisions.
    // Both tables are inserted with one multi-row statement each (`UNNEST`
    // arrays), so a copy of N entries costs a constant number of round-trips.
    if !new_entries.is_empty() {
        let mut entry_ids: Vec<i64> = Vec::with_capacity(new_entries.len());
        let mut scopes: Vec<String> = Vec::with_capacity(new_entries.len());
        let mut types: Vec<String> = Vec::with_capacity(new_entries.len());
        let mut keys: Vec<String> = Vec::with_capacity(new_entries.len());
        let mut hidden: Vec<bool> = Vec::with_capacity(new_entries.len());
        let mut display_keys: Vec<String> = Vec::with_capacity(new_entries.len());
        let mut saved_ids: Vec<i64> = Vec::with_capacity(new_entries.len());
        let mut published_ids: Vec<Option<i64>> = Vec::with_capacity(new_entries.len());
        let mut unversioned: Vec<Option<Value>> = Vec::with_capacity(new_entries.len());
        let mut rev_ids: Vec<i64> = Vec::with_capacity(new_entries.len());
        let mut rev_entry_ids: Vec<i64> = Vec::with_capacity(new_entries.len());
        let mut datas: Vec<Option<Value>> = Vec::with_capacity(new_entries.len());
        let mut metas: Vec<Option<Value>> = Vec::with_capacity(new_entries.len());
        let mut annotations: Vec<Option<Value>> = Vec::with_capacity(new_entries.len());
        let mut links: Vec<Option<Value>> = Vec::with_capacity(new_entries.len());

        // `drain(..)` hands the owned values over: no clones of `data`/`meta`.
        for entry in new_entries.drain(..) {
            entry_ids.push(entry.new_entry_id);
            scopes.push(entry.scope);
            types.push(entry.entry_type);
            keys.push(entry.key);
            hidden.push(entry.hidden);
            display_keys.push(entry.display_key);
            saved_ids.push(entry.new_revision_id);
            published_ids.push(entry.published.then_some(entry.new_revision_id));
            unversioned.push(js_null(entry.unversioned_data));

            rev_ids.push(entry.new_revision_id);
            rev_entry_ids.push(entry.new_entry_id);
            datas.push(js_null(entry.data));
            metas.push(js_null(entry.meta));
            annotations.push(js_null(entry.annotation));
            links.push(js_null(entry.links));
        }

        sqlx::query(
            "INSERT INTO entries (entry_id, scope, type, key, inner_meta, created_by, updated_by, \
             deleted_at, hidden, display_key, saved_id, published_id, tenant_id, unversioned_data, \
             workbook_id) \
             SELECT u.entry_id, u.scope::scope, u.entry_type, u.key, NULL, $1, $1, NULL, \
                    u.hidden, u.display_key, u.saved_id, u.published_id, $2, u.unversioned_data, $3 \
             FROM UNNEST($4::bigint[], $5::text[], $6::text[], $7::text[], $8::boolean[], \
                         $9::text[], $10::bigint[], $11::bigint[], $12::jsonb[]) \
                  AS u(entry_id, scope, entry_type, key, hidden, display_key, saved_id, \
                       published_id, unversioned_data)",
        )
        .bind(CREATED_BY)
        .bind(&tenant_id)
        .bind(target_workbook_id)
        .bind(&entry_ids)
        .bind(&scopes)
        .bind(&types)
        .bind(&keys)
        .bind(&hidden)
        .bind(&display_keys)
        .bind(&saved_ids)
        .bind(&published_ids)
        .bind(&unversioned)
        .execute(&mut *transaction)
        .await?;

        sqlx::query(
            "INSERT INTO revisions (rev_id, entry_id, data, meta, annotation, created_by, \
             updated_by, links) \
             SELECT u.rev_id, u.entry_id, u.data, u.meta, u.annotation, $1, $1, u.links \
             FROM UNNEST($2::bigint[], $3::bigint[], $4::jsonb[], $5::jsonb[], $6::jsonb[], \
                         $7::jsonb[]) AS u(rev_id, entry_id, data, meta, annotation, links)",
        )
        .bind(CREATED_BY)
        .bind(&rev_ids)
        .bind(&rev_entry_ids)
        .bind(&datas)
        .bind(&metas)
        .bind(&annotations)
        .bind(&links)
        .execute(&mut *transaction)
        .await?;
    }

    let copied: Vec<CopiedEntry> = sqlx::query_as(
        "SELECT e.entry_id, r.rev_id, r.data, r.meta, r.links \
         FROM entries AS e \
         JOIN revisions AS r ON r.rev_id = COALESCE(e.published_id, e.saved_id) \
         WHERE e.tenant_id = $1 AND e.is_deleted = FALSE AND e.entry_id = ANY($2)",
    )
    .bind(&tenant_id)
    .bind(&new_ids)
    .fetch_all(&mut *transaction)
    .await?;

    let mut revision_updates: Vec<(i64, Option<Value>, Option<Value>, Option<Value>)> =
        Vec::with_capacity(copied.len());
    let mut links_to_sync: Vec<(i64, Value)> = Vec::with_capacity(copied.len());

    for entry in &copied {
        let data = js_null(replace_ids(entry.data.clone(), &mapping));
        let meta = js_null(replace_ids(entry.meta.clone(), &mapping));
        let links = js_null(replace_ids(entry.links.clone(), &mapping));

        if let Some(links) = links.clone() {
            links_to_sync.push((entry.entry_id, links));
        }

        revision_updates.push((entry.rev_id, data, meta, links));
    }

    // Links: one DELETE + one INSERT for the whole batch.
    sync_links_bulk(&mut transaction, &links_to_sync).await?;

    if !revision_updates.is_empty() {
        // `UPDATE ... FROM UNNEST(...)` patches every copied revision in a
        // single statement instead of one round-trip per entry.
        let rev_ids: Vec<i64> = revision_updates.iter().map(|row| row.0).collect();
        let datas: Vec<Option<Value>> = revision_updates.iter().map(|row| row.1.clone()).collect();
        let metas: Vec<Option<Value>> = revision_updates.iter().map(|row| row.2.clone()).collect();
        let links: Vec<Option<Value>> = revision_updates.iter().map(|row| row.3.clone()).collect();

        sqlx::query(
            "UPDATE revisions SET data = u.data, meta = u.meta, links = u.links \
             FROM UNNEST($1::bigint[], $2::jsonb[], $3::jsonb[], $4::jsonb[]) \
                  AS u(rev_id, data, meta, links) \
             WHERE revisions.rev_id = u.rev_id",
        )
        .bind(&rev_ids)
        .bind(&datas)
        .bind(&metas)
        .bind(&links)
        .execute(&mut *transaction)
        .await?;
    }

    transaction.commit().await?;

    // `workbookIdModel.format` — only the target workbook id.
    Ok(Json(json!({ "workbookId": encode_id(target_workbook_id) })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_by_key_matches_the_javascript_regex() {
        assert_eq!(name_by_key("123/Chart"), Some("Chart".to_owned()));
        assert_eq!(name_by_key("123/Folder/"), Some("Folder".to_owned()));
        assert_eq!(name_by_key("abc"), Some("abc".to_owned()));
        assert_eq!(name_by_key("123//Leaf"), Some("Leaf".to_owned()));
        // `([^/]+)\/?$` cannot match two trailing slashes nor an empty key.
        assert_eq!(name_by_key("123/Folder//"), None);
        assert_eq!(name_by_key("/"), None);
        assert_eq!(name_by_key(""), None);
    }

    #[test]
    fn copy_numbers_follow_utils() {
        assert_eq!(get_copy_number("Collide"), None);
        assert_eq!(get_copy_number("Collide (COPY 3)"), Some(3.0));
        // `Number('abc')` is NaN, i.e. treated as "no copy number".
        assert_eq!(get_copy_number("Collide (COPY abc)"), None);
        assert_eq!(name_without_copy_number("Collide (COPY 3)"), "Collide");
        assert_eq!(
            name_without_copy_number("Collide (copy 3)"),
            "Collide (copy 3)"
        );
        assert_eq!(set_copy_number("Collide", 0.0), "Collide");
        assert_eq!(set_copy_number("Collide", 5.0), "Collide (COPY 5)");
        assert_eq!(set_copy_number("Collide", 1.5), "Collide (COPY 1.5)");
    }

    #[test]
    fn name_collisions_use_max_and_increment() {
        let existing = vec![
            (1, Some("3/Collide".to_owned())),
            (2, Some("4/Collide (COPY 3)".to_owned())),
            (3, Some("5/collide (COPY 1)".to_owned())),
        ];
        let adding = vec![
            (10, Some("6/Collide".to_owned())),
            (11, Some("7/Collide (COPY 2)".to_owned())),
            (12, Some("8/brand new".to_owned())),
        ];

        let names = resolve_entries_name_collisions(&existing, &adding).expect("resolved");

        assert_eq!(names.get(&10).map(String::as_str), Some("Collide (COPY 5)"));
        assert_eq!(names.get(&11).map(String::as_str), Some("Collide (COPY 6)"));
        assert_eq!(names.get(&12).map(String::as_str), Some("brand new"));
    }

    #[test]
    fn copy_entry_validation_order_matches_zod() {
        let (_, issues) = validate_copy_entry(&json!({"workbookId": 123, "name": 5}));
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0]["path"], json!(["workbookId"]));
        assert_eq!(issues[0]["code"], json!("invalid_type"));
        assert_eq!(issues[1]["path"], json!(["name"]));

        let (_, issues) = validate_copy_entry(&json!({"name": "a/b"}));
        assert_eq!(issues.len(), 2);
        assert_eq!(
            issues[0]["message"],
            json!("String should not contain the symbol /")
        );
        assert_eq!(issues[1]["path"], json!(["name"]));

        let (_, issues) = validate_copy_entry(&json!([1, 2]));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["path"], json!([]));
        assert_eq!(issues[0]["expected"], json!("object"));
    }

    #[test]
    fn copy_entries_validation_reports_sync_issues_before_transform_issues() {
        let (_, issues) = validate_copy_entries(&json!({}));
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0]["path"], json!(["entryIds"]));
        assert_eq!(issues[1]["path"], json!(["workbookId"]));

        // `["bad"]` passes the array schema, so the per-item decode issue
        // (async transform) is appended after the synchronous `workbookId`
        // issue; array items always report the `custom` format error.
        let (_, issues) = validate_copy_entries(&json!({
            "entryIds": ["bad"],
            "workbookId": "short",
        }));
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0]["path"], json!(["workbookId"]));
        assert_eq!(issues[1]["code"], json!("custom"));
        assert_eq!(issues[1]["path"], json!(["entryIds"]));

        // An invalid item aborts the array schema (no per-item transform), but
        // the other fields still report their own issues, as in zod.
        let (_, issues) = validate_copy_entries(&json!({
            "entryIds": ["bad", 123],
            "workbookId": "aaaaaaaaaaaaa",
        }));
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0]["path"], json!(["entryIds", 1]));
        assert_eq!(issues[1]["path"], json!(["workbookId"]));

        let (_, issues) = validate_copy_entries(&json!({
            "entryIds": [],
            "workbookId": "aaaaaaaaaaaaa",
        }));
        assert_eq!(issues[0]["code"], json!("too_small"));

        let (args, issues) = validate_copy_entries(&json!({
            "entryIds": ["qvnkqzm0wstyf"],
            "workbookId": "qvnkqzm0wstyf",
        }));
        assert!(issues.is_empty());
        let args = args.expect("parsed");
        assert_eq!(args.workbook_id, decode_id("qvnkqzm0wstyf").unwrap());
        assert!(!args.empty_id);
    }

    #[test]
    fn empty_entry_id_is_flagged_for_the_database_error() {
        // An empty element is not a schema error: it reaches the database as an
        // empty id (`Utils.decodeId('') === ''`) and fails there, like Node.
        let (args, issues) = validate_copy_entries(&json!({
            "entryIds": [""],
            "workbookId": "qvnkqzm0wstyf",
        }));
        assert!(issues.is_empty());
        assert!(args.expect("parsed").empty_id);
    }

    #[test]
    fn js_number_matches_number_coercion() {
        assert_eq!(js_number("1"), Some(1.0));
        assert_eq!(js_number(" 2 "), Some(2.0));
        assert_eq!(js_number(""), Some(0.0));
        assert_eq!(js_number("abc"), None);
        assert_eq!(js_number("0x10"), Some(16.0));
    }
}
